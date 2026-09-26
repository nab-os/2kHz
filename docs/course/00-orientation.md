# 0. Orientation

By the end of this chapter you will know what 2kHz is for, what its two
programs are, what files pass between them, and how one track travels from
"a song you liked on Qobuz" to "a dot on a map you can walk from".

## The idea

Most music recommendation is a black box: a service decides what you might
like. 2kHz makes it geometry instead. Every analysed track becomes a point in
an ~81-dimensional space where **distance approximates perceptual
similarity**. Once that space exists, recommendation is just asking
geometric questions:

| question | geometric answer | mode in the app |
|---|---|---|
| "more like this" | the *k* nearest points | **neighbours** |
| "keep going" | a greedy walk to the nearest unvisited point | **radio** |
| "get me from A to B" | a shortest path through a neighbour graph, or a straight line with stops | **path** |
| "like this, but more *melancholic*" | a line from here towards the region that matches a phrase | **drift** |

Qobuz, the streaming service 2kHz is built on, gives metadata only: titles,
genres, labels. It gives no tempo, no key, no energy. So 2kHz **listens to
the audio itself**: it downloads the middle 90 seconds of each track, runs it
through a neural audio model (CLAP) and some classic signal processing, and
turns the result into a vector. That is the expensive part, and it is why the
system is split in two.

## Two programs

```
  two-khz-server  (one Rust binary)            two-khz-app  (desktop / Android)
  ────────────────────────────────             ────────────────────────────────
  holds the Qobuz credentials                  holds a device token, nothing else
  one shared 2 req/s rate limit                syncs space.bin, space.json,
  SQLite corpus (two_khz.db)                     semantic_pca.bin, catalog.db
  crawl → analyse → build-space → layout       answers every navigation query
  CLAP audio + text towers (ONNX)                in-process, in microseconds
  HTTP API on :7700 + SSE for logs             draws the map, plays audio
           │                                                ▲
           └──── /api/… (search, stream URLs, sync, embed) ─┘
                        audio never passes through the server:
                        the app streams straight from Qobuz's CDN
```

**`two-khz-server`** (`server/`) is "everything but the screen". It owns the
Qobuz account, the database, the models and the whole pipeline. It also
serves an HTTP API. There is exactly one of these per user.

**`two-khz-app`** (`app/`) is a client. It never holds Qobuz credentials.
It pairs with a server using a per-device token, downloads a copy of the
built space, and then does all of the navigation *locally*. There can be
many of these (a desktop, a phone…).

A crucial design choice: **the space is synced, not queried.** A
nearest-neighbour lookup over 30k points takes microseconds in process. If
every slider move had to go over HTTP, the live feel of the weight sliders
(chapter 8, chapter 13) would be lost. So the client holds the vectors, and
only one space operation crosses the wire: turning a *phrase* into a CLAP
text embedding, because that model is ~500MB and its answer is 512 floats.

There is no "local mode". A desktop user runs the server on the same machine
and pairs with it like any other device.

## The shared crate

One subtlety in the Cargo layout is worth learning now. The app crate is
called `two-khz-app`, but its *library* target is named `two_khz`
(`app/Cargo.toml`, `[lib] name = "two_khz"`). The server depends on it with
default features turned off:

```toml
# server/Cargo.toml
two-khz-app = { path = "../app", default-features = false }
```

With the `gui` feature off, the app crate is just a library: wire types
(`api.rs`), the database schema (`schema.rs`), Qobuz item shapes
(`qobuz.rs`), the space loader (`space.rs`), the catalogue reader (`db.rs`),
the navigator (`paths.rs`) and a log buffer. The server uses these so the two
sides of every HTTP call agree on types by construction, and the server
never has to link a browser engine. Chapter 1 covers the features in detail.

## The files that matter

On the server, under `TWO_KHZ_DATA_DIR` (default `data/` in the checkout):

| file | written by | what it is |
|---|---|---|
| `two_khz.db` | every stage | the full SQLite corpus: artists, albums, tracks, features, frontier, layout, failures, blocked artists, devices |
| `space.bin` | `build-space` | `n_tracks × n_dims` little-endian `f32`, per-block normalised, **unweighted** |
| `space.json` | `build-space` | the manifest: track id order, block layout, default weights |
| `semantic_pca.bin` | `build-space` | the PCA used for the semantic block (mean + 40 components) |
| `catalog.db` | the server, automatically | a slim projection of `two_khz.db` for clients (~269MB → ~39MB) |
| `models/*.onnx` | first use / `models` | CLAP audio tower, text tower, tokenizer (~620MB) |
| `.env` | `login` | Qobuz app id, secrets, user token |

