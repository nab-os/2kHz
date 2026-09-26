# 10. The server process

By the end of this chapter you will know how `two-khz-server` starts, what
lives in its shared state, how a pipeline stage started from a phone runs to
completion on the server, how stop works, and how the slim catalogue is kept
in step with the space.

Files: `server/src/main.rs`, `server/src/cli.rs`, `server/src/hub.rs`,
`server/src/stages.rs`, `server/src/pipeline/mod.rs`.

## Entry: one binary, many commands

`main.rs` defines an `argh` CLI whose subcommands are grouped: serving
(`serve`, `pair`, `devices`, `revoke`, `build-catalog`), the account
(`login`, `refresh-credentials`, `whoami`, `favourites`), the pipeline
(`crawl`, `analyse`, `build-space`, `layout`, `models`, `status`,
`evaluate`, `demo`), and hiding (`block`, `unblock`, `blocked`).

`parse_args` accepts two American spellings (`favorites`, `analyze`) by
rewriting only the first argument before handing it to argh.

`main` handles the serving group itself and passes everything else to
`cli::run`, which builds a **current-thread** tokio runtime per command and
blocks on it. Pipeline commands report through `Job::stderr()`, a `Job`
whose sink is `eprintln!` and whose cancel flag is never set.

### Where things live: `Paths`

`pipeline::Paths::from_env()`:

| field | env var | default |
|---|---|---|
| `data_dir` | `TWO_KHZ_DATA_DIR` | `<repo>/data` |
| `db_path` | - | `<data_dir>/two_khz.db` |
| `model_dir` | `TWO_KHZ_MODEL_DIR` | `<repo>/data/models` |
| `cache_dir` | `TWO_KHZ_CACHE_DIR` | `<repo>/cache/audio` |
| `env_dir` | `TWO_KHZ_ENV_DIR` | `<repo>` |

"Repo" is found at *compile time* from `CARGO_MANIFEST_DIR`'s parent
(`qobuz::repo_root`). That is convenient in a checkout and irrelevant in
Docker, where the image sets all four variables to `/data…` and `/cache…`.

`Paths::under(root)` puts data and cache under one directory but keeps the
shared model dir, used by `demo` and tests.

## `serve`

```rust
fn serve(bind, paths, store) -> Result<()> {
    // warn if not loopback; warn if no devices are paired
    let hub = Arc::new(Hub::new(paths));
    let state = AppState { hub, auth: Arc::new(store), data_dir, db_path };
    let runtime = tokio::runtime::Runtime::new()?;          // multi-threaded
    runtime.block_on(async {
        tokio::spawn(watch_generation(hub, db_path, data_dir));
        axum::serve(listener, routes::router(state))
            .with_graceful_shutdown(shutdown()).await
    });
    runtime.shutdown_background();
}
```

- A non-loopback bind prints a loud warning: this is plain HTTP, and device
  tokens and signed stream URLs are credentials in flight.
- `shutdown()` waits for Ctrl-C, prints a message, and installs a second
  Ctrl-C handler that exits with 130 immediately, graceful shutdown waits
  for requests in flight, and an SSE log stream never finishes.
- `shutdown_background()` rather than dropping the runtime: dropping would
  wait for blocking threads, and a running stage sits on one until it is
  done. **A stage does not outlive the server.**

### `watch_generation`

Every 5 seconds: read `hub.pipeline_status().generation`; if it changed
since last time (including the very first pass, since a catalogue left by an
older server may not match the schema clients now read), rebuild
`catalog.db`. This is what keeps the slim catalogue in step with the space
without any stage having to know about it.

## `AppState` and the `Hub`

```rust
pub struct AppState {
    pub hub: Arc<Hub>,
    pub auth: Arc<AuthStore>,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
}
```

The routes are "a scope check and a call into `Hub`" (chapter 11). The `Hub`
(`hub.rs`) holds everything with state:

```rust
pub struct Hub {
    paths, env_dir, db_path, model_dir,
    client: OnceLock<Arc<tokio::sync::Mutex<QobuzClient>>>,  // built on first use
    text: tokio::sync::Mutex<Option<TextEncoder>>,          // loaded on first use
    text_tried: AtomicBool,
    crawl: Arc<CrawlJob>,
    pipeline: Arc<PipelineJob>,
    log: Arc<LogBuffer>,
}
```

### The browsing client

`Hub::qobuz()` builds a `QobuzClient` from `.env` the first time anything
needs Qobuz and caches it. Browsing the *space* needs no credentials, so a
missing `.env` only bites when you reach for Qobuz. Every browsing method is
`self.qobuz()?.lock().await.<call>().await`, one request at a time through
the shared client, which is fine because the rate limit serialises them
anyway.

### The text encoder

`encoder()` locks the mutex and, the first time only (`text_tried.swap`),
tries `TextEncoder::load`. After that, `embed` either uses it or returns a
helpful error naming the missing file; `can_steer` reports whether it
loaded.

## Jobs

Two kinds of long-running work, with different lifetimes:

- the **crawl** runs until the frontier empties or you stop it;
- the **stages** (`analyse`, `build-space`, `layout`) run until done.

### `PipelineJob`

