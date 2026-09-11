//! Track metadata, read straight from the SQLite database both halves share.
//! The schema lives in `schema.sql` at the repo root, see `ensure_schema`.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TrackMeta {
    pub track_id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub artist_id: i64,
    pub album_id: String,
    pub bpm: Option<f32>,
    pub seed_distance: i32,
    /// UMAP coordinates for the map, if the layout step has been run.
    pub x: Option<f32>,
    pub y: Option<f32>,
}

pub struct Catalog {
    pub tracks: Vec<TrackMeta>,
    /// Raw CLAP audio embeddings, row-aligned with `tracks`. Raw rather than
    /// the PCA'd semantic block; see paths::text_target.
    pub clap: Option<Vec<f32>>,
    pub clap_dims: usize,
    /// Artists to pretend are not there. The space keeps their rows, so
    /// blocking is enforced on every read, which is what lets it take effect
    /// without a rebuild.
    pub blocked_artists: HashSet<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockedArtist {
    pub artist_id: i64,
    pub name: String,
    pub reason: Option<String>,
}

/// The shared schema, embedded at compile time. Both halves create these
/// tables, so the crawler cannot assume a Python command ran first.
const SCHEMA: &str = include_str!("../../schema.sql");

/// Create any missing tables and indexes. Cheap and idempotent, every
/// statement in the schema is `IF NOT EXISTS`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)
        .context("applying schema.sql")?;
    Ok(())
}

/// Open the database for writing, with the schema applied.
pub fn open_for_write(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    // The pipeline may be reading or writing the same file.
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Artists the user has blocked. Written by either half; see
/// `pipeline/qsuggest/blocklist.py` for what the pipeline does with it.
fn load_blocked(conn: &Connection) -> Result<HashSet<i64>> {
    let mut statement = conn.prepare("SELECT artist_id FROM blocked_artists")?;
    let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Load CLAP embeddings for the given track order.
fn load_clap(conn: &Connection, track_ids: &[i64]) -> Result<(Option<Vec<f32>>, usize)> {
    let mut statement =
        conn.prepare("SELECT track_id, clap_f32 FROM features WHERE clap_f32 IS NOT NULL")?;
    let mut stored: HashMap<i64, Vec<f32>> = HashMap::new();

    let rows = statement.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        Ok((id, blob))
    })?;
    for row in rows {
        let (id, blob) = row?;
        if blob.len() % 4 == 0 {
            stored.insert(id, bytemuck::cast_slice::<u8, f32>(&blob).to_vec());
        }
    }

    let Some(dims) = stored.values().map(|v| v.len()).next() else {
        return Ok((None, 0));
    };

    let mut matrix = vec![0.0f32; track_ids.len() * dims];
    for (i, id) in track_ids.iter().enumerate() {
        if let Some(vector) = stored.get(id) {
            if vector.len() == dims {
                matrix[i * dims..(i + 1) * dims].copy_from_slice(vector);
            }
        }
    }
    Ok((Some(matrix), dims))
}

impl Catalog {
    /// Load metadata for the given track ids, in that exact order, so indices
    /// line up with the rows of space.bin.
    pub fn load(db_path: &Path, track_ids: &[i64]) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        let mut statement = conn.prepare(
            "SELECT t.id, t.title, t.artist_id, t.album_id, t.seed_distance,
                    ar.name, al.title, al.genre,
                    f.essentia_json, l.x, l.y
             FROM tracks t
             LEFT JOIN artists  ar ON ar.id = t.artist_id
             LEFT JOIN albums   al ON al.id = t.album_id
             LEFT JOIN features f  ON f.track_id = t.id
             LEFT JOIN layout   l  ON l.track_id = t.id",
        )?;

        let mut by_id: HashMap<i64, TrackMeta> = HashMap::new();
        let rows = statement.query_map([], |row| {
            let essentia: Option<String> = row.get(8)?;
            // BPM lives inside the descriptor blob; pull just that one field.
            let bpm = essentia.as_deref().and_then(|json| {
                serde_json::from_str::<serde_json::Value>(json)
                    .ok()
                    .and_then(|v| v.get("bpm").and_then(|b| b.as_f64()))
                    .map(|b| b as f32)
            });

            Ok(TrackMeta {
                track_id: row.get(0)?,
                title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                artist_id: row.get::<_, Option<i64>>(2)?.unwrap_or(-1),
                album_id: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                seed_distance: row.get::<_, Option<i32>>(4)?.unwrap_or(0),
                artist: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                album: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                genre: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                bpm,
                x: row.get::<_, Option<f64>>(9)?.map(|v| v as f32),
                y: row.get::<_, Option<f64>>(10)?.map(|v| v as f32),
            })
        })?;

        for row in rows {
            let meta = row?;
            by_id.insert(meta.track_id, meta);
        }

        let tracks = track_ids
            .iter()
            .map(|id| {
                by_id.remove(id).unwrap_or(TrackMeta {
                    track_id: *id,
                    title: format!("<missing {id}>"),
                    ..Default::default()
                })
            })
            .collect();

        let (clap, clap_dims) = load_clap(&conn, track_ids)?;
        let blocked_artists = load_blocked(&conn)?;

        Ok(Self {
            tracks,
            clap,
            clap_dims,
            blocked_artists,
        })
    }

    /// Whether row `i` belongs to a blocked artist.
    pub fn is_blocked(&self, i: usize) -> bool {
        self.tracks
            .get(i)
            .is_some_and(|t| self.blocked_artists.contains(&t.artist_id))
    }

    pub fn is_blocked_id(&self, track_id: i64) -> bool {
        self.tracks
            .iter()
            .find(|t| t.track_id == track_id)
            .is_some_and(|t| self.blocked_artists.contains(&t.artist_id))
    }

    /// Row indices that are visible, in catalog order.
    pub fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.tracks.len()).filter(|&i| !self.is_blocked(i))
    }

    pub fn get(&self, i: usize) -> &TrackMeta {
        &self.tracks[i]
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Ids present in the space, for checking whether a Qobuz result is
    /// navigable. Blocked tracks are left out.
    pub fn id_set(&self) -> HashSet<i64> {
        self.visible().map(|i| self.tracks[i].track_id).collect()
    }
}

// ------------------------------------------------------------ writing back
//
// Mostly this module reads. It writes two things: the block list below, and,
// via `crate::crawl`, catalogue rows and frontier entries.

/// Hide an artist everywhere. Mirrors the Python side, including dropping them
/// from the frontier so an in-flight crawl stops expanding them.
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

/// Just the ids, for refreshing a loaded catalog after a block changes.
pub fn blocked_artist_ids(db_path: &Path) -> Result<HashSet<i64>> {
    let conn = Connection::open(db_path)?;
    load_blocked(&conn)
}

pub fn utc_now() -> String {
    // Same shape as the Python side writes: UTC, seconds precision.
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

/// How much work the crawler has waiting, for the status line.
pub fn pending_count(db_path: &Path) -> Result<i64> {
    let conn = Connection::open(db_path)?;
    let count = conn.query_row(
        "SELECT COUNT(*) FROM frontier WHERE state = 'pending'",
        [],
        |row| row.get(0),
    )?;
    Ok(count)
}
