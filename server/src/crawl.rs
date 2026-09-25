//! Catalogue crawler.
//!
//! Seeds from favourites at seed_distance 0, then expands through
//! `artist/getSimilarArtists`. The frontier lives in SQLite, so the crawl is
//! interruptible.

use crate::qobuz::QobuzClient;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashSet;


#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub artists_expanded: usize,
    pub albums_expanded: usize,
    pub tracks_added: i64,
    pub errors: usize,
    pub blocked_skipped: usize,
}

fn now() -> String {
    crate::db::utc_now()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn as_i64(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn as_id_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// ------------------------------------------------------------------ upserts
//
// COALESCE keeps stored values when a sparser payload for the same entity
// arrives, a stub artist inside a track versus a full artist/get.

pub fn upsert_artist(conn: &Connection, artist: &Value) -> Result<Option<i64>> {
    let Some(id) = as_i64(artist, "id") else {
        return Ok(None);
    };
    conn.execute(
        "INSERT INTO artists (id, name, qobuz_json) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET
             name       = excluded.name,
             qobuz_json = COALESCE(excluded.qobuz_json, artists.qobuz_json)",
        params![
            id,
            text(artist, "name").unwrap_or_else(|| "Unknown Artist".into()),
            artist.to_string()
        ],
    )?;
    Ok(Some(id))
}

pub fn upsert_album(conn: &Connection, album: &Value) -> Result<Option<String>> {
    let Some(id) = as_id_string(album, "id") else {
        return Ok(None);
    };
    let artist_id = match album.get("artist") {
        Some(artist) => upsert_artist(conn, artist)?,
        None => None,
    };

    conn.execute(
        "INSERT INTO albums (id, artist_id, title, release_date, label, genre, qobuz_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             artist_id    = COALESCE(excluded.artist_id, albums.artist_id),
             title        = excluded.title,
             release_date = COALESCE(excluded.release_date, albums.release_date),
             label        = COALESCE(excluded.label, albums.label),
             genre        = COALESCE(excluded.genre, albums.genre),
             qobuz_json   = COALESCE(excluded.qobuz_json, albums.qobuz_json)",
        params![
            id,
            artist_id,
            text(album, "title").unwrap_or_else(|| "Unknown Album".into()),
            text(album, "release_date_original").or_else(|| text(album, "released_at")),
            album.get("label").and_then(|l| l.get("name")).and_then(|v| v.as_str()),
            album.get("genre").and_then(|g| g.get("name")).and_then(|v| v.as_str()),
            album.to_string()
        ],
    )?;
    Ok(Some(id))
}

pub fn upsert_track(
    conn: &Connection,
    track: &Value,
    album_id: Option<&str>,
    artist_id: Option<i64>,
    seed_distance: i64,
) -> Result<Option<i64>> {
    let Some(id) = as_i64(track, "id") else {
        return Ok(None);
    };

    let album_id = match (album_id, track.get("album")) {
        (Some(given), _) => Some(given.to_string()),
        (None, Some(album)) => upsert_album(conn, album)?,
        (None, None) => None,
    };

    let artist_id = match artist_id {
        Some(given) => Some(given),
        None => {
            let performer = track.get("performer").or_else(|| track.get("artist"));
            match performer {
                Some(value) => upsert_artist(conn, value)?,
                None => None,
            }
        }
    };

    conn.execute(
        "INSERT INTO tracks (id, album_id, artist_id, title, duration, isrc,
                             qobuz_json, seed_distance)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             album_id      = COALESCE(excluded.album_id, tracks.album_id),
             artist_id     = COALESCE(excluded.artist_id, tracks.artist_id),
             title         = excluded.title,
             duration      = COALESCE(excluded.duration, tracks.duration),
             isrc          = COALESCE(excluded.isrc, tracks.isrc),
             qobuz_json    = COALESCE(excluded.qobuz_json, tracks.qobuz_json),
             -- Keep the shortest known distance to a favourite.
             seed_distance = MIN(tracks.seed_distance, excluded.seed_distance)",
        params![
            id,
            album_id,
            artist_id,
            text(track, "title").unwrap_or_else(|| "Unknown Track".into()),
            as_i64(track, "duration"),
            text(track, "isrc"),
            track.to_string(),
            seed_distance
        ],
    )?;
    Ok(Some(id))
}

// ----------------------------------------------------------------- frontier

/// Add to the frontier, keeping the lowest priority (closest to a seed).
pub fn enqueue(conn: &Connection, kind: &str, ref_id: &str, priority: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO frontier (kind, ref_id, priority, state) VALUES (?1, ?2, ?3, 'pending')
         ON CONFLICT(kind, ref_id) DO UPDATE SET
             priority = MIN(frontier.priority, excluded.priority)",
        params![kind, ref_id, priority],
    )?;
    Ok(())
}

