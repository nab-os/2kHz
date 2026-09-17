//! Core of the Qobuz suggestion system.
//!
//! Reads what the Python pipeline produces, `qsuggest.db`, `space.bin`,
//! `space.json`, and answers navigation queries in process.
//!
//! Shared by three binaries, the server and the Android app. What runs behind
//! HTTP is in `backend`; what a build needs from its machine is the `local`
//! feature.

pub mod api;
pub mod backend;
pub mod crawl;
pub mod db;
pub mod map;
pub mod paths;
pub mod qobuz;
pub mod space;
pub mod stages;

/// The CLAP text tower, in this process. `local` only, a phone asks its
/// server to embed a phrase rather than carrying a 479MB model to do it.
#[cfg(feature = "local")]
pub mod text;

/// The window, and everything in it. Shared by every platform that has one.
#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod ui;

// No JNI entry point here on purpose: `dioxus-desktop` already exports
// `start_app` for Android, so defining another would be a duplicate
// `#[no_mangle]`. The Android build is the ordinary binary target below.

/// The webview backend, under whichever name this build's platform gives it.
/// `dioxus::desktop` and `dioxus::mobile` are the same crate re-exported
/// twice, so aliasing lets `app` and `ui` ignore which screen they got.
#[cfg(feature = "desktop")]
pub(crate) use dioxus::desktop as platform;
#[cfg(all(feature = "mobile", not(feature = "desktop")))]
pub(crate) use dioxus::mobile as platform;

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The loaded space, for the life of the process.
///
/// A global because every panel needs it and it is one thing. Not behind the
/// backend split: neighbours, paths, drift and the sliders are answered here
/// whether or not there is a server.
static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

pub fn init_engine(data_dir: &Path, db_path: &Path) -> Result<()> {
    let loaded = Engine::load(data_dir, db_path)?;
    let _ = ENGINE.set(Mutex::new(loaded));
    Ok(())
}

/// Swap in a freshly built space, after `build-space` or `layout`, or, in
/// remote mode, after a sync brought a newer one down.
pub fn reload_engine(data_dir: &Path, db_path: &Path) -> Result<()> {
    let loaded = Engine::load(data_dir, db_path)?;
    *engine().lock().unwrap() = loaded;
    Ok(())
}

pub fn engine() -> &'static Mutex<Engine> {
    ENGINE.get().expect("init_engine runs before launch")
}

/// Whether a space has been loaded yet. The mobile client starts before it has
/// synced one, so it has to be able to ask.
pub fn engine_ready() -> bool {
    ENGINE.get().is_some()
}

/// Everything the navigation side needs, loaded once at startup.
pub struct Engine {
    pub data_dir: PathBuf,
    pub space: space::Space,
    pub navigator: paths::Navigator,
}

impl Engine {
    pub fn load(data_dir: &Path, db_path: &Path) -> Result<Self> {
        let space = space::Space::load(data_dir)?;
        let catalog = db::Catalog::load(db_path, &space.manifest.track_ids)?;
        let weights = space.default_weights();
        let navigator = paths::Navigator::new(&space, &weights, catalog);

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            space,
            navigator,
        })
    }

    /// Whether the corpus carries the raw CLAP embeddings text steering
    /// anchors against. Only half the question, the backend answers whether
    /// the text tower is available.
    pub fn has_audio_embeddings(&self) -> bool {
        self.navigator.catalog.clap.is_some()
    }

    /// Rebuild the weighted view after a slider move. Milliseconds, no I/O.
    pub fn set_weights(&mut self, weights: &HashMap<String, f32>) -> Result<()> {
        let catalog = db::Catalog {
            tracks: std::mem::take(&mut self.navigator.catalog.tracks),
            clap: self.navigator.catalog.clap.take(),
            clap_dims: self.navigator.catalog.clap_dims,
            blocked_artists: std::mem::take(&mut self.navigator.catalog.blocked_artists),
        };
        self.navigator = paths::Navigator::new(&self.space, weights, catalog);
        Ok(())
    }

    /// Apply a block list the backend has already settled. Takes ids rather
    /// than reading the database, because remotely there is none here.
    /// Blocking only filters, so nothing needs rebuilding.
    pub fn set_blocked(&mut self, blocked: HashSet<i64>) {
        self.navigator.catalog.blocked_artists = blocked;
        // The kNN graph was built over the old visibility, so drop it.
        self.navigator.invalidate_graph();
    }

    /// Re-read the block list from a database this process can see.
    pub fn refresh_blocked(&mut self, db_path: &Path) -> Result<()> {
        self.set_blocked(db::blocked_artist_ids(db_path)?);
        Ok(())
    }

    pub fn blocked_count(&self) -> usize {
        self.navigator.catalog.blocked_artists.len()
    }
}

// ------------------------------------------------------------------ paths

/// Where a client keeps its own files. Overridden by `set_data_dir` from the
/// Android entry point, a hardcoded `/data/data/<pkg>` is wrong on work
/// profiles and secondary users.
static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the client data directory. Call before anything reads a path.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR_OVERRIDE.set(dir);
}

/// Locate the repo's data directory, allowing an override for tests.
///
/// The *server's* copy; a remote client keeps its synced copy elsewhere.
/// `CARGO_MANIFEST_DIR` is compile-time, so this only means anything where
/// build and run share a filesystem, never on Android.
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QSUGGEST_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // app/ lives next to data/ in the repo.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"))
}

