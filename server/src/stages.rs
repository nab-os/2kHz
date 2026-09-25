//! Running a pipeline stage, and the counts that say which one is due.
//!
//! Stages run in this process. The crawl has its own loop in `hub`, since it
//! runs until stopped rather than until done; the other three come through
//! here, from the pipeline view and the command line alike.

use crate::pipeline::{analyse, assemble, layout, Job, Paths};
use crate::qobuz::RateLimit;
use anyhow::{bail, Result};
use std::path::Path;
use two_khz::api::{Corpus, Stage};

/// Run one terminating stage to completion. `!Send`, analyse holds a
/// database connection across awaits, so drive it on a thread of its own.
pub async fn run(stage: Stage, paths: &Paths, limiter: RateLimit, job: &Job) -> Result<()> {
    match stage {
        Stage::Crawl => bail!("the crawl runs from its own loop, not as a stage"),
        Stage::Analyse => {
            analyse::run(paths, limiter, &analyse::Options::default(), job).await?;
        }
        Stage::BuildSpace => {
            assemble::run(paths, None, job).await?;
        }
        Stage::Layout => {
            layout::run(paths, &layout::Options::default(), job)?;
        }
    }
    Ok(())
}

/// Re-read the counts that tell you what still needs running. `in_space` and
/// `on_map` stay zero: they are questions about what a client has loaded. See
/// `api::Corpus`.
pub fn corpus(db_path: &Path) -> Result<Corpus> {
    let conn = crate::db::open_for_write(db_path)?;
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };

    // Asked the way `build-space` asks it.
    let buildable = count(&format!(
        "SELECT COUNT(*) FROM tracks t
         JOIN features f ON f.track_id = t.id
         WHERE f.clap_f32 IS NOT NULL AND {}",
        crate::db::NOT_BLOCKED
    ));

    Ok(Corpus {
        tracks: count("SELECT COUNT(*) FROM tracks"),
        analysed: count("SELECT COUNT(*) FROM features"),
        to_analyse: analyse::pending_count(&conn).unwrap_or(0),
        failed: count("SELECT COUNT(*) FROM failures"),
        pending: count("SELECT COUNT(*) FROM frontier WHERE state = 'pending'"),
        buildable,
        in_space: 0,
        on_map: 0,
    })
}
