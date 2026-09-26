# 1. The toolkit

By the end of this chapter you will recognise every major dependency, know
how the two crates and their feature flags fit together, and be able to spot
the half-dozen patterns that the rest of the code leans on.

## The crates, by job

| job | crate | where |
|---|---|---|
| async runtime | `tokio` | both |
| HTTP server | `axum` 0.8 | server |
| HTTP client | `reqwest` (rustls, no OpenSSL) | both |
| SQL | `diesel` 2 + bundled `libsqlite3-sys` | both |
| CLI parsing | `argh` | server |
| errors | `anyhow` | both |
| JSON | `serde`, `serde_json` | both |
| memory-mapped files | `memmap2` | app |
| byte reinterpretation | `bytemuck` | both |
| hashing | `md-5` (sync digests, Qobuz signing), `sha2` (tokens, weights) | both |
| ONNX inference | `ort` (ONNX Runtime) | server |
| tokenising text | `tokenizers` | server |
| MP3 decoding | `symphonia` (mp3 only) | server |
| FFT | `realfft` | server |
| loudness | `ebur128` | server |
| eigendecomposition | `nalgebra` | server |
| scraping credentials | `regex` | server |
| UI | `dioxus` 0.7 (desktop/mobile webview) | app |

A few of these deserve a sentence each.

- **Diesel** is a typed query builder. You declare tables with the
  `diesel::table!` macro (`app/src/schema.rs`), and queries like
  `tracks::table.filter(tracks::id.eq(1))` are checked at compile time. The
  schema here is written *by hand* and must match `schema.sql` (chapter 2).
- **`ort`** runs ONNX models, neural networks exported from PyTorch into a
  portable format. With the `download-binaries` feature it fetches a
  prebuilt ONNX Runtime at build time, which is why the Docker build needs
  network access (chapter 17).