fn mark(conn: &Connection, kind: &str, ref_id: &str, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE frontier SET state = ?1 WHERE kind = ?2 AND ref_id = ?3",
        params![state, kind, ref_id],
    )?;
    Ok(())
}

struct Item {
    kind: String,
    ref_id: String,
    priority: i64,
}

fn next_batch(conn: &Connection, limit: usize) -> Result<Vec<Item>> {
    let mut statement = conn.prepare(
        "SELECT kind, ref_id, priority FROM frontier
         WHERE state = 'pending'
         ORDER BY priority ASC, kind DESC
         LIMIT ?1",
    )?;
    let rows = statement.query_map(params![limit as i64], |row| {
        Ok(Item {
            kind: row.get(0)?,
            ref_id: row.get(1)?,
            priority: row.get(2)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn track_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?)
}

fn blocked_ids(conn: &Connection) -> Result<HashSet<i64>> {
    let mut statement = conn.prepare("SELECT artist_id FROM blocked_artists")?;
    let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Whether a frontier item belongs to a blocked artist.
fn is_blocked(conn: &Connection, kind: &str, ref_id: &str, blocked: &HashSet<i64>) -> bool {
    if blocked.is_empty() {
        return false;
    }
    match kind {
        "artist" => ref_id.parse::<i64>().is_ok_and(|id| blocked.contains(&id)),
        "album" => conn
            .query_row(
                "SELECT artist_id FROM albums WHERE id = ?1",
                params![ref_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
            .is_some_and(|id| blocked.contains(&id)),
        _ => false,
    }
}

// --------------------------------------------------------------------- seed

/// Load every favourite into the catalogue at seed_distance 0.
pub async fn seed(conn: &Connection, client: &mut QobuzClient, cap: usize) -> Result<Stats> {
    let mut stats = Stats::default();

    for track in client.favorites_raw("tracks", cap).await? {
        if upsert_track(conn, &track, None, None, 0)?.is_some() {
            stats.tracks_added += 1;
            if let Some(id) = track.get("performer").and_then(|p| as_i64(p, "id")) {
                enqueue(conn, "artist", &id.to_string(), 0)?;
            }
        }
    }

    for album in client.favorites_raw("albums", cap).await? {
        if let Some(album_id) = upsert_album(conn, &album)? {
            stats.albums_expanded += 1;
            enqueue(conn, "album", &album_id, 0)?;
            if let Some(id) = album.get("artist").and_then(|a| as_i64(a, "id")) {
                enqueue(conn, "artist", &id.to_string(), 0)?;
            }
        }
    }

    for artist in client.favorites_raw("artists", cap).await? {
        if let Some(id) = upsert_artist(conn, &artist)? {
            stats.artists_expanded += 1;
            enqueue(conn, "artist", &id.to_string(), 0)?;
        }
    }

    Ok(stats)
}

// -------------------------------------------------------------------- crawl

/// Pull an artist's albums, and enqueue their similar artists one hop further out.
pub async fn expand_artist(
    conn: &Connection,
    client: &mut QobuzClient,
    artist_id: i64,
    distance: i64,
    max_distance: i64,
) -> Result<()> {
    for album in client.artist_albums_raw(artist_id, 1000).await? {
        if let Some(album_id) = upsert_album(conn, &album)? {
            enqueue(conn, "album", &album_id, distance)?;
        }
    }

    conn.execute(
        "UPDATE artists SET similar_fetched_at = ?1 WHERE id = ?2",
        params![now(), artist_id],
    )?;

    if distance < max_distance {
        let blocked = blocked_ids(conn)?;
        for similar in client.similar_artists_raw(artist_id, 50).await? {
            if let Some(id) = upsert_artist(conn, &similar)? {
                // A blocked artist is a dead end, not just a hidden one:
                // following them would pull their whole neighbourhood in.
                if !blocked.contains(&id) {
                    enqueue(conn, "artist", &id.to_string(), distance + 1)?;
                }
            }
        }
    }

    Ok(())
}

/// Pull an album's tracklist. Returns how many tracks were written.
pub async fn expand_album(
    conn: &Connection,
    client: &mut QobuzClient,
    album_id: &str,
    distance: i64,
) -> Result<usize> {
    let album = client.album_raw(album_id).await?;
    upsert_album(conn, &album)?;
    let artist_id = album.get("artist").and_then(|a| as_i64(a, "id"));

    let mut written = 0;
    let tracks = album
        .get("tracks")
        .and_then(|block| block.get("items"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for track in &tracks {
        if upsert_track(conn, track, Some(album_id), artist_id, distance)?.is_some() {
            written += 1;
        }
    }
    Ok(written)
}

/// The outcome of one unit of crawl work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Expanded { kind: String, ref_id: String },
    Skipped { kind: String, ref_id: String },
    Failed { kind: String, ref_id: String, error: String },
    /// The track budget is reached; nothing was done.
    BudgetReached,
    /// The frontier is empty; nothing was done.
    Exhausted,
}

/// Take one item off the frontier and expand it.
///
/// One entry rather than the whole crawl, because the app runs this while you
/// browse: the caller takes the client for one step and gives it back.
pub async fn step(
    conn: &Connection,
    client: &mut QobuzClient,
    max_tracks: i64,
    max_distance: i64,
) -> Result<StepResult> {
    if track_count(conn)? >= max_tracks {
        return Ok(StepResult::BudgetReached);
    }

    let Some(item) = next_batch(conn, 1)?.into_iter().next() else {
        return Ok(StepResult::Exhausted);
    };
    let (kind, ref_id) = (item.kind.clone(), item.ref_id.clone());

    if item.priority > max_distance || is_blocked(conn, &kind, &ref_id, &blocked_ids(conn)?) {
        mark(conn, &kind, &ref_id, "skipped")?;
        return Ok(StepResult::Skipped { kind, ref_id });
    }

    // A dead id or a region-locked album must not stop the crawl.
    let outcome = match kind.as_str() {
        "artist" => match ref_id.parse::<i64>() {
            Ok(id) => expand_artist(conn, client, id, item.priority, max_distance)
                .await
                .map(|_| ()),
            Err(err) => Err(err).context("artist id"),
        },
        "album" => expand_album(conn, client, &ref_id, item.priority)
            .await
            .map(|_| ()),
        _ => Ok(()),
    };

    match outcome {
        Ok(()) => {
            mark(conn, &kind, &ref_id, "done")?;
            Ok(StepResult::Expanded { kind, ref_id })
        }
        Err(err) => {
            mark(conn, &kind, &ref_id, "failed")?;
            Ok(StepResult::Failed {
                kind,
                ref_id,
                error: format!("{err:#}"),
            })
        }
    }
}

impl Stats {
    /// Fold one step's outcome in. Returns false once the crawl should stop.
    pub fn absorb(&mut self, result: &StepResult) -> bool {
        match result {
            StepResult::Expanded { kind, .. } => {
                if kind == "artist" {
                    self.artists_expanded += 1;
                } else {
                    self.albums_expanded += 1;
                }
                true
            }
            StepResult::Skipped { .. } => {
                self.blocked_skipped += 1;
                true
            }
            StepResult::Failed { .. } => {
                self.errors += 1;
                true
            }
            StepResult::BudgetReached | StepResult::Exhausted => false,
        }
    }
}

/// What to report while a crawl runs. The CLI prints; the app updates a signal.
pub type Progress<'a> = &'a (dyn Fn(&Stats, i64) + Send + Sync);

/// Work the frontier until the track budget or the distance limit is reached.
pub async fn crawl(
    conn: &Connection,
    client: &mut QobuzClient,
    max_tracks: i64,
    max_distance: i64,
    progress: Option<Progress<'_>>,
) -> Result<Stats> {
    client.login().await?;
    let mut stats = Stats::default();
    let start_count = track_count(conn)?;
    let mut since_report = 0;

    loop {
        let result = step(conn, client, max_tracks, max_distance).await?;
        if let StepResult::Failed { kind, ref_id, error } = &result {
            eprintln!("  ! {kind} {ref_id}: {error}");
        }
        if !stats.absorb(&result) {
            break;
        }

        since_report += 1;
        if since_report >= 32 {
            since_report = 0;
            if let Some(report) = progress {
                report(&stats, track_count(conn)?);
            }
        }
    }

    stats.tracks_added = track_count(conn)? - start_count;
    Ok(stats)
}

/// Pull an artist's discography into the catalogue and queue their albums.
///
/// Deliberately does not fetch every tracklist: a prolific artist is hundreds
/// of albums, which at 2/s is minutes of a frozen button. Returns how many
/// albums were queued.
pub async fn discover_artist(
    conn: &Connection,
    client: &mut QobuzClient,
    artist_id: i64,
) -> Result<usize> {
    client.login().await?;

    let albums = client.artist_albums_raw(artist_id, 1000).await?;
    let mut queued = 0;
    for album in &albums {
        if let Some(album_id) = upsert_album(conn, album)? {
            enqueue(conn, "album", &album_id, 0)?;
            queued += 1;
        }
    }

    // Queue the artist too, so a later crawl still takes the similar-artist
    // hop from here.
    enqueue(conn, "artist", &artist_id.to_string(), 0)?;
    Ok(queued)
}

/// Crawl a single album's tracklist.
pub async fn crawl_one_album(
    conn: &Connection,
    client: &mut QobuzClient,
    album_id: &str,
) -> Result<usize> {
    client.login().await?;
    expand_album(conn, client, album_id, 0).await
}
