//! Where the app gets everything that is not the space.
//!
//! `Local` is a Qobuz client and SQLite in this process; `Remote` is the same
//! surface over HTTP. An enum rather than a trait object, there will only
//! ever be these two.
//!
//! Deliberately absent: neighbours, paths, drift, the map, the sliders. Those
//! stay on the in-process `engine()` in both modes. Only `embed` crosses.

#[cfg(feature = "local")]
pub mod local;
pub mod remote;

use crate::api::{
    BlockedArtist, Corpus, CrawlStatus, Device, PairingGrant, PipelineStatus, Scope, Stage,
};
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use anyhow::Result;
use std::sync::OnceLock;

#[cfg(feature = "local")]
pub use local::Local;
pub use remote::Remote;

/// Lives with the thing that produces log lines. Re-exported because every
/// caller reaches it through a backend.
pub use crate::stages::LogBuffer;

// -------------------------------------------------------------- the backend

pub enum Backend {
    /// Absent on mobile: there is no Essentia to drive, no `uv` to spawn and
    /// no repo checkout to find credentials in. See the `local` feature.
    #[cfg(feature = "local")]
    Local(Local),
    Remote(Remote),
}

/// Generate the dispatch, so adding a method does not mean writing the same
/// two-armed match for the twenty-ninth time.
macro_rules! dispatch {
    ($( $(#[$doc:meta])* $name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty; )*) => {
        impl Backend {
            $(
                $(#[$doc])*
                pub async fn $name(&self, $($arg: $ty),*) -> Result<$ret> {
                    match self {
                        #[cfg(feature = "local")]
                        Backend::Local(inner) => inner.$name($($arg),*).await,
                        Backend::Remote(inner) => inner.$name($($arg),*).await,
                    }
                }
            )*
        }
    };
}

dispatch! {
    // ------------------------------------------------------------- browsing
    search(query: &str, limit: usize) -> SearchResults;
    favourite_tracks(cap: usize) -> Vec<RemoteTrack>;
    favourite_albums(cap: usize) -> Vec<RemoteAlbum>;
    favourite_artists(cap: usize) -> Vec<RemoteArtist>;
    playlists(cap: usize) -> Vec<RemotePlaylist>;
    playlist_tracks(playlist_id: i64, cap: usize) -> Vec<RemoteTrack>;
    album_tracks(album_id: &str) -> Vec<RemoteTrack>;
    artist_albums(artist_id: i64, cap: usize) -> Vec<RemoteAlbum>;
    similar_artists(artist_id: i64, limit: usize) -> Vec<RemoteArtist>;

    // ------------------------------------------------------------ playback
    /// A signed, short-lived Qobuz URL. Audio never proxies through the
    /// server.
    file_url(track_id: i64, format_id: u32) -> String;
    export_playlist(name: &str, track_ids: &[i64]) -> i64;

    // ------------------------------------------------------- text steering
    /// A phrase as a CLAP text embedding. The only space operation that
    /// crosses the wire: 479MB model, 512 floats of answer.
    embed(phrase: &str) -> Vec<f32>;
    /// Whether text steering is available at all, the tower exported, and
    /// the corpus carrying the audio embeddings to anchor against.
    can_steer() -> bool;

    // ------------------------------------------------------------ hiding
    blocked_artists() -> Vec<BlockedArtist>;
    block_artist(artist_id: i64, name: &str) -> ();
    unblock_artist(artist_id: i64) -> ();

    // -------------------------------------------- extending the catalogue
    /// Pull a discography in and queue its albums. Returns how many.
    fetch_artist(artist_id: i64) -> usize;
    /// Pull one album's tracklist in. Returns how many tracks landed.
    fetch_album(album_id: &str) -> usize;
    corpus() -> Corpus;

    // ------------------------------------------------------------- crawling
    crawl_start(max_distance: i64) -> ();
    crawl_stop() -> ();
    crawl_status() -> CrawlStatus;

    // ------------------------------------------------------------- pipeline
    pipeline_start(stage: Stage) -> ();
    pipeline_start_full() -> ();
    pipeline_stop() -> ();
    pipeline_status() -> PipelineStatus;
    pipeline_log() -> Vec<String>;
    pipeline_clear_log() -> ();

    // ----------------------------------------------------------- the space
    /// Bring the local copy of `space.bin` up to date. A no-op locally.
    /// Returns whether anything changed, so the caller knows to reload.
    sync_space() -> bool;

    // ------------------------------------------------------------ devices
    /// Paired devices, for the pipeline view to list and revoke.
    devices() -> Vec<Device>;
    pair_device(name: &str, scope: Scope) -> PairingGrant;
    revoke_device(device_id: i64) -> ();
}

// ------------------------------------------------------------ the singleton

/// The backend, for the life of the process. A global for the same reason the
/// engine is: every panel needs it, and UI event handlers must be `Copy`.
static BACKEND: OnceLock<Backend> = OnceLock::new();

pub fn init(backend: Backend) {
    let _ = BACKEND.set(backend);
}

pub fn backend() -> &'static Backend {
    BACKEND.get().expect("backend::init runs before launch")
}

/// Whether this process is talking to a server. The pipeline view uses it to
/// decide whether to offer device pairing, which only a server can grant.
pub fn is_remote() -> bool {
    matches!(BACKEND.get(), Some(Backend::Remote(_)))
}
