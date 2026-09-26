# 8. Building the space

By the end of this chapter you will know exactly what is in each of the ~81
dimensions, why every block is standardised and unit-normalised, how moods
and styles are scored from text, what PCA is doing here, and why the weights
are *not* baked into the file.

File: `server/src/pipeline/assemble.rs` (the `build-space` stage), plus
`server/src/pipeline/labels.rs` and, on the reading side,
`app/src/space.rs`.

## A pure function of the database

`build-space` reads only `features`, `tracks` and `albums`, no network
(except fetching the text tower once), no audio. It takes seconds. That is
the payoff of chapter 5's "store the embedding, derive the rest": editing a
mood phrase or a weight is a rebuild, never a re-analysis.

## The rows

`load_rows` selects every track that:

- has a `features` row with a non-null `clap_f32`,
- is `not_blocked()`,

ordered by track id, with its album's `release_date`, the descriptor JSON,
and the 512-d CLAP vector decoded from bytes. Rows whose vector is not
exactly 512 floats are dropped.

The order of these rows *is* the row order of `space.bin`, recorded as
`track_ids` in the manifest.

## The eight blocks

| block | dims | how |
|---|---|---|
| tempo | 2 | `fold_bpm(bpm)` = log2 of BPM folded into [70, 140); `onset_rate` |
| key | 3 | circle-of-fifths position × strength, and ±strength for major/minor |
| dynamics | 3 | integrated loudness, dynamic complexity, loudness range |
| timbre | 4 | centroid, rolloff, flatness, zero-crossing rate |
| mood | 8 | CLAP contrasts: `emb · (text(a) - text(b))` for 8 axes |
| style | 20 | CLAP similarity to 60 style phrases, reduced by PCA |
| era | 1 | release year |
| semantic | 40 | the CLAP embedding itself, reduced by PCA |

Total: 2+3+3+4+8+20+1+40 = **81**. (PCA takes `min(k, n, d)` components, so
a tiny corpus can have fewer.) `BLOCKS` fixes the order, and a
`debug_assert!` checks the built blocks match it.

### Missing values

`impute` replaces a missing or non-finite descriptor with the column
**median** (all-missing → 0). The median is robust to the outliers a
descriptor produces on odd material.

### tempo: folding

```rust
pub fn fold_bpm(bpm: Option<f64>) -> Option<f64> {
    let mut value = bpm.filter(|b| b.is_finite() && *b > 0.0)?;
    while value < 70.0  { value *= 2.0; }
    while value >= 140.0 { value /= 2.0; }
    Some(value.log2())
}
```

Drum & bass detected at 86 instead of 172 is the classic beat-tracker
failure. Folding into one octave makes the metric **immune** to it instead of
patching per genre. log2 makes equal ratios equal distances. The raw BPM
stays in the database for display.

### key: the circle of fifths

```rust
let position = (index * 7) % 12;               // C=0, G=1, D=2, …
let angle = 2π · position / 12;
[r·cos(angle), r·sin(angle), r·(±1 major/minor)]   // r = key_strength (0.5 if unknown)
```

Multiplying the chromatic index by 7 (mod 12) walks the circle of fifths, so
keys a fifth apart, which share six of seven notes, are neighbours. The
test `keys_a_fifth_apart_are_neighbours` checks C-G is closer than C-F♯.
An unknown key is `[0, 0, 0]`.

The intent in the comment is that "an ambiguous key sits near the origin
rather than confidently in the wrong place". Keep that in mind for the
normalisation section below.

### mood: contrasts, not similarities

`labels.rs::MOODS` defines eight axes as phrase pairs:

```
energy        "energetic, intense music"             vs "calm, gentle music"
valence       "happy, uplifting music"               vs "sad, melancholic music"
aggression    "aggressive, angry music"              vs "tender, peaceful music"
danceability  "danceable music with a strong groove" vs "music with no beat or groove"
darkness      "dark, ominous music"                  vs "bright, sunny music"
acoustic      "acoustic instruments played by hand"  vs "electronic music made with synthesizers"
vocals        "music with a singer and lyrics"       vs "instrumental music with no vocals"
complexity    "complex, experimental music"          vs "simple, repetitive music"
```

