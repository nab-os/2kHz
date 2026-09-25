//! The slim catalogue clients sync.
//!
//! `two_khz.db` is 269MB, almost none of it read at query time. This projects
//! out what navigation needs: the metadata minus every `qobuz_json` blob,
//! `layout`, `blocked_artists`, and `features` reduced to the CLAP embedding
//! plus a one-field `descriptors_json` carrying only the BPM.
//!
//! That last stub is what `Catalog::load` reads BPM from, by pulling `$.bpm`.
//! About 35MB for a 15k corpus, against 269MB.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

pub fn build(source: &Path, target: &Path) -> Result<u64> {
    if !source.exists() {
        anyhow::bail!("{} does not exist", source.display());
    }

    // The source may be a database nothing has crawled into yet; create the
    // schema rather than failing with "no such table: artists". This is also
    // what migrates an older one.
    crate::db::ensure_schema(
        &Connection::open(source)
            .with_context(|| format!("opening {}", source.display()))?,
    )?;

    // Built beside the target and renamed, so a client syncing mid-rebuild
    // cannot download a half-written catalogue.
    let staging = target.with_extension("partial");
    let _ = std::fs::remove_file(&staging);

    let conn = Connection::open(&staging)
        .with_context(|| format!("creating {}", staging.display()))?;

    // Before any table exists: a page size can only be chosen for an empty
    // database. Worth twice the file size, at the default 4096 bytes exactly
    // one 2048-byte CLAP blob fits per page, wasting half of every one. At
    // 16KB seven fit, and 63MB becomes 35MB.
    conn.pragma_update(None, "page_size", 16384)?;

    // The shared schema, so the slim copy is still a two_khz database and
    // `Catalog::load` needs no special case for it.
    crate::db::ensure_schema(&conn)?;

    conn.execute("ATTACH DATABASE ?1 AS src", [source.to_string_lossy()])
        .with_context(|| format!("attaching {}", source.display()))?;

    // Tracks before features and layout: those two carry the only declared
    // foreign keys in the schema.
    conn.execute_batch(
        "BEGIN;

         INSERT INTO artists (id, name)
             SELECT id, name FROM src.artists;

         INSERT INTO albums (id, artist_id, title, release_date, label, genre)
             SELECT id, artist_id, title, release_date, label, genre FROM src.albums;

         INSERT INTO tracks (id, album_id, artist_id, title, duration, isrc, seed_distance)
             SELECT id, album_id, artist_id, title, duration, isrc, seed_distance
             FROM src.tracks;

         INSERT INTO layout (track_id, x, y)
             SELECT track_id, x, y FROM src.layout;

         INSERT INTO blocked_artists (artist_id, name, reason, blocked_at)
             SELECT artist_id, name, reason, blocked_at FROM src.blocked_artists;

         -- descriptors_json is reduced to the one field anything reads.
         INSERT INTO features (track_id, extractor_version, descriptors_json, clap_f32, analysed_at)
             SELECT track_id,
                    extractor_version,
                    CASE WHEN descriptors_json IS NULL THEN NULL
                         ELSE json_object('bpm', json_extract(descriptors_json, '$.bpm'))
                    END,
                    clap_f32,
                    analysed_at
             FROM src.features;

         COMMIT;",
    )
    .context("projecting the catalogue")?;

    conn.execute("DETACH DATABASE src", [])?;

    // Leave a single self-contained file: sync ships `catalog.db` and nothing
    // beside it, so anything still sitting in a WAL would be lost.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.execute_batch("VACUUM")?;
    drop(conn);

    std::fs::rename(&staging, target)
        .with_context(|| format!("replacing {}", target.display()))?;

    Ok(std::fs::metadata(target)?.len())
}
