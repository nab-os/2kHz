# 6. CLAP: the neural half

By the end of this chapter you will know what CLAP is, how 90 seconds of
audio become one 512-number vector, how a phrase becomes a vector in the
same space, and how the weights are fetched and verified.

Files: `server/src/pipeline/clap.rs` (audio), `server/src/text.rs` (text),
`server/src/pipeline/models.rs` (weights).

## What CLAP is

**CLAP** (Contrastive Language-Audio Pretraining) is two neural networks, an
*audio tower* and a *text tower*, trained together so that a clip of audio
and a sentence describing it map to nearby unit vectors in one shared
512-dimensional space. Two consequences matter here:

1. **Audio-audio similarity**: two clips that sound alike have a high cosine
   similarity. This becomes the *semantic* block of the space.
2. **Audio-text similarity**: you can score any audio against any phrase,
   "sad, melancholic music", with no training. This is *zero-shot*
   classification, and it powers the mood and style blocks (chapter 8) and
   drift (chapter 13).

The checkpoint is `laion/clap-htsat-unfused`, as exported to ONNX by Xenova.
The header comment of `clap.rs` records why *not* `larger_clap_music`: its
text tower returns near-constant embeddings (mean pairwise cosine ≈ 0.999),
which would flatten every text score while still appearing to work.

It replaced a Python stack (Essentia + EffNet + torch). The Rust port was
validated against transformers: the mel front end agrees to within 0.01 dB
and the embedding's cosine to the torch model's is above 0.9999 (the ignored
test `front_end_matches_transformers`).

## From samples to a spectrogram: `MelFrontEnd`

Neural audio models do not read raw samples; they read a **log-mel
spectrogram**, a picture of how much energy there is at each perceptual
frequency band over time. The model was trained on one exact recipe, so the
front end must reproduce transformers' `ClapFeatureExtractor` bit for bit
(near enough). The constants:

```rust
SAMPLE_RATE = 48_000
WINDOW_SAMPLES = 10 * 48_000        // the model reads 10-second windows
N_FFT = 1024, HOP = 480             // 10ms hop at 48kHz
N_MELS = 64, F_MIN = 50.0, F_MAX = 14_000.0
N_FRAMES = WINDOW_SAMPLES / HOP + 1 // 1001, "centred" STFT
```

`log_mel(window)`:

1. **Reflect-pad** by `N_FFT/2` on each side, numpy-style (the edge sample
   is not repeated). This is what "centred" STFT means: frame *t* is centred
   on sample `t × HOP`.
2. For each of 1001 frames: multiply 1024 samples by a **periodic Hann
   window** (`0.5 - 0.5 cos(2πn/N)`), real FFT, take **power** `|X|²` for the
   513 bins.
3. Multiply by the **mel filter bank**, 64 triangular filters, to get 64
   band energies.
4. Convert to decibels: `10 log10(max(energy, 1e-10))`.

### The Slaney mel scale

`hz_to_mel` / `mel_to_hz` implement the Slaney (Auditory Toolbox) scale:
linear below 1kHz (`mel = 3·hz/200`), logarithmic above. `slaney_filters`
spaces 66 edges evenly in mel between 50Hz and 14kHz, builds a triangle for
each band, and scales each by `2 / (right_edge - left_edge)`, "Slaney
normalisation", which makes each filter's area equal so wide high bands do
not dominate. The test `slaney_scale_round_trips` checks the scale inverts.

## Windows: `windows` and `repeat_pad`

The audio tower consumes 10-second windows. A 90s excerpt is cut into nine.
The rules mirror the Hugging Face processor:

- keep whole 10s windows;
- keep a trailing stub only if it is at least half a window, and
  **repeat-pad** it: tile the stub as many whole times as fits, then fill
  with zeros;
- if the audio is shorter than half a window, use it anyway, repeat-padded.

Tests: `windows_drop_a_short_stub`, `repeat_pad_tiles_then_zero_fills`.

## Embedding: `AudioEncoder::embed`

```rust
let windows = windows(audio);
features = concat(log_mel(w) for w in windows);          // [W, 1, 1001, 64]
let out = session.run(inputs!["input_features" => features])?;
let data = out["audio_embeds"];                          // [W, 512]
pooled = Σ_w  data[w] / ‖data[w]‖                        // normalise, then sum
normalise(pooled)                                        // unit length
```

All windows go through the network as one batch. Each window's embedding is
normalised *before* averaging so a loud window cannot dominate, and the mean
is re-normalised. The result is the 512-d unit vector stored in
`features.clap_f32`.

An `AudioEncoder` is built per worker thread because
`Session::run` takes `&mut self`.

## Text: `TextEncoder`

`server/src/text.rs`. Tokenise with the HF `tokenizers` crate, run the text
tower ONNX session, read `text_embeds`, L2-normalise. One important detail:

> One phrase per call: the export takes no attention mask, so a padded batch
> would embed the padding too.

`TextEncoder::load` returns `Ok(None)` when the files are absent, which is a
normal state, not an error: the UI then hides text steering.

The text tower is used in two places:

- **`build-space`** (chapter 8) embeds the mood and style phrases from
  `labels.rs` and scores every stored audio embedding against them;
- **`POST /api/embed`** (chapter 11), which the app calls for drift. The
  `Hub` loads the encoder once and keeps it behind a `tokio::sync::Mutex`,
  remembering whether it has tried (`text_tried`) so a missing model is not
  re-probed every call.

## Model weights: `models.rs`

Three files, pinned to one Hugging Face revision and checked by SHA-256:

| name | remote | size |
|---|---|---|
| `clap-htsat-unfused-audio.onnx` | `onnx/audio_model.onnx` | ~118MB |
| `clap-htsat-unfused-text.onnx` | `onnx/text_model.onnx` | ~502MB |
| `clap-htsat-unfused-tokenizer.json` | `tokenizer.json` | ~2MB |

- `present` checks **size only**, hashing 500MB on every start would be
  slower than what it guards.
- `download` streams the response (reqwest's `stream` feature) into a
  `.part` file while hashing, logs progress at 25/50/75%, honours
  cancellation between chunks, verifies the SHA-256, and only then renames.
- `ensure(names)` downloads what is missing; `ensure_all` is the `models`
  subcommand.

Weights live in `TWO_KHZ_MODEL_DIR` (default `data/models`), separate from
the data dir, so several corpora (e.g. the demo) share one copy
(`Paths::under` keeps `model_dir`).

## Check yourself

1. What two properties of CLAP does 2kHz rely on?
2. Why must the Rust front end match transformers' so exactly?
3. Why normalise each window's embedding before averaging?
4. What happens if you batch several phrases through the text tower at once?
5. Why does `present` not hash the file?

## Exercises

1. Compute by hand how many windows a 90s excerpt produces, and a 25s one
   (the demo's length). Check against `windows_drop_a_short_stub`'s logic.
2. Plot (or print) `slaney_filters()` for a few bands to see the triangles
   widen above 1kHz.
3. Swap the checkpoint's text tower for a deliberately broken one (e.g. a
   function returning a constant vector) in a scratch test, rebuild the demo
   space, and look at what happens to the mood block's variance.
