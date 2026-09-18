//! Everything in this process, on this disk. What the app has always done.
//!
//! The crawl loop and pipeline driver live here rather than in the UI: they
//! publish a status the view polls, because a server cannot write signals.

use crate::api::{
    BlockedArtist, Corpus, CrawlStatus, Device, LogSlice, PairingGrant, PipelineStatus, Scope,
    Stage, FULL_RUN,
};
use crate::qobuz::{QobuzClient, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::stages::{self, LogBuffer};
use crate::text::TextEncoder;
use crate::{crawl, db};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub struct Local {
    repo_root: PathBuf,
    db_path: PathBuf,
    model_dir: PathBuf,
    /// Built on first use: browsing the space needs no credentials, so a
    /// missing .env should only bite when you reach for Qobuz.
    client: OnceLock<Arc<tokio::sync::Mutex<QobuzClient>>>,
    /// Loaded on first use and kept. `None` inside means the tower has not
    /// been exported, which is not an error, the UI hides steering.
    text: tokio::sync::Mutex<Option<TextEncoder>>,
    text_tried: AtomicBool,
    crawl: Arc<CrawlJob>,
    pipeline: Arc<PipelineJob>,
    log: Arc<LogBuffer>,
}

// ------------------------------------------------------------------- jobs

#[derive(Default)]
struct CrawlJob {
    status: Mutex<CrawlStatus>,
    /// Set to ask the loop to stop after the current step.
    stop: AtomicBool,
}

#[derive(Default)]
struct PipelineJob {
    running: Mutex<Option<Stage>>,
    /// Stages still to run in a chained job, in the order they will run.
    queued: Mutex<VecDeque<Stage>>,
    cancel: AtomicBool,
    generation: AtomicU64,
}

impl Local {
    pub fn new(repo_root: PathBuf, db_path: PathBuf, model_dir: PathBuf) -> Self {
        Self {
            repo_root,
            db_path,
            model_dir,
            client: OnceLock::new(),
            text: tokio::sync::Mutex::new(None),
            text_tried: AtomicBool::new(false),
            crawl: Arc::new(CrawlJob::default()),
            pipeline: Arc::new(PipelineJob::default()),
            log: Arc::new(LogBuffer::default()),
        }
    }

    /// The shared Qobuz client, built on first reach.
    fn qobuz(&self) -> Result<Arc<tokio::sync::Mutex<QobuzClient>>> {
        if let Some(existing) = self.client.get() {
            return Ok(existing.clone());
        }
        let built = QobuzClient::from_repo(&self.repo_root)?;
        let _ = self.client.set(Arc::new(tokio::sync::Mutex::new(built)));
        Ok(self.client.get().expect("just set").clone())
    }

    // -------------------------------------------------------------- browsing

    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        self.qobuz()?.lock().await.search(query, limit).await
    }

    pub async fn favourite_tracks(&self, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.qobuz()?.lock().await.favorite_tracks(cap).await
    }

    pub async fn favourite_albums(&self, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.qobuz()?.lock().await.favorite_albums(cap).await
    }

    pub async fn favourite_artists(&self, cap: usize) -> Result<Vec<RemoteArtist>> {
        self.qobuz()?.lock().await.favorite_artists(cap).await
    }

    pub async fn playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        self.qobuz()?.lock().await.user_playlists(cap).await
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.qobuz()?
            .lock()
            .await
            .playlist_tracks(playlist_id, cap)
            .await
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<RemoteTrack>> {
        Ok(self.qobuz()?.lock().await.album_tracks(album_id).await?.1)
    }

    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self
            .qobuz()?
            .lock()
            .await
            .artist_albums(artist_id, cap)
            .await?
            .1)
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        self.qobuz()?
            .lock()
            .await
            .similar_artists(artist_id, limit)
            .await
    }

    // -------------------------------------------------------------- playback

    pub async fn file_url(&self, track_id: i64, format_id: u32) -> Result<String> {
        self.qobuz()?.lock().await.file_url(track_id, format_id).await
    }

    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        self.qobuz()?
            .lock()
            .await
            .export_playlist(name, track_ids)
            .await
    }

    // --------------------------------------------------------- text steering

    /// Load the tower once, remembering that we tried. A missing export is not
    /// an error, it is the ordinary state of a fresh checkout.
    async fn encoder(&self) -> tokio::sync::MutexGuard<'_, Option<TextEncoder>> {
        let mut guard = self.text.lock().await;
        if !self.text_tried.swap(true, Ordering::SeqCst) {
            *guard = TextEncoder::load(&self.model_dir).unwrap_or(None);
        }
        guard
    }

    pub async fn embed(&self, phrase: &str) -> Result<Vec<f32>> {
        let mut guard = self.encoder().await;
        let encoder = guard.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "text steering needs {}/{}; run: uv run python -m two_khz.features.onnx_export",
                self.model_dir.display(),
                crate::text::ONNX_NAME
            )
        })?;
        encoder.embed(phrase)
    }

    pub async fn can_steer(&self) -> Result<bool> {
        Ok(self.encoder().await.is_some())
    }

    // ---------------------------------------------------------------- hiding

    pub async fn blocked_artists(&self) -> Result<Vec<BlockedArtist>> {
        Ok(db::blocked_artists(&self.db_path)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn block_artist(&self, artist_id: i64, name: &str) -> Result<()> {
        db::block_artist(&self.db_path, artist_id, name, None)
    }

    pub async fn unblock_artist(&self, artist_id: i64) -> Result<()> {
        db::unblock_artist(&self.db_path, artist_id)
    }

    // ------------------------------------------- extending the catalogue

    /// What a worker thread needs to talk to Qobuz and the database. The rate
    /// limit is shared; everything else it builds for itself, because none of
    /// it may cross a thread boundary mid-await.
    async fn worker_context(&self) -> Result<(PathBuf, PathBuf, crate::qobuz::RateLimit)> {
        let limit = self.qobuz()?.lock().await.rate_limit();
        Ok((self.repo_root.clone(), self.db_path.clone(), limit))
    }

    pub async fn fetch_artist(&self, artist_id: i64) -> Result<usize> {
        let (repo_root, db_path, limit) = self.worker_context().await?;
        off_thread(move || async move {
            let (conn, mut client) = worker_parts(&repo_root, &db_path, limit)?;
            crawl::discover_artist(&conn, &mut client, artist_id).await
        })
        .await
    }

    pub async fn fetch_album(&self, album_id: &str) -> Result<usize> {
        let (repo_root, db_path, limit) = self.worker_context().await?;
        let album_id = album_id.to_string();
        off_thread(move || async move {
            let (conn, mut client) = worker_parts(&repo_root, &db_path, limit)?;
            crawl::crawl_one_album(&conn, &mut client, &album_id).await
        })
        .await
    }

    pub async fn corpus(&self) -> Result<Corpus> {
        stages::corpus(&self.db_path)
    }

    // -------------------------------------------------------------- crawling

    /// Start a crawl on a thread of its own.
    ///
    /// Not `tokio::spawn`: `rusqlite::Connection` is `Send` but not `Sync` and
    /// `crawl::step` holds `&Connection` across awaits, so the future is
    /// `!Send`. Its own client, but the same `RateLimit`, the account is what
    /// 2/s protects, not the socket.
    pub async fn crawl_start(&self, max_distance: i64) -> Result<()> {
        if self.crawl.status.lock().unwrap().running {
            return Ok(());
        }

        // Built here, on purpose: a missing .env should fail the button press
        // rather than a thread nobody is watching.
        let limit = self.qobuz()?.lock().await.rate_limit();
        let repo_root = self.repo_root.clone();
        let db_path = self.db_path.clone();
        let job = self.crawl.clone();

        job.stop.store(false, Ordering::SeqCst);
        *job.status.lock().unwrap() = CrawlStatus {
            running: true,
            last: Some("starting…".into()),
            ..Default::default()
        };

        std::thread::Builder::new()
            .name("two-khz-crawl".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let mut status = job.status.lock().unwrap();
                        status.last = Some(format!("could not start the crawl: {err}"));
                        status.running = false;
                        return;
                    }
                };

                runtime.block_on(async {
                    if let Err(err) = crawl_loop(&job, &repo_root, limit, &db_path, max_distance).await
                    {
                        job.status.lock().unwrap().last = Some(format!("{err:#}"));
                    }
                });

                job.status.lock().unwrap().running = false;
            })
            .context("spawning the crawl thread")?;

        Ok(())
    }

    pub async fn crawl_stop(&self) -> Result<()> {
        self.crawl.stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn crawl_status(&self) -> Result<CrawlStatus> {
        Ok(self.crawl.status.lock().unwrap().clone())
    }

    // -------------------------------------------------------------- pipeline

    pub async fn pipeline_start(&self, stage: Stage) -> Result<()> {
        if stage == Stage::Crawl {
            return self.crawl_start(crawl::DEFAULT_MAX_DISTANCE).await;
        }
        if self.pipeline.running.lock().unwrap().is_some() {
            return Ok(());
        }

        let job = self.pipeline.clone();
        let log = self.log.clone();
        let repo_root = self.repo_root.clone();

        job.cancel.store(false, Ordering::SeqCst);
        *job.running.lock().unwrap() = Some(stage);

        tokio::spawn(async move {
            // A loop rather than recursion: a chained run is just the next
            // stage, and boxing a recursive future to say so would be worse.
            let mut current = Some(stage);

            while let Some(stage) = current {
                *job.running.lock().unwrap() = Some(stage);
                log.push(format!(
                    "$ uv run two-khz {}",
                    stage.command().unwrap_or("")
                ));

                match stages::run(stage, &repo_root, &log, &job.cancel).await {
                    Ok(0) => log.push(format!("{} finished", stage.label())),
                    Ok(code) => log.push(format!("{} exited with status {code}", stage.label())),
                    Err(err) => log.push(format!("{} failed: {err:#}", stage.label())),
                }

                if stage.rebuilds_space() {
                    job.generation.fetch_add(1, Ordering::SeqCst);
                }

                current = if job.cancel.load(Ordering::SeqCst) {
                    job.queued.lock().unwrap().clear();
                    None
                } else {
                    job.queued.lock().unwrap().pop_front()
                };
            }

            *job.running.lock().unwrap() = None;
        });

        Ok(())
    }

    pub async fn pipeline_start_full(&self) -> Result<()> {
        if self.pipeline.running.lock().unwrap().is_some() {
            return Ok(());
        }
        *self.pipeline.queued.lock().unwrap() = FULL_RUN[1..].iter().copied().collect();
        self.pipeline_start(FULL_RUN[0]).await
    }

    pub async fn pipeline_stop(&self) -> Result<()> {
        self.pipeline.cancel.store(true, Ordering::SeqCst);
        self.pipeline.queued.lock().unwrap().clear();
        self.crawl.stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn pipeline_status(&self) -> Result<PipelineStatus> {
        Ok(PipelineStatus {
            running: *self.pipeline.running.lock().unwrap(),
            queued: self.pipeline.queued.lock().unwrap().iter().copied().collect(),
            generation: self.pipeline.generation.load(Ordering::SeqCst),
        })
    }

    pub async fn pipeline_log(&self) -> Result<Vec<String>> {
        Ok(self.log.all())
    }

    /// Incremental read, for the server's SSE handler. Views use
    /// `pipeline_log`; this exists so a stream can push only what is new.
    pub async fn pipeline_log_since(&self, since: u64) -> Result<LogSlice> {
        Ok(self.log.since(since))
    }

    pub async fn pipeline_clear_log(&self) -> Result<()> {
        self.log.clear();
        Ok(())
    }

    // ----------------------------------------------------------- the space

    /// Nothing to sync: there is one copy and the engine reads it directly.
    pub async fn sync_space(&self) -> Result<bool> {
        Ok(false)
    }

    // ------------------------------------------------------------ devices

    pub async fn devices(&self) -> Result<Vec<Device>> {
        anyhow::bail!("device pairing needs a server; run two-khz-server and connect to it")
    }

    pub async fn pair_device(&self, _name: &str, _scope: Scope) -> Result<PairingGrant> {
        anyhow::bail!("device pairing needs a server; run two-khz-server and connect to it")
    }

    pub async fn revoke_device(&self, _device_id: i64) -> Result<()> {
        anyhow::bail!("device pairing needs a server; run two-khz-server and connect to it")
    }
}

// --------------------------------------------------------------- off-thread
//
// Crawl futures hold `&Connection` across awaits, so they are `!Send` and no
// work-stealing runtime will schedule them. Rather than restructure
// `crawl::step`, the work moves to a thread with its own runtime. The client
// is separate but the budget is shared, see `qobuz::RateLimit`.

/// A connection and a client, both belonging to the calling thread.
fn worker_parts(
    repo_root: &Path,
    db_path: &Path,
    limit: crate::qobuz::RateLimit,
) -> Result<(rusqlite::Connection, QobuzClient)> {
    let conn = db::open_for_write(db_path).context("opening the database")?;
    let credentials = crate::qobuz::Credentials::from_env(repo_root)?;
    Ok((conn, QobuzClient::sharing(credentials, limit)))
}

/// Drive a `!Send` future to completion somewhere it is allowed to live. `job`
/// is `Send`; the future it produces need not be.
async fn off_thread<T, F, Fut>(job: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>>,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building a runtime for the worker thread")?;
        runtime.block_on(job())
    })
    .await
    .context("the worker thread panicked")?
}