```rust
struct PipelineJob {
    running: Mutex<Option<Stage>>,
    queued: Mutex<VecDeque<Stage>>,
    cancel: Arc<AtomicBool>,
    generation: AtomicU64,
}
```

`Hub::pipeline_start(stage)`:

1. `Crawl` is redirected to `crawl_start`.
2. If something is running, return `Ok` (idempotent, a double click is not
   an error).
3. Grab the shared rate limit if credentials exist; otherwise a fresh one.
   Only `analyse` talks to Qobuz, so `build-space` and `layout` still run
   without credentials, and `analyse` explains what is missing in the log.
4. Clear `cancel`, set `running`.
5. `tokio::spawn` a driver loop:

```rust
let mut current = Some(stage);
while let Some(stage) = current {
    running = Some(stage);
    log.push(format!("$ two-khz-server {}", stage.command()));
    let reporter = Job::new(move |line| sink.push(line), job.cancel.clone());
    let outcome = off_thread(move || async move {
        stages::run(stage, &paths, limit, &reporter).await
    }).await;
    // log "<stage> finished" | "cancelled" | "<stage> failed: <err>"
    if stage.rebuilds_space() { generation += 1; }
    current = if cancelled { queued.clear(); None } else { queued.pop_front() };
}
running = None;
```

`pipeline_start_full` fills `queued` with `FULL_RUN[1..]` =
`[BuildSpace, Layout]` and starts `FULL_RUN[0]` = `Analyse`. A chained run
is "just the next stage", handled by the loop rather than recursion (a
recursive async fn would need boxing).

`Stage::rebuilds_space()` is true for `BuildSpace` and `Layout`. Bumping
`generation` is the *only* signal clients get that their copy is stale,
and `watch_generation` rebuilds the catalogue off the same signal.

Note that `generation` is bumped whether the stage succeeded, failed or was
cancelled; a client will then re-check digests and simply find nothing
changed if nothing did.

### `stages::run`

```rust
match stage {
    Stage::Crawl => bail!("the crawl runs from its own loop, not as a stage"),
    Stage::Analyse => analyse::run(paths, limiter, &Options::default(), job).await?,
    Stage::BuildSpace => assemble::run(paths, None, job).await?,
    Stage::Layout => layout::run(paths, &layout::Options::default(), job)?,
}
```

The same functions the CLI calls, "there must never be a second
implementation". The difference is only the `Job`: stderr for the CLI, the
log buffer plus a shared cancel flag for the server.

### Why `off_thread`

Chapter 1, pattern 4: `analyse::run` holds a blocking `SqliteConnection`
across awaits. `off_thread` runs the future on a `spawn_blocking` thread
with its own current-thread runtime. The driver `await`s it from the main
runtime without blocking any worker.

### Stopping

`pipeline_stop` sets `cancel`, clears `queued`, **and** sets the crawl's
stop flag, one button stops everything. What happens next depends on the
stage (chapter 1, pattern 3): analyse finishes excerpts in flight, layout
stops within 25 epochs, build-space is seconds anyway.

### The crawl job

Covered in chapter 4. Its own OS thread (`two-khz-crawl`) with its own
runtime, a `Mutex<CrawlStatus>` for clients to poll, and an `AtomicBool`
stop flag.

## The log

`two_khz::logbuffer::LogBuffer`, shared by both crates:

- a `Mutex<VecDeque<String>>` capped at 400 lines (`MAX_LOG_LINES`);
- an `AtomicU64 pushed` counting every line ever pushed.

`since(cursor)` returns lines pushed after `cursor`. Because `pushed` counts
rather than indexes, a cursor taken before old lines were evicted still
resolves correctly: it computes `first_retained = pushed - len` and skips
accordingly. `clear()` empties the lines but deliberately does *not* reset
`pushed`, so an old cursor sees the clear as "nothing new", not a replay.

The server's buffer is shared by every connected device. Chapter 11 shows
how it is streamed.

## Counts: `stages::corpus`

Answers "what still needs running" for the pipeline view, each count asked
the way its stage asks it (chapter 1, pattern 7): `tracks`, `analysed`,
`to_analyse` (= `analyse::pending_count`), `failed`, `pending` frontier,
and `buildable` (analysed with a CLAP blob and not blocked, what
build-space would include). `in_space` and `on_map` are left at zero: they
are questions about what a *client* has loaded, which the server cannot
answer.

## Check yourself

1. Why does the server call `shutdown_background()` instead of dropping the
   runtime?
2. A phone starts `analyse` and is then switched off. What happens? What if
   the server is restarted?
3. Why is `pipeline_start` a no-op, not an error, when a stage is running?
4. What two things react to `generation` changing?
5. How does a `LogBuffer` cursor survive old lines being evicted?

## Exercises

1. Start the server, start `analyse` from the app, and watch
   `GET /api/pipeline` with `curl -H 'Authorization: Bearer …'` while it
   runs and after you stop it.
2. Add a `Stage::Evaluate` that runs `evaluate` and logs its output through
   the `Job`. What in `api.rs`, `stages.rs` and the UI has to change?
3. `generation` is in memory, so it resets to 0 on restart. Find the comment
   in `api.rs` that explains why clients still sync correctly.
