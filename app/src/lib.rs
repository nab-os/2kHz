//! Core of the Qobuz suggestion desktop app.
//!
//! Reads what the Python pipeline produces, `qsuggest.db`, `space.bin`,
//! `space.json`, and answers navigation queries in process.

pub mod db;
pub mod paths;
pub mod qobuz;
pub mod space;
pub mod text;

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The loaded space, for the life of the process. A global because every
/// panel needs it and it is one thing.
static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

pub fn init_engine(data_dir: &Path, db_path: &Path) -> Result<()> {
    let loaded = Engine::load(data_dir, db_path)?;
    let _ = ENGINE.set(Mutex::new(loaded));
    Ok(())
}

/// Swap in a freshly built space, after `build-space` or `layout`.
pub fn reload_engine(data_dir: &Path, db_path: &Path) -> Result<()> {
    let loaded = Engine::load(data_dir, db_path)?;
    *engine().lock().unwrap() = loaded;
    Ok(())
}

pub fn engine() -> &'static Mutex<Engine> {
    ENGINE.get().expect("init_engine runs before launch")
}

/// Everything the app needs, loaded once at startup.
pub struct Engine {
    pub data_dir: PathBuf,
    pub space: space::Space,
    pub navigator: paths::Navigator,
    /// None when the text tower has not been exported; the UI hides steering
    /// rather than refusing to start.
    pub text_encoder: Option<text::TextEncoder>,
}

impl Engine {
    pub fn load(data_dir: &Path, db_path: &Path) -> Result<Self> {
        let space = space::Space::load(data_dir)?;
        let catalog = db::Catalog::load(db_path, &space.manifest.track_ids)?;
        let weights = space.default_weights();
        let navigator = paths::Navigator::new(&space, &weights, catalog);

        // Neither of these is fatal: the app is still fully usable for
        // neighbours and paths without text steering.
        let text_encoder = text::TextEncoder::load(&default_model_dir()).unwrap_or(None);

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            space,
            navigator,
            text_encoder,
        })
    }

    pub fn can_steer(&self) -> bool {
        self.text_encoder.is_some() && self.navigator.catalog.clap.is_some()
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

    /// Re-read the block list after the app changes it. Blocking only
    /// filters, so nothing needs rebuilding, which is why it works
    /// mid-session.
    pub fn refresh_blocked(&mut self, db_path: &Path) -> Result<()> {
        self.navigator.catalog.blocked_artists = db::blocked_artist_ids(db_path)?;
        // The kNN graph was built over the old visibility, so drop it.
        self.navigator.invalidate_graph();
        Ok(())
    }

    pub fn blocked_count(&self) -> usize {
        self.navigator.catalog.blocked_artists.len()
    }

    /// Walk away from a track towards a described sound.
    pub fn drift_by_text(
        &mut self,
        from: i64,
        phrase: &str,
        steps: usize,
        anchors: usize,
    ) -> Result<Vec<paths::Step>> {
        let Some(encoder) = self.text_encoder.as_mut() else {
            anyhow::bail!(
                "text steering needs data/models/{}; run: uv run python -m qsuggest.features.onnx_export",
                text::ONNX_NAME
            );
        };
        let embedding = encoder.embed(phrase)?;
        Ok(self.navigator.drift_to_text(
            from,
            &embedding,
            steps,
            anchors,
            &paths::Constraints::default(),
        ))
    }
}

/// Locate the repo's data directory, allowing an override for tests.
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
        .unwrap_or_else(|| PathBuf::from("data/models"))
}
