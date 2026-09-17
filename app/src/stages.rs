//! Running a pipeline stage as a subprocess, and not leaving it behind.
//!
//! Three of the four stages are Python and always will be, so whoever drives
//! them runs `uv run qsuggest <stage>` and reads its output. Progress goes to
//! stderr and results to stdout, so both are read and interleaved.
//!
//! In the library because both drivers need it, and because the process-group
//! handling below must exist exactly once.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[cfg(feature = "local")]
use {
    crate::api::Stage,
    anyhow::Result,
    std::path::{Path, PathBuf},
    std::process::Stdio,
    std::sync::atomic::AtomicBool,
    std::time::Duration,
    tokio::io::{AsyncBufReadExt, BufReader},
};

#[cfg(feature = "local")]
/// How often to look at the cancel flag while a stage is running. It cannot
/// only be checked between output lines: `layout` goes silent for minutes
/// while UMAP runs.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// Keep the log bounded; `analyse` over a large corpus prints thousands of
/// lines and nobody scrolls back that far.
pub const MAX_LOG_LINES: usize = 400;

// ------------------------------------------------------------------ the log

/// A bounded log that can be read from a cursor. Cursor-based because the
/// transports differ: locally the subprocess below, remotely an SSE
/// connection. Both land here and are read the same way.
#[derive(Default)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<String>>,
    /// How many lines have ever been pushed. Counting rather than indexing
    /// means a cursor taken before an overflow still resolves.
    pushed: AtomicU64,
}

impl LogBuffer {
    pub fn push(&self, line: impl Into<String>) {
        let mut lines = self.lines.lock().unwrap();
        lines.push_back(line.into());
        while lines.len() > MAX_LOG_LINES {
            lines.pop_front();
        }
        self.pushed.fetch_add(1, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        // `pushed` deliberately does not reset: an old cursor should see the
        // clear as "nothing new", not a full replay.
        self.lines.lock().unwrap().clear();
    }

    pub fn cursor(&self) -> u64 {
        self.pushed.load(Ordering::SeqCst)
    }

    /// Everything currently retained. The views mirror this rather than
    /// accumulating their own copy, so a reconnecting stream does not show the
    /// tail twice.
    pub fn all(&self) -> Vec<String> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }

    /// Everything pushed since `since`, and the cursor to pass next time.
    pub fn since(&self, since: u64) -> crate::api::LogSlice {
        let lines = self.lines.lock().unwrap();
        let pushed = self.pushed.load(Ordering::SeqCst);
        let first_retained = pushed.saturating_sub(lines.len() as u64);
        let start = since.max(first_retained);
        let skip = (start - first_retained) as usize;

        crate::api::LogSlice {
            lines: lines.iter().skip(skip).cloned().collect(),
            cursor: pushed,
        }
    }
}

// ----------------------------------------------------------------- finding uv

/// Where `uv` lives. Not just "uv", a desktop launcher gives a minimal PATH
/// without ~/.local/bin, and the failure then looks like a broken pipeline.
#[cfg(feature = "local")]
pub fn uv_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        for candidate in [".local/bin/uv", ".cargo/bin/uv"] {
            let path = Path::new(&home).join(candidate);
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from("uv")
}

// --------------------------------------------------------- exit cleanup
//
// A stage outliving its driver is the worst failure here: `analyse` holds a
// pool of workers that would go on burning cores with nothing to stop them.
//
// One stage at a time, so this is a single atomic rather than a set behind a
// lock, the cleanup runs from a signal handler, where locking is not
// permitted but `kill` is.

#[cfg(all(unix, feature = "local"))]
static ACTIVE_GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(all(unix, feature = "local"))]
fn set_active_group(pgid: i32) {
    ACTIVE_GROUP.store(pgid, Ordering::SeqCst);
}

/// Signal whatever stage is still running. Safe to call more than once.
#[cfg(all(unix, feature = "local"))]
extern "C" fn reap_active_group() {
    let pgid = ACTIVE_GROUP.swap(0, Ordering::SeqCst);
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
}

#[cfg(all(unix, feature = "local"))]
extern "C" fn reap_then_die(signal: i32) {
    reap_active_group();
    // Restore the default action and re-raise, so the process still dies the
    // way whoever signalled it expects.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Make sure a running stage does not outlive its driver. Call once at startup.
///
/// `atexit` covers a normal quit; the handlers cover Ctrl-C or a session
/// shutdown. SIGKILL runs nothing, in practice the stage still dies when the
/// driver's pipes close, but that is the pipes doing it, not us.
#[cfg(feature = "local")]
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
#[cfg(feature = "local")]
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

// ------------------------------------------------------------------- running

/// Run one Python stage to completion, streaming its output into `log`.
/// Checks `cancel` on every line and at least every `CANCEL_POLL`, so a stop
/// lands within ~200ms even mid-`layout`.
#[cfg(feature = "local")]
pub async fn run(
    stage: Stage,
    repo_root: &Path,
    log: &LogBuffer,
    cancel: &AtomicBool,
) -> Result<i32> {
    let command = stage
        .command()
        .ok_or_else(|| anyhow::anyhow!("{} is not a Python stage", stage.label()))?;

    let uv = uv_path();
    let mut builder = tokio::process::Command::new(&uv);
    builder
        .args(["run", "qsuggest", command])
        .current_dir(repo_root.join("pipeline"))
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
        .map_err(|err| anyhow::anyhow!("could not start {}: {err}", uv.display()))?;

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
                Some(line) => log.push(line),
                None => out_done = true,
            },
            line = err.next_line(), if !err_done => match line? {
                // The CLI reports progress on stderr; it is not an error.
                Some(line) => log.push(line),
                None => err_done = true,
            },
            // Wakes the loop even when the stage is silent, so the check below
            // actually gets a chance to run.
            _ = tokio::time::sleep(CANCEL_POLL) => {}
        }

        if cancel.load(Ordering::SeqCst) {
            terminate(&mut child);
            log.push("cancelled");
            break;
        }
    }

    let status = child.wait().await?;
    #[cfg(unix)]
    set_active_group(0);
    Ok(status.code().unwrap_or(-1))
}

// ------------------------------------------------------------------- counts

/// Re-read the counts that tell you what still needs running. `in_space` and
/// `on_map` stay zero: they are questions about what a client has loaded. See
/// `api::Corpus`.
#[cfg(feature = "local")]
pub fn corpus(db_path: &Path) -> Result<crate::api::Corpus> {
    let conn = rusqlite::Connection::open(db_path)?;
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

    Ok(crate::api::Corpus {
        tracks: count("SELECT COUNT(*) FROM tracks"),
        analysed: count("SELECT COUNT(*) FROM features"),
        to_analyse,
        failed: count("SELECT COUNT(*) FROM failures"),
        pending: count("SELECT COUNT(*) FROM frontier WHERE state = 'pending'"),
        buildable,
        in_space: 0,
        on_map: 0,
    })
}
