//! Track metadata, read from the slim catalogue the server hands out. The
//! schema lives in `schema.sql` at the repo root; only the server writes.

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

/// Artists the user has blocked. The server honours the list everywhere; the
/// client filters what it has already loaded.
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
                    f.descriptors_json, l.x, l.y
             FROM tracks t
             LEFT JOIN artists  ar ON ar.id = t.artist_id
             LEFT JOIN albums   al ON al.id = t.album_id
             LEFT JOIN features f  ON f.track_id = t.id
             LEFT JOIN layout   l  ON l.track_id = t.id",
        )?;

        let mut by_id: HashMap<i64, TrackMeta> = HashMap::new();
        let rows = statement.query_map([], |row| {
            let descriptors: Option<String> = row.get(8)?;
            // BPM lives inside the descriptor blob; pull just that one field.
            let bpm = descriptors.as_deref().and_then(|json| {
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

/// Just the ids, for refreshing a loaded catalog after a block changes.
pub fn blocked_artist_ids(db_path: &Path) -> Result<HashSet<i64>> {
    let conn = Connection::open(db_path)?;
    load_blocked(&conn)
}
