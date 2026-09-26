# 7. Descriptors: the signal-processing half

By the end of this chapter you will understand the twelve numbers 2kHz
computes from every excerpt without a neural network, loudness, spectral
shape, tempo, onset rate, key, and the design principle that lets them be
simple.

File: `server/src/pipeline/descriptors.rs`. It replaces Essentia's
descriptors from the Python era.

## The principle: order, not accuracy

From the file's header:

> None of it tries to reproduce a particular library's numbers, the space
> z-scores every column, so what matters is that a descriptor orders tracks
> sensibly and consistently, not its absolute scale.

and

> BPM in particular is allowed to be off by an octave: the space folds tempo
> into one octave before using it, so 86 and 172 land in the same place.

Keep this in mind. Each of these is a *ranking device*, feeding a column
that will be standardised (chapter 8).

## The output

```rust
pub struct Descriptors {
    bpm, onset_rate,                          // rhythm
    key, scale, key_strength,                 // tonality
    loudness_integrated, loudness_range,      // EBU R128
    dynamic_complexity,                       // level movement
    spectral_centroid, spectral_rolloff,      // brightness
    spectral_flatness, zero_crossing_rate,    // noisiness
}
```

Every field is `Option<f64>`: `finite()` turns NaN/∞ into `None` ("unknown"),
and build-space imputes missing values with the column median.

`extract(audio, rate)` requires at least `4 × N_FFT` samples and computes all
of them from the 48kHz mono signal.

## Loudness

### Integrated loudness and loudness range

`loudness()` uses the `ebur128` crate, the EBU R128 broadcast standard.
*Integrated loudness* (LUFS) is a gated, frequency-weighted average of
perceived level over the whole excerpt; *loudness range* (LU) measures the
spread between quiet and loud passages. Silence is gated out entirely and
reports -∞, which becomes `None`.

The test `loudness_of_a_quiet_sine`: a full-scale 1kHz sine is -3.01 LUFS,
so one at amplitude 0.1 (-20dB) should read about -23.

### Dynamic complexity

`dynamic_complexity()` is a home-made measure of *how much the level moves*:
cut into 400ms blocks, take each block's power in dB, drop blocks below
-70dB (a gap between movements says nothing about the music's dynamics),
and return the **mean absolute deviation** from the mean level. Loudness
range is about the extremes; this is about the typical movement.

## The spectrum: `spectral()`

A short-time Fourier transform: `N_FFT = 2048` samples per frame, `HOP = 480`
(10ms at 48kHz, so exactly 100 frames per second), Hann-windowed. For each
frame it computes the power spectrum and then:

- **40 onset bands**: `log_bands` spaces band edges evenly in
  log-frequency from 30Hz to 16kHz ("close enough to mel for onset
  detection"). Each band's energy is compressed with `ln(1 + 1000·E/N)` and
  kept for the rhythm analysis below.
- Skipping silent frames (total power < 1e-9) and the DC bin:
  - **Spectral centroid**: the power-weighted mean frequency. The "centre
    of mass" of the spectrum: higher means brighter.
  - **Spectral rolloff**: the frequency below which 85% of the power sits.
  - **Spectral flatness**: geometric mean over arithmetic mean of the
    power, in dB. Near 0dB for white noise (flat), very negative for a pure
    tone (peaky).

Each is averaged over the non-silent ("voiced") frames.

**Zero-crossing rate** is computed on the raw signal: the fraction of
adjacent sample pairs whose sign differs. Noisy, percussive, bright material
crosses zero often.

## Rhythm

### The onset envelope: spectral flux

`onset_envelope(bands, frames)`:

1. **Flux**: for each frame, sum over the 40 bands of how much the log
   energy *rose* since the last frame (`max(0, now - before)`). A drum hit
   is a sudden rise across many bands.
2. **Remove the trend**: subtract a centred moving average over ±25 frames
   (~0.5s), computed with a prefix sum, and rectify. Without this a
   crescendo, a slow rise, would read as a continuous onset.

The result is a signal that spikes at note/beat onsets, sampled at 100Hz.

### Onset rate

`onset_rate(envelope, fps)` counts peaks that are:

- above `mean + 1 std` of the envelope,
- the local maximum within ±3 frames,
- at least 50ms after the previous counted peak,

and divides by the duration. Onsets per second: a measure of rhythmic
density that does not depend on getting the tempo right.

### Tempo: autocorrelation with a prior

`tempo(envelope, fps)`:

1. Centre the envelope and compute its **autocorrelation** for lags covering
   40 to 220 BPM. A periodic beat makes the envelope correlate with a copy of
   itself shifted by one beat period.
2. Multiply each lag's score by a **prior**: a log-normal bump centred on
   120 BPM, `exp(-½ · log2(bpm/120)²)`. When two octave-related lags score
   similarly (86 vs 172), the one nearer 120 wins.
3. Take the best lag and refine it with **parabolic interpolation** between
   its neighbours, for sub-frame precision (10ms frames are coarse: at 174
   BPM one frame is ~3 BPM).
4. `bpm = 60 × fps / lag`.

The test `tempo_of_a_click_track` synthesises clicks at 90, 128 and 174 BPM
and checks the *folded* BPM is within 2, and the onset rate within 0.5/s of
beats per second.

## Key: chroma against Krumhansl-Kessler

`key(audio, rate)`:

1. **Chroma** (`chroma()`): a long FFT (8192 points → 5.9Hz bins, fine
   enough to separate semitones down to ~100Hz), hop 4096. Every bin between
   100Hz and 5kHz is assigned a pitch class via
   `midi = 69 + 12·log2(hz/440)`, rounded, mod 12. Sum magnitudes per class,
   **normalise each frame by its peak** so loud passages do not decide the
   key, and sum over frames. The result is a 12-number profile of how much
   each pitch class is present.
2. **Profiles**: the Krumhansl-Kessler probe-tone profiles (`MAJOR`,
   `MINOR`), how strongly listeners feel each scale degree belongs to a key.
3. **Match**: rotate each profile to all 12 tonics (24 candidate keys) and
   take the one with the highest **Pearson correlation** with the chroma.
4. `key_strength` is that correlation, clamped to 0..1.

The test `key_of_a_triad` checks A-C-E-A reads as A minor and G-B-D-G as
G major.

## Where the numbers go

Chapter 8 turns these into blocks:

| descriptor(s) | block |
|---|---|
| `bpm` (folded, log2), `onset_rate` | tempo |
| `key`, `scale`, `key_strength` | key (circle of fifths) |
| `loudness_integrated`, `dynamic_complexity`, `loudness_range` | dynamics |
| centroid, rolloff, flatness, ZCR | timbre |

Only `bpm` survives into the client's `catalog.db` (for display and for
`Constraints::max_bpm_delta`).

## Check yourself

1. Why is it acceptable for the BPM estimate to be off by a factor of 2?
2. What would the onset envelope look like for a slow crescendo without the
   moving-average subtraction?
3. What does the 120 BPM prior do, and what failure would you expect for
   genuinely 60 BPM music?
4. Why normalise each chroma frame by its peak?
5. Which descriptors would you expect to be highest for harsh noise music,
   and lowest for a solo flute?

## Exercises

1. Extend `tempo_of_a_click_track` with a 60 BPM case and see what the
   estimator returns before and after folding.
2. Add a `spectral_contrast` descriptor. What else must change for it to
   reach the space? (Hint: chapter 8's `named(&[...])`, and `VERSION`.)
3. `key_strength` is "radius" in the key block. Read chapter 8's section on
   row normalisation and decide whether that radius survives.
