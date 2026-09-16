//! Driving the whole pipeline from the app.
//!
//! Four stages: crawling is native, the other three are Python subprocesses
//! whose output is streamed here. Progress goes to stderr and results to
//! stdout, so both are read and interleaved.

use super::crawler::Crawler;
use dioxus::prelude::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// How often to look at the cancel flag while a stage is running. It cannot
/// only be checked between output lines: `layout` goes silent for minutes
/// while UMAP runs.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// Keep the log bounded; `analyse` over a large corpus prints thousands of
/// lines and nobody scrolls back that far.
const MAX_LOG_LINES: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Crawl,
    Analyse,
    BuildSpace,
    Layout,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Crawl => "crawl",
            Stage::Analyse => "analyse",
            Stage::BuildSpace => "build space",
            Stage::Layout => "layout",
        }
    }

    /// The `qsuggest` subcommand, for the three stages that are Python.
    fn command(self) -> Option<&'static str> {
        match self {
            Stage::Crawl => None,
            Stage::Analyse => Some("analyse"),
            Stage::BuildSpace => Some("build-space"),
            Stage::Layout => Some("layout"),
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Stage::Crawl => "Follow similar artists outward from your favourites.",
            Stage::Analyse => "Download excerpts and extract features. The slow one.",
            Stage::BuildSpace => "Assemble the vectors. Seconds, no network.",
            Stage::Layout => "UMAP projection for the map.",
        }
    }

    /// Whether finishing this stage invalidates the space the app has loaded.
    fn rebuilds_space(self) -> bool {
        matches!(self, Stage::BuildSpace | Stage::Layout)
    }
}

/// The three that terminate on their own, in dependency order. Crawl runs
/// until the frontier empties, so it is not queued behind other work.
pub const FULL_RUN: [Stage; 3] = [Stage::Analyse, Stage::BuildSpace, Stage::Layout];

/// What still needs doing, phrased as the stages themselves would phrase it.
///
/// Each field is asked the way its stage asks it. Deriving them arithmetically
/// is wrong: the populations differ over blocked artists.
#[derive(Clone, Default, PartialEq)]
pub struct Corpus {
    pub tracks: i64,
    /// Rows in `features`, whatever their artist.
    pub analysed: i64,
    /// Exactly what `analyse` would queue: unanalysed, not failed, not blocked.
    pub to_analyse: i64,
    pub failed: i64,
    /// Frontier entries waiting for a crawl.
    pub pending: i64,
    /// What `build-space` would include if run now.
    pub buildable: i64,
    /// Tracks in the space the app currently has loaded.
    pub in_space: i64,
    /// Of those, the ones with coordinates, the points actually drawn.
    pub on_map: i64,
}