// ------------------------------------------------------------- the crawl loop

/// Run the frontier until it empties, the budget is hit, or stop is asked. The
/// Qobuz client is released around each step, so search and playback never wait
/// longer than one item.
async fn crawl_loop(
    job: &CrawlJob,
    repo_root: &Path,
    limit: crate::qobuz::RateLimit,
    db_path: &Path,
    max_distance: i64,
) -> Result<()> {
    // This thread's own connection and client, but not its own budget, both
    // halves talk to one account. See `qobuz::RateLimit`.
    let (conn, mut client) = worker_parts(repo_root, db_path, limit)?;

    // No budget: the frontier and the stop button are the limits.
    let max_tracks = i64::MAX;
    let mut stats = crawl::Stats::default();

    loop {
        if job.stop.load(Ordering::SeqCst) {
            job.status.lock().unwrap().last = Some("stopped".into());
            break;
        }

        let result = crawl::step(&conn, &mut client, max_tracks, max_distance).await?;

        let line = match &result {
            crawl::StepResult::Expanded { kind, ref_id } => format!("{kind} {ref_id}"),
            crawl::StepResult::Skipped { kind, ref_id } => format!("skipped {kind} {ref_id}"),
            crawl::StepResult::Failed { kind, ref_id, error } => {
                format!("{kind} {ref_id}: {error}")
            }
            crawl::StepResult::Exhausted => "frontier exhausted".into(),
            crawl::StepResult::BudgetReached => "budget reached".into(),
        };

        let keep_going = stats.absorb(&result);

        {
            let mut status = job.status.lock().unwrap();
            status.last = Some(line);
            status.artists_expanded = stats.artists_expanded;
            status.albums_expanded = stats.albums_expanded;
            status.tracks_added = stats.tracks_added;
            status.errors = stats.errors;
            status.blocked_skipped = stats.blocked_skipped;
            status.tracks = conn
                .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
                .unwrap_or(0);
            status.pending = conn
                .query_row(
                    "SELECT COUNT(*) FROM frontier WHERE state = 'pending'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
        }

        if !keep_going {
            break;
        }
    }

    Ok(())
}
