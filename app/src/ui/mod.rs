//! The Qobuz half: browsing, playing, and crawling more of it. Live, so it
//! sees the whole catalogue rather than just the analysed corpus; the two
//! halves meet on track ids.
//!
//! This file holds the shared Qobuz client, the database path, and the
//! contexts every panel reads.

pub mod crawler;
pub mod generate;
pub mod library;
pub mod player;

pub use crawler::Crawler;
pub use generate::{GeneratePanel, Generator};
pub use library::{open_initial, Library, LibraryPanel};
pub use player::{use_transport, Player, PlayerBar};

use dioxus::prelude::*;
use qsuggest::db;
use qsuggest::qobuz::{QobuzClient, RemoteTrack};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

/// Qobuz pages at 100. These caps stop a four-thousand-track favourites list
/// from stalling the panel the first time it is opened.
pub(crate) const LIST_CAP: usize = 500;
pub(crate) const SEARCH_LIMIT: usize = 50;

static QOBUZ: OnceLock<tokio::sync::Mutex<QobuzClient>> = OnceLock::new();

/// Built on first use: the app is fully usable for browsing the space without
/// credentials, so a missing .env should only bite when you reach for Qobuz.
pub fn client() -> anyhow::Result<&'static tokio::sync::Mutex<QobuzClient>> {
    if let Some(existing) = QOBUZ.get() {
        return Ok(existing);
    }
    let built = QobuzClient::from_repo(&qsuggest::qobuz::repo_root())?;
    let _ = QOBUZ.set(tokio::sync::Mutex::new(built));
    Ok(QOBUZ.get().expect("just set"))
}

// ------------------------------------------------------------------ context

/// Track ids in the analysed space and not hidden. A memo, not a snapshot,
/// hiding an artist changes it and the rows reading it must re-render.
#[derive(Clone, Copy)]
pub struct LocalIds(pub Memo<Rc<HashSet<i64>>>);

/// The map/neighbours selection, owned by the app shell. Browsing can drive it
/// when a Qobuz result turns out to be a track that was analysed.
#[derive(Clone, Copy)]
pub struct Selection(pub Signal<Option<i64>>);

/// Artists the user has hidden, and the actions that change that. A signal
/// rather than a per-render read: the engine's copy sits behind a mutex the UI
/// cannot subscribe to.
#[derive(Clone, Copy)]
pub struct Blocklist {
    pub artists: Signal<Vec<db::BlockedArtist>>,
    /// (artist_id, name)
    pub block: Callback<(i64, String)>,
    pub unblock: Callback<i64>,
}

impl Blocklist {
    pub fn contains(&self, artist_id: i64) -> bool {
        self.artists
            .read()
            .iter()
            .any(|entry| entry.artist_id == artist_id)
    }

    /// Matches on the performer's artist id, so a featured credit filed under
    /// someone else's id can still slip through.
    fn hides(&self, track: &RemoteTrack) -> bool {
        track.artist_id.is_some_and(|id| self.contains(id))
    }
}

static DB_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Point the crawl-queueing path at the database the engine was loaded from.
pub fn set_db_path(path: std::path::PathBuf) {
    let _ = DB_PATH.set(path);
}

/// Process-global rather than a context value: it never changes, and a
/// non-Copy context cannot be captured by the per-row event handlers.
pub(crate) fn db_path() -> &'static std::path::Path {
    DB_PATH.get_or_init(qsuggest::default_db_path)
}
