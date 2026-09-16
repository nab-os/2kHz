# Qobuz suggestion system

Music as a navigable space. Every track becomes a point in ~80 dimensions where
distance approximates perceptual similarity, so recommendation becomes geometry:
the nearest neighbours of a track, a path that morphs from A to B, or a drift in
a direction you describe in words.

Qobuz's API returns metadata only (no BPM, no key, no energy), so the acoustic
half of every vector is computed from the audio itself.

## Shape of the system

```
  Python: analysis + layout                Rust: crawler + desktop app
  ─────────────────────────                ───────────────────────────
                                           favourites ─┐
              catalog ◄──────────────────              ├─► crawl ──► catalog
                 │                         similar   ──┘
                 ▼                                        rusqlite + memmap
         fetch 90s excerpt                                neighbours/paths/drift
                 │                                        ort: CLAP text tower
          [LRU cache, capped]                             library browse, search,
                 ▼                                        playback, playlists
   Essentia + CLAP extraction                                     │
                 │                                                │ binary over
                 ▼                                                │ custom protocol
       assemble space ──────────────►  data/qsuggest.db           ▼
                 │                     data/space.bin      webview: canvas map,
                 ▼                     data/space.json     controls, <audio>
         UMAP 2D layout ────────────►  layout
```

Python does the modelling (Essentia, CLAP, UMAP). Rust does I/O and interaction.
The two meet at files rather than a socket: both read and write `qsuggest.db`,
and Python writes the vector file that Rust memory-maps. The schema is
`schema.sql` at the repo root, executed by both halves, so they cannot drift.

Design notes, measurements and the space layout are in
[docs/design.md](docs/design.md).

## Setup

Requires `ffmpeg`, `uv`, and a Rust toolchain.

```sh
cd pipeline
uv sync --extra extract --extra layout
```

### Credentials

Qobuz does not issue API credentials to individuals, and since April 2026 its
API no longer serves anonymous requests, so sign-in has to happen in a browser:

```sh
cd pipeline
uv run qsuggest login
```

That scrapes the current app id and signing secrets from the web player, opens
your browser to Qobuz, and writes all three into `.env`.

If the browser round-trip is awkward, log in at <https://play.qobuz.com/>, open
devtools → Application → Local Storage → `play.qobuz.com`, copy the
`localuser.token` value, and set:

```sh
QOBUZ_USER_AUTH_TOKEN=<the token>
```

Tokens expire when the session ends. The app id and secrets rotate with web
player releases; `qsuggest refresh-credentials --write` re-scrapes those alone.

## Running the pipeline

```sh
cd app     && cargo run --release --bin crawl -- --max-tracks 2000
cd pipeline
uv run qsuggest whoami                       # verify credentials
uv run qsuggest analyse                      # download excerpts, extract features
uv run qsuggest build-space                  # assemble vectors
uv run qsuggest layout                       # UMAP projection for the map
uv run qsuggest evaluate                     # space sanity check
```

Every stage is resumable: the crawl frontier lives in SQLite and analysis skips
what it has already done, so Ctrl-C and rerun is always safe.

For text steering, export the CLAP text tower once, 479MB, lands in
`data/models/`:

```sh
uv run python -m qsuggest.features.onnx_export
```

### Crawling

Seeds from your favourites, then expands outward through
`artist/getSimilarArtists`.

```sh
cd app
cargo run --release --bin crawl -- --max-tracks 5000   # favourites, then similar
cargo run --release --bin crawl -- --no-seed           # resume the frontier only
cargo run --release --bin crawl -- --artist 43840      # one discography, queued
cargo run --release --bin crawl -- --album 0634904077969
```

Requests are limited to 2/s with retry-and-backoff on 429s, one account pays
for every call. `uv run qsuggest crawl` still exists and writes the same tables.

The app can crawl while you use it, from the **Pipeline** view. Both halves
share the one rate limit, so browsing during a crawl is slower, not blocked.

## Running the app

```sh
cd app
cargo run --release
```

Two views, switched from the header, over a transport bar that stays put.

**Explore** is three columns: Qobuz live on the left, the map in the middle, the
analysed space on the right. Clicking a track plays it and makes the list you
clicked in the queue. **in space** selects an analysed track on the map;
**fetch** pulls an album's tracklist, or an artist's discography, into the
catalogue.

Generation is one panel with four modes:

| mode | what it does |
|---|---|
| neighbours | the *k* most similar tracks; follows the selection as you browse |
| radio | a greedy walk, each jump to the nearest track not yet played |
| path | A to B, either the shortest route or an evenly paced interpolation |
| drift | away from the selection, towards a phrase |

Radio has an *artist pull* slider, which stops a walk sitting inside one
discography. Map positions are fixed until `layout` is re-run, the weight
sliders change which tracks are near each other, not where the dots sit.

**Pipeline** runs the same four stages as the CLI and streams their output into
a log. Stop signals the stage's whole process group, and closing the app does
the same, so a locally started stage never outlives the window.

**Playback** offers MP3 320, FLAC 16/44 or FLAC hi-res. It needs credentials;
browsing the space does not.

To try the app without a real corpus:

```sh
cd pipeline && uv run python scripts/make_demo.py /tmp/qsuggest-demo
cd ../app && QSUGGEST_DATA_DIR=/tmp/qsuggest-demo/data cargo run
```

## Hiding an artist

`blocked_artists` is a table both halves honour: the pipeline will not crawl
them, follow them to their similar artists, analyse their tracks or place them
in the space, and the app hides what is already stored and refuses to play it.

```sh
uv run qsuggest block "artist name"       # or an artist id
uv run qsuggest block 224109 --reason "why"
uv run qsuggest blocked                   # list
uv run qsuggest unblock 224109
```

Or click **hide** on any artist or track in the app. The **Hidden** panel at the
bottom of the Qobuz column lists them with a way back.

Hiding filters rather than deletes, which is what lets a block take effect
immediately and be undone. `block --purge` deletes the tracks, features and
albums instead; re-run `build-space` and `layout` afterwards.

The match is on `artists.id`, so a featured credit that Qobuz files under a
different artist id can still surface, block those ids too. An ambiguous name
refuses to act rather than guessing.
