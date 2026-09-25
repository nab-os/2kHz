//! The database, from the side that writes it: the schema, the block list,
//! and the helpers every stage shares. `schema.sql` at the repo root is the
//! contract; the app only ever reads the slim copy `catalog` builds from it.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use two_khz::api::BlockedArtist;

/// The schema, embedded at compile time.
const SCHEMA: &str = include_str!("../../schema.sql");

/// Create any missing tables and indexes, after migrating an older layout.
/// Cheap and idempotent.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    migrate(conn)?;
    conn.execute_batch(SCHEMA)
        .context("applying schema.sql")?;
    Ok(())
}

/// Bring an older database up to `schema.sql`.
///
/// The one migration so far: `features` as the Python pipeline wrote it, with
/// Essentia's descriptors and EffNet embeddings. None of it is comparable with
/// what the Rust analyser produces, and every row would be re-analysed anyway
/// for its extractor version, so the table is dropped and made again.
fn migrate(conn: &Connection) -> Result<()> {
    let python_features = conn
        .prepare("SELECT 1 FROM pragma_table_info('features') WHERE name = 'essentia_json'")?
        .exists([])?;
    if python_features {
        conn.execute_batch("DROP TABLE features")
            .context("dropping the Python-era features table")?;
    }
    Ok(())
}

/// Open the database for writing, with the schema applied.
pub fn open_for_write(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    // A stage may be writing while a request reads, or the other way round.
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// The fragment every stage filters tracks by, so the definition of "blocked"
/// cannot drift between the crawl, the analyser and the space. Expects the
/// tracks table aliased as `t`.
pub const NOT_BLOCKED: &str =
    "(t.artist_id IS NULL OR t.artist_id NOT IN (SELECT artist_id FROM blocked_artists))";

// ------------------------------------------------------------ writing back
//
// The block list. Hiding filters rather than deletes, so a block takes effect
// at once and can be lifted; see `purge` for the destructive version.

/// Hide an artist everywhere, and drop them from the frontier so an in-flight
/// crawl stops expanding them.
pub fn block_artist(
    db_path: &Path,
    artist_id: i64,
    name: &str,
    reason: Option<&str>,
) -> Result<()> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening {} for writing", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    conn.execute(
        "INSERT INTO blocked_artists (artist_id, name, reason, blocked_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(artist_id) DO UPDATE SET
             name   = COALESCE(excluded.name, blocked_artists.name),
             reason = COALESCE(excluded.reason, blocked_artists.reason)",
        rusqlite::params![artist_id, name, reason, utc_now()],
    )
    .with_context(|| format!("blocking artist {artist_id}"))?;

    conn.execute(
        "DELETE FROM frontier WHERE kind = 'artist' AND ref_id = ?1",
        rusqlite::params![artist_id.to_string()],
    )?;

    Ok(())
}

pub fn unblock_artist(db_path: &Path, artist_id: i64) -> Result<()> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute(
        "DELETE FROM blocked_artists WHERE artist_id = ?1",
        rusqlite::params![artist_id],
    )?;
    Ok(())
}

pub fn blocked_artists(db_path: &Path) -> Result<Vec<BlockedArtist>> {
    let conn = Connection::open(db_path)?;
    let mut statement = conn.prepare(
        "SELECT artist_id, name, reason FROM blocked_artists ORDER BY name COLLATE NOCASE",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(BlockedArtist {
            artist_id: row.get(0)?,
            name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            reason: row.get(2)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// An artist as the command line lists them: id, name, stored tracks.
#[derive(Debug, Clone)]
pub struct ArtistMatch {
    pub id: i64,
    pub name: String,
    pub tracks: i64,
}

/// Candidate artists for an id or a name fragment. Every match, not a guess:
/// Qobuz sometimes files one person under several ids, and all of them need
/// blocking.
pub fn resolve_artist(conn: &Connection, needle: &str) -> Result<Vec<ArtistMatch>> {
    let row = |row: &rusqlite::Row| {
        Ok(ArtistMatch {
            id: row.get(0)?,
            name: row.get(1)?,
            tracks: row.get(2)?,
        })
    };
    if let Ok(id) = needle.parse::<i64>() {
        let found: Vec<ArtistMatch> = conn
            .prepare(
                "SELECT a.id, a.name, (SELECT COUNT(*) FROM tracks t WHERE t.artist_id = a.id)
                 FROM artists a WHERE a.id = ?1",
            )?
            .query_map([id], row)?
            .collect::<rusqlite::Result<_>>()?;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(conn
        .prepare(
            "SELECT a.id, a.name, (SELECT COUNT(*) FROM tracks t WHERE t.artist_id = a.id) AS n
             FROM artists a
             WHERE a.name LIKE ?1 COLLATE NOCASE
             ORDER BY n DESC, a.name
             LIMIT 25",
        )?
        .query_map([format!("%{needle}%")], row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// What `purge` removed.
#[derive(Debug, Default)]
pub struct Purged {
    pub tracks: usize,
    pub features: usize,
    pub albums: usize,
}

/// Delete a blocked artist's stored data outright, rather than hiding it. The
/// space keeps its own copy of every vector, so `build-space` and `layout`
/// still need re-running for the map to forget them.
pub fn purge(conn: &Connection, artist_id: i64) -> Result<Purged> {
    let owned = "SELECT id FROM tracks WHERE artist_id = ?1";
    let features = conn.execute(&format!("DELETE FROM features WHERE track_id IN ({owned})"), [artist_id])?;
    conn.execute(&format!("DELETE FROM layout WHERE track_id IN ({owned})"), [artist_id])?;
    conn.execute(&format!("DELETE FROM failures WHERE track_id IN ({owned})"), [artist_id])?;
    let tracks = conn.execute("DELETE FROM tracks WHERE artist_id = ?1", [artist_id])?;
    let albums = conn.execute("DELETE FROM albums WHERE artist_id = ?1", [artist_id])?;
    conn.execute(
        "DELETE FROM frontier WHERE kind = 'artist' AND ref_id = ?1",
        [artist_id.to_string()],
    )?;
    Ok(Purged {
        tracks,
        features,
        albums,
    })
}

pub fn utc_now() -> String {
    // UTC, seconds precision: sorts as text.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Cheaper than pulling in chrono
/// for the one timestamp this crate writes.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