#[derive(Clone, Copy)]
pub struct Pipeline {
    pub running: Signal<Option<Stage>>,
    pub log: Signal<Vec<String>>,
    pub corpus: Signal<Corpus>,
    /// Bumped when the space on disk has been rebuilt, so the shell knows to
    /// reload the engine and redraw the map.
    pub generation: Signal<u64>,
    /// (in_space, on_map), written by the shell, only it can see the engine.
    pub space_counts: Signal<(i64, i64)>,
    /// Stages still to run in a chained job.
    queue: Signal<Vec<Stage>>,
    cancel: Signal<bool>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            running: Signal::new(None),
            log: Signal::new(Vec::new()),
            corpus: Signal::new(Corpus::default()),
            generation: Signal::new(0),
            space_counts: Signal::new((0, 0)),
            queue: Signal::new(Vec::new()),
            cancel: Signal::new(false),
        }
    }

    fn say(mut self, line: impl Into<String>) {
        let mut log = self.log.write();
        log.push(line.into());
        let overflow = log.len().saturating_sub(MAX_LOG_LINES);
        if overflow > 0 {
            log.drain(..overflow);
        }
    }

    pub fn clear_log(mut self) {
        self.log.set(Vec::new());
    }

    /// Re-read the counts that tell you what still needs running. `in_space`
    /// and `on_map` come from the loaded engine: the question is what the app
    /// is drawing, not what is on disk.
    pub fn refresh(mut self) {
        let Ok(conn) = rusqlite::Connection::open(super::db_path()) else {
            return;
        };
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };

        // Mirrors analyse.pending_tracks, including the blocklist clause and
        // the stale-extractor check.
        let to_analyse = count(
            "SELECT COUNT(*) FROM tracks t
             LEFT JOIN features f ON f.track_id = t.id
             LEFT JOIN failures x ON x.track_id = t.id
             WHERE x.track_id IS NULL
               AND (f.track_id IS NULL
                    OR f.extractor_version != (SELECT extractor_version FROM features
                                               ORDER BY analysed_at DESC LIMIT 1))
               AND (t.artist_id IS NULL
                    OR t.artist_id NOT IN (SELECT artist_id FROM blocked_artists))",
        );

        // Mirrors space.load_rows.
        let buildable = count(
            "SELECT COUNT(*) FROM tracks t
             JOIN features f ON f.track_id = t.id
             WHERE t.artist_id IS NULL
                OR t.artist_id NOT IN (SELECT artist_id FROM blocked_artists)",
        );

        let (in_space, on_map) = *self.space_counts.peek();

        self.corpus.set(Corpus {
            tracks: count("SELECT COUNT(*) FROM tracks"),
            analysed: count("SELECT COUNT(*) FROM features"),
            to_analyse,
            failed: count("SELECT COUNT(*) FROM failures"),
            pending: count("SELECT COUNT(*) FROM frontier WHERE state = 'pending'"),
            buildable,
            in_space,
            on_map,
        });
    }

    pub fn cancel_running(mut self) {
        self.cancel.set(true);
        self.queue.set(Vec::new());
    }

    /// Run one Python stage, or start the native crawl.
    pub fn start(mut self, stage: Stage, crawler: Crawler) {
        if self.running.peek().is_some() {
            return;
        }
        if stage == Stage::Crawl {
            crawler.start(qsuggest::crawl::DEFAULT_MAX_DISTANCE);
            return;
        }

        self.cancel.set(false);
        self.running.set(Some(stage));
        self.say(format!("$ uv run qsuggest {}", stage.command().unwrap_or("")));

        spawn(async move {
            let pipeline = self;
            match run_python(pipeline, stage).await {
                Ok(0) => pipeline.say(format!("{} finished", stage.label())),
                Ok(code) => pipeline.say(format!("{} exited with status {code}", stage.label())),
                Err(err) => pipeline.say(format!("{} failed: {err:#}", stage.label())),
            }

            let mut pipeline = pipeline;
            pipeline.running.set(None);
            pipeline.refresh();
            if stage.rebuilds_space() {
                let next = *pipeline.generation.peek() + 1;
                pipeline.generation.set(next);
            }

            // Chained run: take the next stage, unless something cancelled it.
            let next = pipeline.queue.write().pop();
            if let Some(next) = next {
                if !*pipeline.cancel.peek() {
                    pipeline.start(next, crawler);
                }
            }
        });
    }

    /// Queue analyse, then build-space, then layout.
    pub fn start_full_run(mut self, crawler: Crawler) {
        if self.running.peek().is_some() {
            return;
        }
        // Reversed: the queue is popped from the back.
        let mut rest: Vec<Stage> = FULL_RUN[1..].to_vec();
        rest.reverse();
        self.queue.set(rest);
        self.start(FULL_RUN[0], crawler);
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Where `uv` lives. Not just "uv", a desktop launcher gives a minimal PATH
/// without ~/.local/bin, and the failure then looks like a broken pipeline.
fn uv_path() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        for candidate in [".local/bin/uv", ".cargo/bin/uv"] {
            let path = std::path::Path::new(&home).join(candidate);
            if path.is_file() {
                return path;
            }
        }
    }
    std::path::PathBuf::from("uv")
}

// --------------------------------------------------------- exit cleanup
//
// A stage outliving the window is the worst failure here: `analyse` holds a
// pool of workers that would go on burning cores with no UI to stop them.
//
// One stage at a time, so this is a single atomic rather than a set behind a
// lock, the cleanup runs from a signal handler, where locking is not
// permitted but `kill` is.

#[cfg(unix)]
static ACTIVE_GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
fn set_active_group(pgid: i32) {
    ACTIVE_GROUP.store(pgid, std::sync::atomic::Ordering::SeqCst);
}

/// Signal whatever stage is still running. Safe to call more than once.
#[cfg(unix)]
extern "C" fn reap_active_group() {
    let pgid = ACTIVE_GROUP.swap(0, std::sync::atomic::Ordering::SeqCst);
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
}

#[cfg(unix)]
extern "C" fn reap_then_die(signal: i32) {
    reap_active_group();
    // Restore the default action and re-raise, so the app still dies the way
    // whoever signalled it expects.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Make sure a running stage does not outlive the app. Call once at startup.
///
/// `atexit` covers a normal quit; the handlers cover Ctrl-C or a session
/// shutdown. SIGKILL runs nothing, in practice the stage still dies when the
/// app's pipes close, but that is the pipes doing it, not us.
pub fn install_exit_guard() {
    #[cfg(unix)]
    unsafe {
        libc::atexit(reap_active_group);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, reap_then_die as *const () as libc::sighandler_t);
        }
    }
}

/// Stop a stage and everything it started.
fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        set_active_group(0);
        // Negative pid addresses the process group. SIGTERM so Python can
        // unwind; the pool workers go with it.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        return;
    }
    let _ = child.start_kill();
}