`cache/audio/` holds downloaded MP3 excerpts. It is disposable.

On a client, under its own data dir (e.g. `~/.local/share/two_khz`): copies
of `space.bin`, `space.json`, `semantic_pca.bin`, `catalog.db`, plus
`server.json` holding the server address and device token.

## The life of one track

Following one song end to end is the fastest way to see how the parts fit.
Each step names the chapter that explains it.

1. **You like a song on Qobuz.** Nothing happens in 2kHz yet.

2. **Crawl** (ch. 4). `crawl::seed` pulls your favourites through
   `favorite/getUserFavorites`, writes the track into `tracks` with
   `seed_distance = 0`, and pushes its artist onto the `frontier` table.
   Later, `crawl::step` pops that artist, fetches their discography and
   their Qobuz "similar artists", and enqueues those at distance 1. Their
   albums get expanded into more `tracks` rows. Everything is resumable,
   because the frontier lives in SQLite.

3. **Analyse** (ch. 5 to 7). `analyse::run` finds tracks with no `features`
   row. For ours, it asks Qobuz for a signed MP3 URL (`track/getFileUrl`,
   this is the only call that costs one of the account's 2 requests/second),
   then fetches *just the middle ~93 seconds* of the file from the CDN with an
   HTTP byte range, finds the first real MP3 frame, and decodes it to mono.
   A worker thread resamples it to 48kHz, runs the CLAP audio tower (→ a
   512-d embedding) and the descriptor code (→ BPM, key, loudness…), and the
   stage writes one `features` row: the descriptors as JSON, the embedding
   as a 2048-byte blob.

4. **Build the space** (ch. 8). `assemble::run` loads every analysed track,
   turns each into eight blocks: tempo, key, dynamics, timbre, mood, style,
   era, semantic, z-scores each column, unit-normalises each block per
   row, and writes `space.bin` and `space.json`. Mood and style are scored
   *from the stored CLAP embedding* against text phrases, so changing a label
   never needs re-analysis.

5. **Layout** (ch. 9). `layout::run` projects the space to 2D with a UMAP
   port and writes `(x, y)` into the `layout` table.

6. **The server notices** (ch. 10). Finishing `build-space` or `layout` bumps
   an in-memory `generation` counter. A background task in `main.rs`
   (`watch_generation`) sees the change and rebuilds `catalog.db`.

7. **The app syncs** (ch. 11 to 12). The app polls `/api/pipeline` every 200ms.
   When `generation` changes, it calls `Remote::sync_space`, which compares
   md5 digests from `/api/sync/manifest` and downloads whatever differs. Then
   `reload_engine` memory-maps the new `space.bin` and reads `catalog.db`.

8. **You search for it** (ch. 14 to 15). Typing filters the local catalogue
   instantly; after a pause, Qobuz is searched too. The track shows under
   "in your space".

9. **You play it** (ch. 15 to 16). The app asks the server for a stream URL
   (`/api/tracks/{id}/url`); the server signs a `track/getFileUrl` request
   and returns the CDN URL; the app hands it to an `<audio>` element in its
   webview. The audio bytes never touch the server.

10. **You walk from it** (ch. 13). "Start radio here" runs
    `Navigator::radio_nearest` in the app's own process: repeated
    nearest-unvisited-neighbour lookups over the weighted space, with a
    penalty for artists already used. The result lands in the sheet and can
    be drawn on the map.

## What to read next

[Chapter 1](01-toolkit.md) introduces the libraries and the patterns you will
see over and over. If you are impatient to see the interesting maths, you can
skip to [chapter 8](08-space.md) and come back.

## Check yourself

1. Why does the app hold its own copy of the vectors instead of asking the
   server for neighbours?
2. Which one space operation *does* go to the server, and why?
3. Which Qobuz call is rate-limited and paid for per analysed track? Which
   part of the download is free?
4. What is the difference between `two_khz.db` and `catalog.db`?
5. What does the server's `generation` counter tell a client?

## Exercises

1. Open `app/Cargo.toml` and `server/Cargo.toml` side by side. Find the line
   that makes the server depend on the app, and the comment explaining why
   they are *not* a Cargo workspace.
2. Trace step 7 in code: find where the app reads `generation`
   (`app/src/ui/pipeline.rs`), and where a change triggers a sync
   (`app/src/app.rs`, the first `use_effect` in `Shell`).
