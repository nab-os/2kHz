# 12. The client core

By the end of this chapter you will know how the app finds its server, where
it keeps its files, how it loads the space into memory, how weights are
applied, and what the `Engine` global holds.

Files: `app/src/lib.rs`, `app/src/space.rs`, `app/src/db.rs`,
`app/src/backend/`, and `app/src/app.rs` (`bootstrap`).

## Startup: `bootstrap`

`main.rs` calls `two_khz::app::bootstrap()` before opening the window:

```rust
pub fn bootstrap() -> anyhow::Result<()> {
    let wiring = Wiring::from_env()?;          // which server, which token
    DATA_DIR.set(wiring.data_dir()); DB_PATH.set(wiring.db_path());
    backend::init(wiring.into_backend());      // the global HTTP client
    runtime.block_on(backend().sync_space())?; // download the space
    crate::init_engine(&data_dir, &db_path)    // map and load it
}
```

It is fallible but **not fatal**: if there is no pairing, or the server does
not answer, `main` prints the reason (desktop) and still launches. The root
component sees `engine_ready() == false` and shows the setup screen
(chapter 14).

Sync happens *before* the engine loads because the engine memory-maps
`space.bin` and will not look again until told to.

## Finding the server: `Wiring`

`Wiring::from_env()`:

1. `TWO_KHZ_SERVER` set → it also needs `TWO_KHZ_TOKEN`, or it fails with a
   message telling you how to pair (using this machine's hostname);
2. otherwise `ServerConfig::load()`, `server.json` in the client data dir,
   written by the setup screen;
3. otherwise "not paired with a server yet".

`ServerConfig { base, token }` has `save`, `load` and `clear` (where
"already absent" counts as success).

## Where the client keeps things: `client_data_dir`

In priority order:

1. `set_data_dir(dir)`: an override for platform entry points;
2. `TWO_KHZ_CLIENT_DIR`;
3. on Android, `android_files_dir()`;
4. `$XDG_DATA_HOME/two_khz`, else `$HOME/.local/share/two_khz`.

It is **never** the server's `TWO_KHZ_DATA_DIR`, with both on one machine,
syncing into the server's directory would overwrite the corpus it was
syncing from.

`android_files_dir` is a neat trick. Android sets no `HOME`, so the XDG
fallback would land on `/` and every write would fail. Instead it reads
`/proc/self/cmdline`, whose first NUL-separated field is the package name,
and builds `/data/user/0/<package>/files/two_khz` (falling back to
`/data/data/…`). No JNI needed. `/data/user/0` is preferred because
`/data/data` is a compatibility symlink that does not exist for secondary
users or work profiles.

## The backend: `Remote`

`backend/mod.rs` declares `pub type Backend = Remote;` and a
`BACKEND: OnceLock<Backend>`. `backend()` returns `&'static Backend`,
usable from any event handler without cloning.

`Remote` (`backend/remote.rs`) is a thin typed wrapper over the routes of
chapter 11: `get<T>`, `post<B, T>`, `check`, plus one method per route. Two
details:

- `urlencode` is a tiny percent-encoder for the characters that turn up in
  search phrases and album ids, not a general one;
- the log is mirrored locally via SSE (chapter 11), and `sync_space` writes
  into `data_dir`.

There is only one backend implementation now. The name `Backend` and the
comments about "local vs remote" are fossils from when the app could also
run the pipeline in-process.

## The engine

```rust
static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

pub struct Engine {
    pub data_dir: PathBuf,
    pub space: space::Space,          // the raw, memory-mapped vectors
    pub navigator: paths::Navigator,  // weighted view + catalogue + kNN cache
}
```

- `init_engine` loads once at startup (or after the first pairing);
- `reload_engine` swaps in a freshly loaded one after a sync;
- `engine()` returns the `&'static Mutex<Engine>`; the UI locks it briefly
  for every query.

`Engine::load`:

```rust
let space = Space::load(data_dir)?;
let catalog = db::Catalog::load(db_path, &space.manifest.track_ids)?;
let navigator = paths::Navigator::new(&space, &space.default_weights(), catalog);
```

Other methods:

- `set_weights(weights)` rebuilds the `Navigator` with a new weighted view,
  moving the catalogue across with `mem::take` rather than reloading it.
  Milliseconds, no I/O.
- `set_blocked(ids)` replaces the blocked set and **drops the cached kNN
  graph**, because its edges were chosen over the old set of visible tracks.
  It takes ids rather than reading a database because the server is the
  authority.
- `has_audio_embeddings()`: whether `catalog.db` carried CLAP vectors (half
  of "can we drift"; the server answers the other half).

## Loading the space: `space.rs`

`Space::load(data_dir)`:

1. parse `space.json` into a `Manifest`;
2. refuse a `version` other than `SPACE_VERSION` ("space-2"), with a
   message saying to rebuild on the server;
3. check the blocks cover exactly `n_dims`;
4. **memory-map** `space.bin` with `memmap2` (an `unsafe` call, justified in
   a comment: the file is written once and only read here, and the sync
   replaces it by rename, so a mapped old inode stays valid);
5. check its length is exactly `n_tracks × n_dims × 4`;
6. build `index_of: HashMap<track_id, row>`.

`raw()` reinterprets the mapped bytes as `&[f32]` with `bytemuck`, zero
copy. (This assumes a little-endian host, which every target here is.)

### Applying weights: `weighted`

```rust
pub fn weighted(&self, weights: &HashMap<String, f32>) -> WeightedSpace {
    // scale[k] = weight of the block containing dimension k
    // for each row: v[k] = raw[k] * scale[k]; then divide by ‖v‖
}
```

The result, `WeightedSpace { unit, n_tracks, n_dims, scale }`, is an owned,
row-normalised copy (~9MB for 28k × 81). Because every row is unit length,
**a cosine similarity is one dot product**.

`similarities(query)` dots the query against every row (dividing by the
query's norm), returning a score per track. `similarities_into` does the same
into a caller-owned buffer, with deliberately identical arithmetic, a
different summation order could reorder near-ties.

### Picking the best: `top_k` and `argsort_desc`

`top_k(scores, k)` uses `select_nth_unstable_by` to partition the top *k* in
O(n), then sorts just those. The comparator is a *total order*: score
descending, then index ascending as a tie-break. That makes the result
exactly the prefix a full sort would give, and makes every ranking
**deterministic** from run to run. `argsort_desc` is the full sort, with the
same comparator.

## The catalogue: `db.rs`

Covered in chapter 2: `Catalog::load` returns `TrackMeta` rows **in space
row order**, the CLAP matrix in the same order, and the blocked artist set.
Helpers:

- `is_blocked(i)`, `visible()`: the client-side filter;
- `id_set()`: visible track ids, for "is this Qobuz result in my space?";
- `reach()`: albums and artists with at least one visible track, for the
  in-space dot on album and artist tiles.

`TrackMeta` carries `x`, `y` from the layout, the BPM parsed out of the stub
`descriptors_json`, and the ISRC.

## The map payload: `map.rs`

Not drawn here, but prepared here. `meta(catalog)` returns JSON (count,
genre names, one "Artist - Title" label per point, bounds). `payload(catalog)`
returns one binary blob laid out for zero-copy typed arrays in JS:

```
offset 0     Float64Array(n)   track ids   (f64 keeps JS numbers exact)
offset 8n    Float32Array(2n)  x, y pairs
offset 16n   Uint16Array(n)    genre index
```

Only visible, laid-out tracks are included. Chapter 16 shows how it reaches
the canvas.

## Check yourself

1. Why is a failed `bootstrap` not fatal?
2. Why must the client data dir never be the server's data dir?
3. What does memory-mapping `space.bin` save, and why is it safe here?
4. Why does `set_blocked` drop the kNN graph but `set_weights` does not need
   to do so explicitly?
5. What guarantees that two runs of `neighbours` return the same order for
   tied scores?

## Exercises

1. Run the app with `TWO_KHZ_CLIENT_DIR=/tmp/khz-client` and list what sync
   put there.
2. Write a test for `LogBuffer::since` that pushes 500 lines and checks a
   cursor taken at line 50 returns exactly the retained lines after it.
3. Measure `Space::weighted` on the demo corpus and on a synthetic 50k × 81
   space. Compare with the "few milliseconds" in its doc comment.