`label_vectors` embeds both ends and stores the **difference** of the two
text vectors as the axis direction. A track's score is `dot(clap, a - b)` =
`sim(clap, a) - sim(clap, b)`.

Why a contrast? design.md: "Raw similarity to a single mood phrase mostly
measures how much a track sounds like music at all." Subtracting the
opposite cancels that shared component.

### style: 60 phrases, then PCA

`labels.rs::STYLES` lists 60 broad genres ("rock", "bebop", "drone",
"drum and bass"…), each prompted as `"{style} music"`. Each track gets 60
similarities, which are highly correlated (rock ~ hard rock ~ grunge). PCA
reduces them to 20 components that capture most of the variation. The
comment: "what counts is covering the ground, not fine distinctions between
neighbours."

### era

`release_year` parses the first four characters of the album's release date.

### semantic: PCA of CLAP

The full 512-d embedding reduced to 40 principal components. This is the
most informative block, and carries the highest default weight.

## PCA, briefly

**Principal component analysis** finds the directions along which a cloud of
points varies most. Projecting onto the top *k* keeps most of the variance
in *k* numbers. `fit_pca(rows, k)`:

1. compute the column means;
2. accumulate the covariance matrix of the centred rows (upper triangle,
   then mirrored);
3. `nalgebra`'s `symmetric_eigen` → eigenvalues and eigenvectors;
4. take the *k* eigenvectors with the largest eigenvalues;
5. **fix each component's sign** so its largest-magnitude coefficient is
   positive, eigenvectors are only defined up to sign, and without this a
   rebuild could flip an axis for no reason.

`project` subtracts the mean and dots with each component. The test
`pca_finds_the_long_axis` checks it recovers the (1,1)/√2 direction.

The semantic PCA's mean and components are written to `semantic_pca.bin`
(`input_dim` floats of mean, then `n_components × input_dim`), so a CLAP
vector from outside could be projected into the semantic block. The client
loads this with `Space::semantic_pca` but, see chapter 13, deliberately
does *not* project text vectors through it.

## Normalisation: the heart of it

```rust
pub fn normalise_block(rows: &[Vec<f64>]) -> (Vec<Vec<f32>>, Vec<f64>, Vec<f64>)
```

Two steps, in this order:

1. **Z-score each column**: subtract the column mean, divide by its standard
   deviation (a zero std becomes 1). Now loudness in LUFS and centroid in Hz
   are on the same footing, "stops one wide-range descriptor swamping its
   neighbours".
2. **L2-normalise each row within the block**: every track's block vector
   becomes unit length. Now each block contributes equally before weights,
   "stops the 40-d semantic block silently dominating the 2-d tempo block
   regardless of any weight you set".

Why does step 2 make blocks equal? The final similarity is a cosine over the
concatenated vector. If each block is unit-length, then after weighting by
*w_b* each block contributes exactly *w_b²* to the squared norm, so every
row has the same norm √(Σ *w_b²*), and the cosine of two tracks is

    cos(x, y) = Σ_b w_b² · cos_b(x, y)  /  Σ_b w_b²

**The overall similarity is a weighted average of per-block cosines, with
weights *w_b²*.** (Strictly, this holds when no block of either row is the
zero vector, which, as the next section shows, can happen.) That is a
clean, understandable model, and it is why the sliders mean what they say.
Note the square: moving a slider from 1 to 2 quadruples that block's share.

### A consequence worth understanding

Row normalisation keeps a block's *direction* and discards its *magnitude*.
For wide blocks that is the intent. For narrow ones it has sharper effects,
and it is worth reasoning through them yourself:

- **era (1 column)**: a unit vector in one dimension is just a sign. After
  z-scoring, a track's era value is `(year - mean)/std`; dividing by its own
  absolute value leaves **+1 (newer than the corpus mean), -1 (older), or 0
  (exactly the mean)**. So the era block, as built, is a binary "before or
  after the average year" feature, and its cosine between two tracks is +1
  or -1. Compare the manifest's `columns: ["release_year"]` with what the
  numbers can actually express.
