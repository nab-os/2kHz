# 5. Getting audio: the `analyse` stage

By the end of this chapter you will understand how 2kHz gets 90 seconds of
audio out of the middle of a track without downloading the file or running
ffmpeg, how it turns MP3 bytes into samples, and how the stage keeps four
CPU workers busy while spending no more than 2 Qobuz requests per second.

Files: `server/src/pipeline/analyse.rs` (the stage), `server/src/pipeline/audio.rs`
(fetching, decoding, resampling, caching).

## Why the middle 90 seconds

Intros, outros and fades skew tempo and timbre statistics. So `audio.rs`
analyses a centred window:

```rust
pub const EXCERPT_SECONDS: f64 = 90.0;
pub fn excerpt_start(duration: Option<i64>) -> f64 {
    match duration {
        Some(d) if d as f64 > EXCERPT_SECONDS => ((d as f64 - EXCERPT_SECONDS) / 2.0).max(0.0),
        _ => 0.0,
    }
}
```

Tracks under 45 seconds (`MIN_TRACK_SECONDS`) are not worth the request, and
an excerpt that decodes to under 20 seconds (`MIN_EXCERPT_SECONDS`) is
rejected as noise.

## Why MP3 320, not FLAC

design.md: roughly **10× less bandwidth**, and CLAP resamples to 48kHz
anyway. But the decisive property is that MP3 320 is **constant bitrate**:
byte offset is proportional to time. That turns "the middle 90 seconds" into
a byte range, which the CDN serves directly. ~3.6MB per track.

## Fetching an excerpt: `fetch_excerpt`

```
GET <signed url>   Range: bytes=0-9
   → first 10 bytes (maybe an ID3v2 header)
   → Content-Range: bytes 0-9/<total>
GET <signed url>   Range: bytes=<audio_start+start>-<audio_start+end-1>
   → the slice
```

Step by step:

1. **Ask for 10 bytes.** They hold the ID3v2 header if there is one, and the
   `Content-Range` response header reveals the total file size.
2. **`id3_size`** parses the ID3 tag's size. ID3 sizes are *synchsafe*
   integers (7 bits per byte, the top bit always 0) so
   `size = b6<<21 | b7<<14 | b8<<7 | b9` (with the high bits masked). Add 10
   for the header and 10 more if the footer flag (`0x10` in byte 5) is set.
   The audio begins after the tag.
3. **`window`** converts seconds to bytes: `per_second = audio_len / duration`,
   start at `excerpt_start * per_second`, length `(90 + 3 slack) * per_second`.
   The 3 seconds of slack cover a slightly-off bitrate estimate.
4. **Fetch that range.**
5. If the server did *not* honour ranges (no `206 Partial Content`), the
   first response already contains the whole file; slice it locally instead.

### Finding a frame: `frame_aligned`

A byte range starts in the middle of an MP3 frame. The decoder needs to
start on a frame header. An MPEG audio frame header begins with 11 set bits
(`0xFFE`), but that bit pattern also appears inside audio data. So:

- `parse_header` decodes a candidate 4-byte header: MPEG version, layer
  (must be III), bitrate index, sample-rate index, padding bit; rejects the
  reserved values; and computes the frame length:
  `length = factor × kbps × 1000 / sample_rate + padding`, with `factor` 144
  for MPEG-1 and 72 for MPEG-2/2.5. (320kbps at 44.1kHz → 1044 bytes, as the
  unit test checks.)
- `frame_aligned` scans forward for a candidate **followed by two more valid
  headers at exactly the predicted offsets, agreeing on version and rate**.
  Three in a row is what a real boundary looks like.

The test `frame_sync_skips_a_false_header` plants a lone fake sync word
before three real frames and checks it is skipped.

What lands in the cache is the frame-aligned MP3 slice, smaller than
decoded audio, and disposable.

## Decoding: `decode_mp3`

Symphonia with only the `mp3` feature. The loop:

- `next_packet()`; an `IoError` means the truncated last frame of a byte
  range, the normal end, so `break`;
- `decode()`; a `DecodeError` is skipped. **The first frame usually fails**:
  MP3's *bit reservoir* lets a frame borrow bits from earlier frames, which
  we did not fetch;
- downmix to mono by averaging channels.

`excerpt_pcm` truncates to exactly 90s and enforces the 20s floor.

## Resampling: `resample`

CLAP wants 48kHz; Qobuz MP3s are usually 44.1kHz. `resample(input, from,
to)` is a **windowed-sinc** resampler:

- `gcd(44100, 48000) = 300`, so up = 160, down = 147: every 147 input
  samples become 160 output samples.
- The low-pass cutoff is 0.95 × the lower Nyquist.
- The kernel is `sinc` × a Blackman window over ±`half` taps (`HALF_TAPS =
  32`, widened when downsampling).
- **Polyphase**: there are only `up` = 160 distinct fractional offsets, so
  the kernel weights are tabulated once per phase (when `up ≤ 4096`) instead
  of evaluating `sin` per tap per sample.

The test `resampling_keeps_a_tone_and_its_length` resamples 1s of a 1kHz
sine and checks the output is 48,000 samples and still that sine to within
2e-3. Resampling is about a third of the per-track CPU cost (0.41 of 1.3
core-seconds).

## The cache: `AudioCache`

A size-capped LRU of excerpts, default 20GB (`--cache-gb`):

- files at `cache/audio/<id mod 100, 2 digits>/<id>.mp3`, sharded so no
  directory gets huge;
