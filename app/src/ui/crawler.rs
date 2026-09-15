//! A crawl running in the background while you browse.
//!
//! Stepped one frontier item at a time, with the Qobuz client released around
//! each step, so search and playback never wait longer than one item.
//!
//! The controls live in the pipeline view; this is only the machinery.

use super::{client, db_path};
use dioxus::prelude::*;
use qsuggest::db;

/// A crawl running in the background while you browse. Stepped one frontier
/// item at a time, with the Qobuz client released around each step, so search
/// and playback never wait longer than one item.
#[derive(Clone, Copy)]
pub struct Crawler {
    pub running: Signal<bool>,
    pub stats: Signal<qsuggest::crawl::Stats>,
    pub tracks: Signal<i64>,
    pub pending: Signal<i64>,
    pub last: Signal<Option<String>>,
    /// Set to ask the loop to stop after the current step.
    stop: Signal<bool>,
}

impl Crawler {
    pub fn new() -> Self {
        Self {
            running: Signal::new(false),
            stats: Signal::new(Default::default()),
            tracks: Signal::new(0),
            pending: Signal::new(0),
            last: Signal::new(None),
            stop: Signal::new(false),
        }
    }

    pub fn request_stop(mut self) {
        self.stop.set(true);
    }

    /// Run the frontier until it empties, the budget is hit, or stop is asked.
    pub fn start(mut self, max_distance: i64) {
        if *self.running.peek() {
            return;
        }
        self.stop.set(false);
        self.running.set(true);
        self.stats.set(Default::default());
        self.last.set(Some("starting…".into()));

        spawn(async move {
            let mut crawler = self;
            let conn = match db::open_for_write(db_path()) {
                Ok(conn) => conn,
                Err(err) => {
                    crawler.last.set(Some(format!("{err:#}")));
                    crawler.running.set(false);
                    return;
                }
            };

            // No budget: the frontier and the stop button are the limits.
            let max_tracks = i64::MAX;

            loop {
                if *crawler.stop.peek() {
                    crawler.last.set(Some("stopped".into()));
                    break;
                }

                // Scoped so the client is free again before the next await.
                let outcome = {
                    let handle = match client() {
                        Ok(handle) => handle,
                        Err(err) => {
                            crawler.last.set(Some(format!("{err:#}")));
                            break;
                        }
                    };
                    let mut guard = handle.lock().await;
                    qsuggest::crawl::step(&conn, &mut guard, max_tracks, max_distance).await
                };

                let result = match outcome {
                    Ok(result) => result,
                    Err(err) => {
                        crawler.last.set(Some(format!("{err:#}")));
                        break;
                    }
                };

                crawler.last.set(Some(match &result {
                    qsuggest::crawl::StepResult::Expanded { kind, ref_id } => {
                        format!("{kind} {ref_id}")
                    }
                    qsuggest::crawl::StepResult::Skipped { kind, ref_id } => {
                        format!("skipped {kind} {ref_id}")
                    }
                    qsuggest::crawl::StepResult::Failed { kind, ref_id, error } => {
                        format!("{kind} {ref_id}: {error}")
                    }
                    qsuggest::crawl::StepResult::Exhausted => "frontier exhausted".into(),
                    qsuggest::crawl::StepResult::BudgetReached => "budget reached".into(),
                }));

                let mut stats = crawler.stats.peek().clone();
                let keep_going = stats.absorb(&result);
                crawler.stats.set(stats);

                crawler.tracks.set(
                    conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
                        .unwrap_or(0),
                );
                crawler.pending.set(
                    conn.query_row(
                        "SELECT COUNT(*) FROM frontier WHERE state = 'pending'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0),
                );

                if !keep_going {
                    break;
                }
            }

            crawler.running.set(false);
        });
    }
}

impl Default for Crawler {
    fn default() -> Self {
        Self::new()
    }
}