- **tempo (2 columns)**: each track becomes a point on the unit circle in
  (folded tempo, onset rate) z-space. Only the *angle* survives. A track
  that is slightly faster than average with average onset density, and one
  that is much faster, land in the same place.
- **key (3 columns)**: the radius-as-strength idea from `key_coordinates`
  does not survive either. The columns are first *z-scored* (so the unknown
  key `[0,0,0]` is shifted away from the origin), then each row is scaled to
  unit length, so an ambiguous key is placed with the same confidence as a
  clear one.

These are not errors in the arithmetic; they are consequences of applying
one normalisation to blocks of very different widths. Whether they are what
you *want* is a design question, chapter 19 lists it among the exercises,
with some options.

## Writing the files

`build()` concatenates the normalised blocks into one row per track and
writes, via `write_atomic`:

- **`space.bin`**: `n × d` `f32`s, row-major, **unweighted**;
- **`semantic_pca.bin`**: as above;
- **`space.json`**: last, so a reader who sees it also finds the new
  vectors:

```json
{
  "version": "space-2",
  "built_at": "...",
  "n_tracks": 28543, "n_dims": 81,
  "track_ids": [ ... ],
  "blocks": [ { "name": "tempo", "start": 0, "end": 2,
                "columns": ["log_bpm_folded", "onset_rate"],
                "default_weight": 1.0, "mean": [...], "std": [...] }, ... ],
  "weights": { "tempo": 1.0, ... },
  "semantic_pca": { "file": "semantic_pca.bin", "input_dim": 512, "n_components": 40 }
}
```

Each block records its column means and stds, so an external point could be
standardised the same way.

## Why weights are applied later

```rust
pub fn default_weights() -> HashMap<String, f32> {
    tempo 1.0, key 0.4, dynamics 0.8, timbre 0.6,
    mood 1.2, style 1.5, era 0.3, semantic 2.0
}
```

These go into the manifest as *defaults*, not into the vectors. The client
applies weights at query time (`Space::weighted`, chapter 12): multiply each
dimension by its block's weight, re-normalise each row, done, a few
milliseconds for 50k × 81. That is what makes the sliders live without a
rebuild.

`build-space --weights '{"tempo": 2.0}'` changes the *defaults* written to
the manifest (and hence also the weights `layout` uses, chapter 9).

## Versioning the format

`two_khz::space::SPACE_VERSION = "space-2"`. `Space::load` refuses a
manifest with any other version, and also checks that block ends cover
`n_dims` and that `space.bin` is exactly `n × d × 4` bytes. The comment
explains why the version is needed on top of the size check: "a reordered
block keeps the same byte count, so the length check cannot see it."

## Sanity: `evaluate`

`two-khz-server evaluate` asks: for each track on a multi-track album, what
rank is the nearest *other track from the same album* among all tracks? It
prints how often that is rank 1, the median and the mean. If albums do not
cluster, "the space is noise and no path logic will rescue it". Run it
whenever you change labels or weights.

## Check yourself

1. Why does the mood block use `a - b` rather than `a`?
2. What does row normalisation buy, stated as a property of the final
   similarity?
3. After building, what values can a track's era dimension take?
4. Why is the PCA sign-fixed?
5. You change `"sad, melancholic music"` to `"mournful music"`. Which
   commands must you run for the app to see the change?

## Exercises

1. Build the demo corpus, load `space.bin` in Python or a Rust scratch
   program, and verify: every block of every row has unit norm; the era
   column holds only -1, 0 and 1.
2. Run `evaluate` on the demo with default weights, then with
   `build-space --weights '{"semantic": 0.0}'` and again with
   `'{"semantic": 4.0}'`. What happens to same-album top-1?
3. Add a ninth mood axis to `labels.rs` (e.g. `"live concert recording"` vs
   `"studio recording"`). Rebuild the demo space. Which places in the client
   adapt automatically, and why? (Look at `WeightsDisclosure` in chapter 15.)
