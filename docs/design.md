# Design notes

Why things are the way they are. The [README](../README.md) covers how to run
it; this covers the reasoning and the measurements behind it.

## Why the audio has to be analysed

Qobuz's API returns metadata only: genre, label, release date, performers.
There is no BPM, no key, no energy. Spotify's `/audio-features`, which everyone
used to build this kind of thing, was removed in Nov 2024, and AcousticBrainz has
been frozen since Jul 2022. So the acoustic half of every vector is computed
from the audio itself, which a Qobuz subscription makes possible via
`track/getFileUrl`.

## Extract once, extract generously

Audio excerpts are discarded after analysis, so the first pass stores
everything: raw descriptors, the full 512-d CLAP embedding, the 400 style
activations, and the 1280-d EffNet embedding. Any future classification head can
be run later without re-downloading anything. Rebuilding the space after a
weight change costs seconds and zero network.

## Why crawling moved to Rust

It is pure HTTP and SQLite (no Essentia, no numpy) and this way the app can
extend the catalogue itself instead of queueing a row and waiting for a CLI run.
`crawl.py` still works and writes the same tables; either can resume what the
other started.

Python keeps what only Python can do: Essentia's descriptors and classifier
heads, CLAP, and UMAP.

## The space

| block | dims | source |
|---|---|---|
| tempo | 2 | folded `log2(bpm)`, onset rate |
| key | 3 | circle-of-fifths position, scaled by detection strength |
| dynamics | 3 | loudness, dynamic complexity, loudness range |
| timbre | 4 | spectral centroid, rolloff, flatness, zero-crossing rate |
| mood | 8 | danceability, happy/sad/aggressive/relaxed/party, approachability, engagement |
| style | 20 | 400 Discogs style activations → PCA |
| era | 1 | release year |
| semantic | 40 | CLAP audio embedding → PCA |

Each block is z-scored per column and L2-normalised per row **before** being
written out. Both halves matter: column z-scoring stops one wide-range
descriptor swamping its neighbours, and row normalisation stops the 40-d
semantic block silently dominating the 2-d tempo block regardless of any weight
you set.

Weights are applied in Rust at query time, which is what makes the sliders live.

Two decisions worth knowing about:

- **Tempo is folded into one octave.** Drum & bass detected at 86 instead of 172
  is a well-known Essentia failure. Folding makes the metric immune to it rather
  than patching it per genre. Raw BPM is still stored for display.
- **`approachability` and `engagement` stand in for arousal/valence.** Essentia's
  emomusic head has no Discogs-EffNet variant, and using its musicnn version
  would force a second embedding pass per track.

**Checkpoint choice.** `laion/clap-htsat-unfused`, deliberately not
`laion/larger_clap_music`, the latter's text tower returns near-constant
embeddings (mean pairwise cosine ≈ 0.999), which would destroy text steering.

## How fast `analyse` goes

Extraction costs about **19 core-seconds per track** (Essentia 7.8, CLAP 10.8),
so it is worth spreading over the machine. `analyse` runs two pools: excerpt
fetching on threads in the main process, sharing the one rate-limited client,
and extraction in worker processes, because Essentia holds a TensorFlow session
and CLAP a torch model and neither likes being driven from several threads.
Only the main process writes to SQLite.

```sh
uv run two-khz analyse                        # one worker per 4 hardware threads
uv run two-khz analyse --workers 12           # override
uv run two-khz analyse --download-workers 16  # if fetching is the laggard
```

Measured on a 32-thread Ryzen 9 9950X3D: **900 → 4,700 tracks/hour**. Past that
the workers start competing with ffmpeg for cores, so more is not better, 8
workers beat 12 and 16 in the full pipeline even though 12 wins on extraction
alone. The ceiling above all of this is Qobuz's rate limit, ~7,200 tracks/hour
at the default 2 req/s, which is why throwing a cloud at the problem buys very
little.

**Analysis uses MP3 320, not FLAC.** Roughly 10× less bandwidth, and both
Essentia and CLAP resample to 16 to 48 kHz anyway. FLAC is reserved for listening.

