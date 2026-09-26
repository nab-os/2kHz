# 19. Patterns, wrinkles and exercises

This last chapter steps back. First, the ideas that make the codebase hang
together, so you can extend it in its own style. Then an honest list of the
rough edges found while writing this course, each checked against the code
at `eaa1322`, with where to look. Finally, some larger projects that would
take you through several chapters at once.

## The ideas that recur

**Compute where the data is, sync the rest.** The expensive things (Qobuz,
the audio, CLAP) live on one server; the cheap, latency-sensitive thing
(walking the space) lives on every client. Only the text tower crosses,
because its input is a phrase and its output is 512 floats. When you add a
feature, ask which side the latency budget belongs to.

**Store what cannot be recomputed; derive the rest.** `features` keeps the
CLAP embedding and raw descriptors, nothing more. Moods, styles, PCA,
normalisation and weights are all build-time or query-time. A label change is
seconds of `build-space`, never hours of `analyse`.

**One definition per question.** `db::not_blocked()`, `analyse::pending_count`,
`stages::corpus`, `Stage::rebuilds_space`, `RemoteTrack::identity`: each
question has exactly one function that answers it, and every caller asks it.
The comments are explicit that deriving counts arithmetically would be wrong.

**No second implementation.** The CLI and the pipeline view run the same
stage functions (`stages::run` is a `match`). The routes are thin. The server
and client share wire types by depending on one crate.

**Filter, don't delete; stop, don't kill.** Blocking inserts a row everyone
filters on. Stopping sets a flag everyone checks between units of work.
Both are reversible or resumable because nothing is torn down mid-flight.

**Atomic handoffs.** Every file someone else reads is written beside and
renamed. `space.json` goes last. `catalog.db` is `VACUUM`ed to a single file
before it is renamed.

**Determinism.** Fixed seeds in UMAP, sign-fixed PCA components, sorted edge
lists, total-order comparators with index tie-breaks in `top_k` and
`argsort_desc`, deterministic radio walks. The same inputs give the same
map, the same neighbours, the same walk.