async fn run_python(pipeline: Pipeline, stage: Stage) -> anyhow::Result<i32> {
    let command = stage
        .command()
        .ok_or_else(|| anyhow::anyhow!("{} is not a Python stage", stage.label()))?;

    let mut builder = tokio::process::Command::new(uv_path());
    builder
        .args(["run", "qsuggest", command])
        .current_dir(qsuggest::qobuz::repo_root().join("pipeline"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Its own process group, so cancelling takes the whole tree down.
    // `uv run` spawns qsuggest, which spawns its own workers; killing uv alone
    // leaves them running.
    #[cfg(unix)]
    builder.process_group(0);

    let mut child = builder
        .spawn()
        .map_err(|err| {
            anyhow::anyhow!("could not start {}: {err}", uv_path().display())
        })?;

    // Its pgid equals its pid, because of process_group(0) above.
    #[cfg(unix)]
    set_active_group(child.id().unwrap_or(0) as i32);

    let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
    let (mut out_done, mut err_done) = (false, false);

    loop {
        if out_done && err_done {
            break;
        }

        tokio::select! {
            line = out.next_line(), if !out_done => match line? {
                Some(line) => pipeline.say(line),
                None => out_done = true,
            },
            line = err.next_line(), if !err_done => match line? {
                // The CLI reports progress on stderr; it is not an error.
                Some(line) => pipeline.say(line),
                None => err_done = true,
            },
            // Wakes the loop even when the stage is silent, so the check below
            // actually gets a chance to run.
            _ = tokio::time::sleep(CANCEL_POLL) => {}
        }

        if *pipeline.cancel.peek() {
            terminate(&mut child);
            pipeline.say("cancelled");
            break;
        }
    }

    let status = child.wait().await?;
    #[cfg(unix)]
    set_active_group(0);
    Ok(status.code().unwrap_or(-1))
}

// ------------------------------------------------------------------- view

#[component]
pub fn PipelineView() -> Element {
    let pipeline = use_context::<Pipeline>();
    let crawler = use_context::<Crawler>();

    let running = *pipeline.running.read();
    let crawling = *crawler.running.read();
    let busy = running.is_some() || crawling;
    let corpus = pipeline.corpus.read().clone();
    let log = pipeline.log.read().clone();

    // The counts are what tell you which stage is worth running next.
    use_effect(move || {
        pipeline.refresh();
    });

    // Each hint names the stage that would fix it, and is derived from the
    // same question that stage asks.
    let needs_build = corpus.buildable != corpus.in_space;
    let needs_layout = corpus.in_space > corpus.on_map;

    rsx! {
        div { class: "pipeline",
            section { class: "panel",
                h2 { "Corpus" }
                div { class: "counts",
                    div { span { class: "count", "{corpus.tracks}" } span { class: "muted", "tracks" } }
                    div { span { class: "count", "{corpus.analysed}" } span { class: "muted", "analysed" } }
                    div { span { class: "count", "{corpus.to_analyse}" } span { class: "muted", "to analyse" } }
                    div { span { class: "count", "{corpus.on_map}" } span { class: "muted", "on the map" } }
                    div { span { class: "count", "{corpus.pending}" } span { class: "muted", "queued to crawl" } }
                    div { span { class: "count", "{corpus.failed}" } span { class: "muted", "failed" } }
                }
                if needs_build {
                    p { class: "muted notice",
                        "The space holds {corpus.in_space} tracks but {corpus.buildable} are ready, run build space."
                    }
                }
                if needs_layout {
                    p { class: "muted notice",
                        {format!("{} tracks in the space have no coordinates, run layout.",
                                 corpus.in_space - corpus.on_map)}
                    }
                }
                if !needs_build && !needs_layout && corpus.to_analyse == 0 {
                    p { class: "muted", "Up to date. Crawl for more, or analyse after a crawl." }
                }
            }

            section { class: "panel",
                h2 { "Stages" }
                div { class: "actions",
                    button {
                        class: "primary",
                        disabled: busy,
                        onclick: move |_| pipeline.start_full_run(crawler),
                        "run analyse → space → layout"
                    }
                    if busy {
                        button {
                            class: "danger",
                            onclick: move |_| {
                                pipeline.cancel_running();
                                crawler.request_stop();
                            },
                            "stop"
                        }
                    }
                }

                ul { class: "stages",
                    for stage in [Stage::Crawl, Stage::Analyse, Stage::BuildSpace, Stage::Layout] {
                        li {
                            key: "{stage.label()}",
                            class: if running == Some(stage) || (stage == Stage::Crawl && crawling) {
                                "stage active"
                            } else {
                                "stage"
                            },
                            div { class: "stage-name", "{stage.label()}" }
                            div { class: "stage-blurb muted", "{stage.blurb()}" }
                            div { class: "stage-run",
                                if stage == Stage::Crawl && crawling {
                                    button {
                                        class: "danger",
                                        onclick: move |_| crawler.request_stop(),
                                        "stop"
                                    }
                                } else {
                                    button {
                                        disabled: busy,
                                        onclick: move |_| pipeline.start(stage, crawler),
                                        "run"
                                    }
                                }
                            }
                        }
                    }
                }

                if crawling {
                    p { class: "muted ellipsis",
                        "crawling: "
                        {crawler.last.read().clone().unwrap_or_default()}
                    }
                }
            }

            section { class: "panel log-panel",
                h2 {
                    "Output"
                    span { class: "spacer" }
                    button { class: "chip", onclick: move |_| pipeline.clear_log(), "clear" }
                }
                pre { class: "log",
                    if log.is_empty() {
                        span { class: "muted", "Nothing run yet. Output from the pipeline appears here." }
                    }
                    for (index, line) in log.into_iter().enumerate() {
                        div { key: "{index}", "{line}" }
                    }
                }
            }
        }
    }
}