## Crawl ordering

The frontier is ordered artists-before-albums within a hop, so the early phase of
a large crawl discovers thousands of artists before the track count moves. That
is working as intended, not a stall.

In-app crawling is stepped one frontier item at a time, and the Qobuz client is
handed back between steps, so searching and playback keep working throughout.
Measured: searches returned in 0.2 to 2.4s with a crawl running.

## Artist pull

Without it a radio walk tends to sit inside one discography, because a prolific
artist occupies a tight cluster: 14.8 distinct artists per 20 tracks. The
penalty is subtracted from a candidate's similarity once per time that artist
already appears in the walk, so the push outward grows the longer you stay. At
the default 0.10 that is 19.4 distinct artists per 20, and it costs almost
nothing in similarity between jumps, 0.786 against 0.796.

It has to reach back over the whole walk rather than a few steps: the path
constraints already forbid an artist repeating within three, so a penalty over
that same window measurably does nothing at any strength.

## Stopping a stage

**Stop** takes effect within about 200ms, even mid-`layout` when UMAP has been
silent for minutes, and it signals the stage's whole process group, `uv run`
spawns the CLI, which spawns its own analysis workers, and killing only the
first would leave the real work running.

Closing the app does the same, so a locally started stage never outlives the
window. The exception is SIGKILL, which runs no cleanup anywhere; the stage
usually still dies when the app's pipes close, but that is luck rather than
design.

A stage started from a *client* deliberately does outlive it, that is the point
of starting `analyse` from a phone and walking away.

## Testing

```sh
cd pipeline
uv run python scripts/smoke_test.py    # synthetic corpus, end to end
uv run python scripts/parity_test.py   # Rust vs Python, exact comparison
```

Neither needs Qobuz credentials: both synthesise audio with known properties and
check the space recovers it.

The parity test matters more than it looks. The Python path functions are the
oracle the space was validated against; the Rust port is what users actually
drive. It has already caught one real bug, the two sides were building
waypoints from differently scaled vectors, which silently produced different
drift paths.

## Not yet verified

Whether Qobuz's signed file URLs are IP-bound. `track/getFileUrl` returns an
opaque CDN URL and whether they tie it to the requesting address is their
policy, not ours. If they do, playback has to proxy through the server and the
bandwidth story changes. Ten-minute test: mint a URL on the server, then
`curl -r 0-1000` it from a host on another network.

## Android: the open questions

- **The layout is still desktop-shaped.** Three columns and a canvas map do not
  belong on a 1080px-wide screen; it is usable but not designed. The panels are
  ordinary flexbox, so this is CSS work, not architecture.
- **Background playback.** Audio is an `<audio>` element in a WebView, which
  Android throttles when backgrounded, with no MediaSession, lockscreen controls
  or audio focus. For a music app that is the product, not a rough edge, it
  wants a native `MediaSessionService` fed the signed URL, with Rust keeping
  only the queue.
- **The applicationId must differ from `dev.dioxus.main`.** The CLI emits
  `typealias BuildConfig = <applicationId>.BuildConfig` into a file that is
  itself in `package dev.dioxus.main`, so reusing the id makes that typealias
  refer to itself and Kotlin fails to compile.

## Web

Not working, and honestly characterised rather than promised.

`cargo check --target wasm32-unknown-unknown --features web` gets further than
expected: **every dependency compiles, including rusqlite and memmap2.**

Compiling is not the hard part. The hard part is that both of those crates
compile and then cannot *work*: there is no filesystem to `std::fs::read` a
`catalog.db` from and nothing to `mmap`. A real web client needs the space
fetched into memory rather than mapped, and the catalogue served as a flat
buffer instead of SQLite, which would also simplify Android. That is a week-ish
of work on the data layer, not an afternoon of cfg attributes.

## Licensing

Essentia's pretrained weights are CC BY-NC-SA 4.0, fine personally, blocking
for anything commercial. CLAP is the permissive one.