**Comments that carry the why, and the measurement.** Most non-obvious
lines say what bug or number motivated them ("~300ms a frame", "14.8
distinct artists per 20", "row N opened row N+1"). Keep doing this: it is
why a course like this one could be written from the code.

**Signals are addressed, not captured.** UI rows are named by an index into
a list that is re-read at click time, or by a track id, never by a captured
copy that may have gone stale. Where an index is used, the list it indexes
is computed by one function (`visible_tracks`) for both rendering and
handling.

## Known wrinkles

None of these is a crash. Most are small. They are listed so you can fix
them, or at least not be surprised by them.

### Behaviour

1. **Weights reset under the sliders after a resync.** The generation effect
   in `Shell` (`app/src/app.rs`) calls `reload_engine`, which builds the
   navigator with the manifest's *default* weights. Nothing resets the
   `Weights` context signal, so after a rebuild the sliders (and their
   "changed" badge) can show custom values while the engine uses defaults.
   Fix: in the effect, either re-apply `weights()` with `set_weights`, or
   reset the signal to `space.default_weights()`.

2. **Narrow blocks lose their magnitude** (chapter 8). Row L2 normalisation
   turns the 1-column era block into -1/0/+1 (before/after the mean year),
   reduces tempo to an angle, and undoes `key_coordinates`' "strength as
   radius". Options: skip row normalisation for blocks with fewer than ~4
   columns and instead divide by √(width) so their expected norm is 1; or
   give era a small basis expansion (e.g. decade one-hot, or sin/cos of a
   year scaled onto a circle) so it has a direction to normalise. Measure
   with `evaluate` before and after.

3. **A packaged server finds its data relative to the build machine**
   (chapter 17). `Paths::from_env` falls back to
   `env!("CARGO_MANIFEST_DIR")`'s parent. Fine in a checkout and in Docker
   (which sets every `TWO_KHZ_*` variable); surprising for the `.deb` and
   tarball. Fix: fall back to an XDG or `/var/lib` location outside debug
   builds.

4. **A refused log stream is retried at the poll rate.** When
   `/api/pipeline/log` answers non-2xx, `Remote::ensure_streaming` pushes
   "log stream refused", clears its `streaming` flag and returns; the
   pipeline view calls `pipeline_log` every 200ms, which restarts it. With a
   revoked token that is five requests and five log lines a second. Fix: keep
   the backoff on refusal, or remember the refusal until the token changes.

5. **Revoke refreshes too early.** In `Devices` (`ui/pipeline.rs`), the
   revoke button spawns the revoke and then immediately calls `refresh()`,
   which may re-list the devices before the revoke lands. Fix: refresh after
   the `await`.

6. **`graph_path` can find no route.** The kNN graph is directed and not
   symmetrised (chapter 13), so a track that is no one's near neighbour
   cannot be reached as B. The UI then says "nothing found". Symmetrising
   (as UMAP does) would fix most cases.

7. **Browsing requests queue behind each other on the server.** Every
   browsing call locks the Hub's one `tokio::sync::Mutex<QobuzClient>` for
   the whole request, so a slow search delays a stream URL for the same
   user. The rate limit would serialise the *requests* anyway, but not the
   response time. Fine for one user; a pool of clients sharing the
   `RateLimit` would decouple them.

8. **The sync manifest hashes every file on every request.**
   `routes::sync_manifest` reads `catalog.db` and `space.bin` into memory to
   md5 them each time a client asks, and `sync_space` hashes the local
   copies too. Cheap at today's sizes and cadence; cache by
   `(size, mtime)` if it ever is not.

9. **Blocking is by artist id only.** Tracks from `album/get` are filed
   under the *album* artist (`crawl::expand_album`), and featured or
   compilation credits can sit under other ids. The README already says to
   block those ids too.

10. **The shared budget is shared per process.** `RateLimit` is an
    in-memory `Arc`, so it covers everything *inside* `serve`, the
    background crawl, stages started from a client, and every browsing
    request. But a stage run from the command line (`two-khz-server analyse`,
    `crawl`) is a separate process with its own bucket: running one while
    `serve` is busy can spend up to 2/s + 2/s against the one account.
    (`docker exec … analyse` is a separate process too.) While the server is
    running, prefer starting Qobuz-touching stages from the pipeline view;
    `build-space` and `layout` make no Qobuz calls and are safe from the
    shell. Relatedly,
    `rate_per_sec` lives on each client while the bucket is shared, so two
    clients with different rates would each refill it at their own rate;
    only `crawl --rate` changes it today.

### Documentation drift

11. `compose.yaml` pins `4gjr3z1t/2khz:v0.6.1` while the crates are 0.7.0.
12. `server/src/routes.rs` says the slim catalogue is "built by
    `two-khz-server sync-catalog`"; the command is `build-catalog`.
13. `server/src/auth.rs` says "there is no unauthed path by construction";
    `/api/health` is the one, deliberate, exception (its own comment says
    so).
14. `app/src/ui/queue.rs`'s module doc says reordering is "by up/down rather
    than drag"; `queue-drag.js` and the grip now provide drag too.
15. `app/src/ui/generate.rs` keeps ISRC dedupe out of `paths.rs` because
    `paths.rs` "mirrors `pipeline/two_khz/paths.py`, the parity oracle".
    The Python pipeline has been removed, so the reason no longer holds (the
    placement is still fine).
16. Several comments still speak of a "local" backend, left from when the
    app could run the pipeline itself: `ui/mod.rs` ("cannot tell local from
    remote"), `api.rs` (`LogSlice`: "locally the lines come off a subprocess
    reader"), `ui/crawler.rs` ("its own thread locally"), `ui/sheet.rs`
    ("Locally that is an exported ONNX file"). The `Backend = Remote`
    alias is the same fossil.

## Projects

Larger pieces of work, roughly in order of how many chapters they touch.

1. **Fix wrinkles 1, 4 and 5.** Small, UI-side, good first changes
   (chapters 14 to 15).

2. **A symmetric kNN graph with cached reverse edges** (chapter 13). Measure
   empty-path frequency on the demo corpus before and after.

3. **Rethink narrow-block normalisation** (wrinkle 2, chapter 8). Implement
   one option, bump `SPACE_VERSION`, and compare `evaluate` numbers and the
   feel of radio walks.

4. **A new mood axis end to end** (chapters 8, 12, 15). Add it in
   `labels.rs`, rebuild, and confirm it appears in the manifest; then add a
   UI that shows a track's eight mood scores in the detail sheet (the client
   has the space row; the block layout tells you where mood lives, but the
   values there are *normalised*, so decide what you want to show).

5. **Route tests** (chapters 11, 18). Build the Axum router over a scratch
   `AuthStore` and cover 401/403/200 for every scope boundary.

6. **Proxy mode for playback** (chapters 3, 11). design.md lists it as the
   open question: if Qobuz ties signed URLs to the requesting IP, the server
   must proxy audio. Add an optional `/api/tracks/{id}/stream` that pipes
   the CDN response through with `Range` support, and a client setting to
   use it.

7. **Background playback on Android** (chapter 16). design.md: the
   `<audio>` element is throttled when backgrounded and has no
   MediaSession service. A native `MediaSessionService` fed the signed URL,
   with Rust keeping only the queue, is the shape it suggests.

8. **A web client** (chapters 1, 12). design.md's "Web" section: every
   dependency compiles for wasm, but there is no filesystem to `mmap` or
   open SQLite from. It sketches the work: fetch the space into memory
   instead of mapping it, and ship the catalogue as a flat buffer instead of
   SQLite.

## Where to go from here

- Read `docs/design.md` again now; every measurement in it will map onto
  code you have seen.
- Read `docs/ux-rethink.md` for how the UI got its current shape, and which
  parts nobody has yet looked at on a real phone.
- Pick a project, build the demo corpus, and change something.