pub fn default_db_path() -> PathBuf {
    default_data_dir().join("qsuggest.db")
}

/// Model weights are shared across corpora, so they do not follow
/// QSUGGEST_DATA_DIR. Mirrors `models.MODEL_DIR` on the Python side.
pub fn default_model_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QSUGGEST_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("data").join("models"))
        .unwrap_or_else(|| PathBuf::from("data").join("models"))
}

/// Where a client connected to a server keeps its synced copy of the space.
///
/// Not the repo's `data/`, that belongs to the server, and overwriting the
/// pipeline's own output would be a fine way to lose a corpus.
pub fn client_data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR_OVERRIDE.get() {
        return dir.clone();
    }
    if let Ok(dir) = std::env::var("QSUGGEST_DATA_DIR") {
        return PathBuf::from(dir);
    }

    #[cfg(target_os = "android")]
    if let Some(dir) = android_files_dir() {
        return dir;
    }

    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local")
                .join("share")
        });
    base.join("qsuggest")
}

/// The app's private directory, worked out without JNI.
///
/// Android sets no `HOME`, so the XDG fallback lands on `/` and every write
/// fails with EROFS. `/proc/self/cmdline` holds the package name, which is
/// enough to build the path without hardcoding it.
#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    let raw = std::fs::read_to_string("/proc/self/cmdline").ok()?;
    let package = raw
        .split('\0')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_string();
    if package.is_empty() {
        return None;
    }

    // `/data/user/0` is the real location; `/data/data` is a compatibility
    // symlink that does not exist for secondary users or work profiles.
    for base in ["/data/user/0", "/data/data"] {
        let dir = PathBuf::from(base).join(&package).join("files");
        if std::fs::create_dir_all(&dir).is_ok() {
            return Some(dir.join("qsuggest"));
        }
    }
    None
}

// ----------------------------------------------------------------- wiring

/// How this process reaches Qobuz and the pipeline.
///
/// Two environment variables on desktop:
///
/// ```sh
/// QSUGGEST_SERVER=https://nas.tailnet.ts.net:7700
/// QSUGGEST_TOKEN=<what `qsuggest-server pair` printed>
/// ```
///
/// With neither set, a `local` build does everything in process. A `mobile`
/// build reads `server.json` instead, see `Wiring::stored`.
pub enum Wiring {
    #[cfg(feature = "local")]
    Local {
        data_dir: PathBuf,
        db_path: PathBuf,
    },
    Remote {
        base: String,
        token: String,
        data_dir: PathBuf,
    },
}

/// A server address and a device token, as a mobile client stores them. A
/// phone has no environment variables, so the setup screen writes them here.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ServerConfig {
    pub base: String,
    pub token: String,
}

impl ServerConfig {
    pub fn path() -> PathBuf {
        client_data_dir().join("server.json")
    }

    pub fn load() -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path()).ok()?).ok()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

impl Wiring {
    /// Environment first, stored config second, local last.
    pub fn from_env() -> Result<Self> {
        if let Ok(base) = std::env::var("QSUGGEST_SERVER") {
            let token = std::env::var("QSUGGEST_TOKEN").map_err(|_| {
                anyhow::anyhow!(
                    "QSUGGEST_SERVER is set but QSUGGEST_TOKEN is not.\n\
                     Pair this device on the server:\n  \
                     qsuggest-server pair --name \"{}\" --scope play",
                    hostname()
                )
            })?;
            return Ok(Wiring::remote(base, token));
        }

        if let Some(stored) = ServerConfig::load() {
            return Ok(Wiring::remote(stored.base, stored.token));
        }

        #[cfg(feature = "local")]
        {
            Ok(Wiring::Local {
                data_dir: default_data_dir(),
                db_path: default_db_path(),
            })
        }

        // A mobile build has no local half to fall back to: no Essentia, no
        // `uv`, no checkout to find a .env in.
        #[cfg(not(feature = "local"))]
        {
            anyhow::bail!("not paired with a server yet")
        }
    }

    pub fn remote(base: String, token: String) -> Self {
        Wiring::Remote {
            base,
            token,
            data_dir: client_data_dir(),
        }
    }

    /// Where the engine should load the space from.
    pub fn data_dir(&self) -> &Path {
        match self {
            #[cfg(feature = "local")]
            Wiring::Local { data_dir, .. } => data_dir,
            Wiring::Remote { data_dir, .. } => data_dir,
        }
    }

    /// Which database the catalogue comes from. In remote mode this is the
    /// slim copy that sync brings down, not the 269MB one the server keeps.
    pub fn db_path(&self) -> PathBuf {
        match self {
            #[cfg(feature = "local")]
            Wiring::Local { db_path, .. } => db_path.clone(),
            Wiring::Remote { data_dir, .. } => data_dir.join("catalog.db"),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Wiring::Remote { .. })
    }

    pub fn into_backend(self) -> backend::Backend {
        match self {
            #[cfg(feature = "local")]
            Wiring::Local { db_path, .. } => backend::Backend::Local(backend::Local::new(
                qobuz::repo_root(),
                db_path,
                default_model_dir(),
            )),
            Wiring::Remote {
                base,
                token,
                data_dir,
            } => backend::Backend::Remote(backend::Remote::new(base, token, data_dir)),
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this device".into())
}
