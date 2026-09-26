# 9. Layout: drawing the map

By the end of this chapter you will understand how 81 dimensions become two,
well enough to follow every loop in the UMAP port, and why the map does not
move when you drag a weight slider.

File: `server/src/pipeline/layout.rs`.

## What UMAP does, in one paragraph

**UMAP** (Uniform Manifold Approximation and Projection) builds a graph in
which each point is connected to its nearest neighbours, with edge weights
saying how strongly they belong together; then it places points in 2D and
nudges them with a physics-like simulation until **connected points attract
and random pairs repel**. The result keeps local neighbourhoods intact,
tracks near each other in the space end up near each other on the map,
while letting global distances bend. It is a picture, not a measurement.

This file is a port of the parts of the Python library `umap-learn` the map
needs, for one metric (cosine over the weighted space) and one purpose.

## The stage

```rust
pub fn run(paths, options, job) -> Result<usize> {
    let space = Space::load(&paths.data_dir)?;
    let weighted = space.weighted(&space.default_weights());
    let coords = umap(&weighted.unit, n, d, options, job)?;
    // delete every layout row, insert the new coordinates, one transaction
}
```

Two things to notice:

- Layout uses the **manifest's default weights**. The client's sliders
  reshape distances live, but the map was drawn with the defaults and stays
  that way until `layout` is re-run. The weights disclosure in the UI says so.
- The `layout` table is **rewritten wholesale**, not upserted: the layout is
  a function of the space, so a track that has left it must not keep old
  coordinates.

Options: `--neighbours 15`, `--min-dist 0.1`, `--epochs 0` (auto: 200 above
10k points, else 500).

## Step 1: nearest neighbours, `nearest`

Brute force: for each point, dot it with every other point (rows are unit
length, so the dot product is the cosine similarity), set self to -∞, take
the top *k* with `two_khz::space::top_k`, and store `(index, 1 - cos)`
cosine *distances*. Rows are split across cores with `std::thread::scope`.

This is O(n²·d). The comment defends it: a 30k corpus is seconds, and the
stage runs rarely. (Approximate nearest-neighbour libraries would be faster
but add complexity and non-determinism.)

`k` is clamped to `n - 1` for tiny corpora.

## Step 2: the fuzzy graph, `fuzzy_graph`

For each point *i* with neighbour distances *d₁ ≤ d₂ ≤ … ≤ d_k*:

- **ρ (rho)** = the distance to the nearest neighbour with non-zero
  distance. Everything is measured relative to it, so the nearest neighbour
  always gets full weight, this is how UMAP adapts to varying density.
- **σ (sigma)**: found by binary search so that
  Σⱼ exp(-(dⱼ - ρ)/σ) = log2(k + 1) - 1.
  (The "- 1" is because `umap-learn` counts the point itself as a neighbour
  at distance zero, contributing 1 to the sum.) σ is floored at
  `1e-3 × mean distance`.
- Edge weight *w(i→j)* = exp(-(dⱼ - ρ)/σ), or 1 if dⱼ ≤ ρ.

That gives a *directed* graph: *j* may be in *i*'s neighbourhood but not vice
versa. UMAP symmetrises with a **fuzzy union**:

    w(i, j) = a + b - a·b      where a = w(i→j), b = w(j→i)

(the probability that at least one of two independent edges exists).
`edges` lists every undirected edge once per direction, as umap-learn samples
them, and is **sorted**, `HashMap` iteration order is random, and the
sampling below must be deterministic.

## Step 3: the curve, `fit_ab`

In the embedding, UMAP models "how strongly two points at distance *x*
should be attracted" with the curve

    φ(x) = 1 / (1 + a · x^(2b))

and fits *a*, *b* so that φ is ≈1 inside `min_dist` and then decays like
exp(-(x - min_dist)/spread). umap-learn uses scipy's `curve_fit`; this port
uses a shrinking **grid search** over (a, b), halving the step whenever no
neighbour improves the squared error. The test
`curve_matches_umap_learn_for_the_defaults` checks it lands on umap-learn's
(a ≈ 1.577, b ≈ 0.895) for `min_dist = 0.1`.

`min_dist` controls how tightly points may pack: smaller = tighter clumps.

## Step 4: the starting positions, `initial`

umap-learn starts from a *spectral* embedding (eigenvectors of the graph
Laplacian). This port uses the **first two principal components** of the
data instead ("simpler and, for a map nobody measures, as good"), scaled to
[-10, 10] per axis, plus a 1e-4 jitter so coincident points can separate.

## Step 5: optimisation, `optimise`

A port of umap-learn's `optimize_layout_euclidean`, **stochastic gradient
descent with negative sampling**:

- Edges weaker than `max_weight / epochs` are dropped (they would not be
  sampled even once).
- Each edge is sampled every `max_weight / weight` epochs, so strong edges
  are pulled more often. `next_sample[e]` tracks when edge *e* is next due.
- For each due edge (i, j):
  - **Attract**: move *i* towards *j* (and *j* towards *i*) along the
    gradient of log φ:
    `coef = -2ab·d^(2(b-1)) / (1 + a·d^(2b))` (with *d²* as `dist_sq`),
    each component clipped to ±4 and scaled by the learning rate α.
  - **Repel**: pick `NEGATIVE_SAMPLE_RATE` (5) random points per positive
    sample (tracked with `next_negative`) and push *i* away from each:
    `coef = 2b / ((0.001 + d²)(1 + a·d^(2b)))`, clipped; coincident points
    get a fixed (4, 4) kick.
- α decays linearly from 1 to 0 over the epochs.
- Every 25 epochs, check for cancellation.

Randomness comes from a tiny **xorshift64\*** generator (`Rng`) with a fixed
seed (42), so **the same space always produces the same map**.

## Is it right?

The test `clusters_stay_apart_on_the_map` builds three tight clusters
around orthogonal directions in 6D and checks that, on the map, the gap
between cluster centres exceeds the sum of their spreads. The demo smoke
test (chapter 18) checks that every track gets coordinates.

## How the client uses it

`catalog.db` carries the `layout` table. `Catalog::load` joins `x`, `y` into
each `TrackMeta`, `map::payload` packs them into a binary blob, and
`map.js` draws them (chapter 16). Tracks in the space but not yet laid out
are simply not drawn; the pipeline view's hint "N tracks in the space have no
coordinates, run layout" counts them.

## Check yourself

1. Why is ρ subtracted from every distance before computing edge weights?
2. What does the fuzzy union do for a pair where only one point lists the
   other as a neighbour?
3. Why must `edges` be sorted?
4. You move the semantic slider to 3.0 in the app. Does the map change? Why?
5. What would you change to make the map look "clumpier"?

## Exercises

1. Run `two-khz-server layout --min-dist 0.01` and then `--min-dist 0.5` on
   the demo corpus and compare the maps.
2. Replace `initial` with random positions (seeded) and compare how many
   epochs it takes the clusters test to pass.
3. Time `nearest` on a 30k-point random space. At what corpus size would
   you reach for an approximate nearest-neighbour index?