- `get` reads the file and bumps its mtime (mtime is the "recently used"
  clock);
- `put` writes a unique temp file and renames it;
- eviction does not run after every write, scanning is O(cache), but after
  every 512MB of new data (`RESCAN_BYTES`), deleting oldest-mtime files until
  under the cap.

The point of the cache: re-analysing (after bumping `VERSION`) can skip
Qobuz entirely, and `--workers 12` then goes as fast as the CPU allows.

## The stage: `analyse::run`

### What is pending

```rust
fn unanalysed() -> _ {
    features::track_id.nullable().is_null()
        .or(features::extractor_version.nullable().ne(VERSION))
}
```

Tracks left-joined to `features` and `failures`, where there is no feature
row or it has the wrong version, not blocked, not previously failed (unless
`--retry-failed`), ordered by `seed_distance, id`, your own library first.

### The two kinds of work

Fetching is **network-bound** and spends the rate limit; extraction is
**CPU-bound**. They are pipelined:

```
             stage thread (one current-thread tokio runtime)
┌────────────────────────────────────────────────────────────────┐
│ pending: VecDeque<Pending>                                     │
│    │ pop while fetches < fetchers && fetches+extracting < cap  │
│    ▼                                                           │
│ FuturesUnordered<fetch>  ──(url: 1 rate-limited request)──►    │
│    │                     ──(bytes: CDN, free)──────────────►   │
│    ▼                                                           │
│ select! {                                                      │
│   fetch done  → pool.submit(id, bytes)       ─────────┐        │
│   result back → store() to SQLite, log progress       │        │
│   250ms tick  → re-check cancellation                 │        │
│ }                                                     │        │
└───────────────────────────────────────────────────────┼────────┘
                                                        ▼
            Workers: N OS threads, each with its own ONNX session
            std::mpsc queue in  →  analyse_mp3  →  tokio mpsc out
```

- **Fetchers** are futures on the stage's own thread, all sharing one
  `QobuzClient` behind a `tokio::sync::Mutex`. The lock is held only for
  `file_url`, the one request that costs budget, and released for the
  CDN download. `client.login()` is called once up front so the fetches do
  not race to log in.
- **Workers** (`Workers::start`) are OS threads, each owning an
  `AudioEncoder` (≈600MB ONNX session). They pull `(track_id, bytes)` from a
  shared `std::sync::mpsc` receiver behind a `Mutex`, and send results back
  over a `tokio::sync::mpsc` unbounded channel the stage can `.await`.
  `catch_unwind` turns a panic into an `Err` string, so one bad file cannot
  kill a worker.
- **Only the stage thread writes SQLite.** Each result is committed as it
  arrives (`store`), which is what makes the stage resumable at any point.

### How many

- `default_workers()` = hardware threads / 4, clamped to 1..4. One ONNX
  thread each (`THREADS_PER_WORKER = 1`): design.md measured 1.3
  core-seconds per excerpt on one thread and only 0.9s wall on four, so
  more workers scale better than more threads per worker.
- `fetchers` defaults to `max(workers, 4)`.
- `in_flight_cap = workers × 2 + fetchers` bounds how many excerpts sit in
  memory between fetch and extraction.

The real ceiling is the rate limit: one signed URL per track at 2/s is
~7,200 tracks/hour, and three workers already keep up with that.

### Failing well

A failed fetch or extraction calls `record_failure` (reason truncated to 500
chars, upserted into `failures`) and logs `! <id> <title>: <reason>`. The run
continues. `store` deletes any old failure row for a track that now
succeeds.

### Stopping

The loop checks `job.cancelled()` each iteration, and the 250ms branch of
`select!` guarantees an iteration even while everything is downloading.
After a stop, `pool.finish()` drops the work sender and joins the workers,
letting them finish the excerpts they hold (about a second each).

### Model weights

Before starting workers, `models::ensure(model_dir, [CLAP_AUDIO])` downloads
the audio tower if absent (chapter 6).

## `analyse_pcm`: the per-track CPU work

```rust
pub fn analyse_pcm(encoder: &mut AudioEncoder, pcm: &Pcm) -> Result<(Descriptors, Vec<f32>)> {
    let audio = audio::resample(&pcm.samples, pcm.rate, clap::SAMPLE_RATE);
    let descriptors = descriptors::extract(&audio, clap::SAMPLE_RATE)?;
    let embedding = encoder.embed(&audio)?;
    Ok((descriptors, embedding))
}
```

Resample once, then both analyses read the same 48kHz mono signal. The
next two chapters cover them.

## Check yourself

1. What makes "the middle 90 seconds" expressible as a byte range?
2. Why check for three consecutive frame headers instead of one?
3. Why does the first decoded frame usually fail, and why is that fine?
4. Which part of fetching a track costs rate-limit budget, and how is the
   shared client's lock scoped to reflect that?
5. Why does only one thread write to SQLite?
6. You bump `VERSION`. What happens on the next `analyse`, and why is it
   faster than the first time?

## Exercises

1. Run `cargo test --release -- --ignored cost --nocapture` (needs the audio
   weights) to measure one excerpt's cost on your machine at 1, 4 and all
   threads. Compare with design.md's numbers.
2. With `TWO_KHZ_MP3_SAMPLE=some.mp3`, run the ignored test
   `excerpt_from_the_middle_of_a_served_file`. Read how it fakes a
   range-honouring CDN in 30 lines.
3. `window()` uses the *reported* duration. What happens to the excerpt if
   Qobuz reports a duration that is 30% too long? Which guard catches the
   worst case?