- **Dioxus** is a React-like UI framework for Rust. On desktop and Android it
  renders into a system *webview* (WebKitGTK on Linux, Android's WebView),
  so the UI is HTML/CSS, driven from Rust. Chapter 14 is a primer.
- **`argh`** derives a CLI from structs with doc comments as help text.

## Two crates, not a workspace

`app/` and `server/` are separate Cargo projects with separate `target/`
directories. `server/Cargo.toml` explains why: a shared target directory
would force full rebuilds of heavy dependencies (ONNX Runtime, bundled
SQLite, Dioxus) whenever one side's feature set differs from the other's.

The server pulls the app in as a path dependency:

```toml
two-khz-app = { path = "../app", default-features = false }
```

### The app's feature flags

From `app/Cargo.toml`:

```toml
[features]
default = ["desktop"]
gui     = ["dep:dioxus"]          # anything with a screen
desktop = ["gui", "dioxus/desktop"]
mobile  = ["gui", "dioxus/mobile"]
web     = ["gui", "dioxus/web"]   # exploratory, does not work
```

And in `app/src/lib.rs`:

```rust
#[cfg(feature = "gui")] pub mod app;
#[cfg(feature = "gui")] pub mod ui;
```

So:

- `cargo build` in `app/` → desktop app (default feature).
- `--no-default-features --features mobile` → the Android build.
- `default-features = false` from the server → **no `gui`**, so no
  `app`/`ui` modules and no Dioxus at all. The server gets `api`, `schema`,
  `qobuz`, `space`, `db`, `paths`, `map`, `logbuffer` and `backend`.

`lib.rs` also aliases the platform:

```rust
#[cfg(feature = "desktop")]
pub(crate) use dioxus::desktop as platform;
#[cfg(all(feature = "mobile", not(feature = "desktop")))]
pub(crate) use dioxus::mobile as platform;
```

`dioxus::desktop` and `dioxus::mobile` are the same crate re-exported, so the
UI code writes `crate::platform::…` and never cares which it got.

The library has `crate-type = ["lib", "cdylib"]`: `lib` for the server to
link, `cdylib` so the Android APK can load it as a `.so`.

### SQLite is linked once

Both crates depend on `libsqlite3-sys` with `bundled`. Only one crate in a
build may link a given native library, so both must agree on the version
(0.38). The server names it explicitly rather than relying on the app's
target-gated dependency.

### The rustls note

`app/Cargo.toml` lists `rustls` with `prefer-post-quantum` even though no
source file names it. This is a Cargo *feature unification* trick: reqwest
pulls in rustls with default features off, which leaves a post-quantum key
exchange group advertised but without a key share, forcing a
HelloRetryRequest round-trip that some ingress proxies mishandle. Naming
rustls with that feature turns it back on for the whole build. It is a good
example of a dependency line that exists purely for its side effect.

### `opt-level = 3` in dev

`server/Cargo.toml` sets `[profile.dev] opt-level = 3`. The analyser's
resampling, mel spectrogram and descriptor loops run ~7× slower unoptimised
(8.8s per excerpt vs 1.3s), which would make `cargo run -- analyse` unusable.

## Patterns you will see everywhere

These recur across both programs. Learning them once makes the rest of the
course easier.

### 1. Write beside, then rename

Whenever a file is read by someone else while it may be rewritten, it is
written to a sibling (`.partial`, `.part`) and `rename`d into place.
`rename` within one filesystem is atomic, so a reader sees either the old
file or the new one, never half of one.

- `assemble::write_atomic` for `space.bin`, `space.json`, `semantic_pca.bin`
- `catalog::build` for `catalog.db`
- `models::download` for the ONNX weights (also hash-checked before rename)
- `AudioCache::put` for cached excerpts (with a per-process, per-thread
  temp name)
- `Remote::sync_space` on the client for every synced file

`space.json` is deliberately written *last* in `assemble::build`, so a client
that sees a new manifest also finds the new vectors.

### 2. `OnceLock` globals for "one per process"

The client has a few global singletons:

- `ENGINE: OnceLock<Mutex<Engine>>` (`app/src/lib.rs`): the loaded space
- `BACKEND: OnceLock<Backend>` (`app/src/backend/mod.rs`): the HTTP client
- `DATA_DIR`, `DB_PATH` (`app/src/app.rs`)

They are globals because every panel needs them and Dioxus event handlers
must be `Copy`, capturing an `Arc` in every closure would be noisy. The
cost: a `OnceLock` can be set once, which is why changing the server address
in Settings asks for a restart (chapter 14).

On the server, `Hub::client` is a `OnceLock` too, so a missing `.env` only
fails when something actually reaches for Qobuz.

### 3. Cancellation by shared flag

Long jobs take an `Arc<AtomicBool>` and check it between units of work:

- `pipeline::Job::cancelled()`: analyse checks between tracks, layout every
  25 epochs, the model download between chunks
- `CrawlJob::stop`: the crawl loop checks between frontier items

Nothing is forcibly killed. "Stop" means "finish the current unit, then
return". Everything already committed stays committed, which is also why
every stage is resumable.

### 4. `!Send` futures on their own thread

Diesel's `SqliteConnection` is blocking, and the crawl and analyse futures
hold one *across* `.await` points. Such a future must not run on a worker of
the shared multi-threaded runtime (it would block that worker, and it is not
`Send`). The server's answer, in `hub.rs`:

```rust
async fn off_thread<T, F, Fut>(job: F) -> Result<T>
where F: FnOnce() -> Fut + Send + 'static, Fut: Future<Output = Result<T>>, T: Send + 'static
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(job())
    }).await?
}
```

The *closure* is `Send`; the future it produces is built and driven entirely
on a blocking-pool thread with its own single-threaded runtime. You will see
this in chapter 10.

### 5. One shared budget, many clients

Qobuz is paid for by one account, so the rate limit belongs to the account,
not to a connection. `qobuz::RateLimit` is an `Arc<Mutex<TokenBucket>>` that
several `QobuzClient`s can share (`QobuzClient::sharing`). The crawl thread,
the analyse stage and every browsing request each have their own client, all
drawing on one bucket. Chapter 3.

### 6. Filter, don't delete

Blocking an artist inserts into `blocked_artists`; it does not remove
anything. Every reader filters: the crawl (`is_blocked`), analyse and
build-space (`db::not_blocked()`), the client (`Catalog::is_blocked`,
`Navigator::mask_blocked`). That makes a block instant and reversible.
`block --purge` is the one destructive path, and it is CLI-only.

### 7. Ask the question the way its owner asks it

`api::Corpus` counts are each computed with the *same query* the relevant
stage uses (`stages::corpus`, `analyse::pending_count`). The comment on
`Corpus` warns that deriving them arithmetically is wrong because blocked
artists make the populations differ. This "one definition" idea shows up
again in `db::not_blocked()`, which exists so "blocked" cannot drift
between stages.

## Check yourself

1. What does the server lose by depending on the app with
   `default-features = false`, and why is that desirable?
2. Why does `assemble::build` write `space.json` after `space.bin`?
3. What does "stop" do to an `analyse` run that is halfway through a track?
4. Why can't the crawl future simply be `tokio::spawn`ed?
5. Why does the Settings screen ask you to restart after changing servers?

## Exercises

1. Run `cargo tree -e features -i dioxus` in `server/`. Confirm Dioxus is
   not in the tree.
2. Grep both crates for `.partial` and `.part`. List every atomic write and
   what reads the file it protects.
3. Find every call to `job.cancelled()` in `server/src/pipeline/`. For each,
   say how much work can still happen after "stop" is pressed.
