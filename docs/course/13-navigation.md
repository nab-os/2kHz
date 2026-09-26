# 13. Navigating the space

By the end of this chapter you will be able to explain, step by step, what
each of the four generate modes computes, why path mode prefers a graph to a
straight line, why drift does not project text straight into the space, and
how the queue's "sort by distance" finds a smooth order.

File: `app/src/paths.rs` (the `Navigator`), with the UI glue in
`app/src/ui/generate.rs`.

## The Navigator

```rust
pub struct Navigator {
    pub space: WeightedSpace,             // unit rows, current weights
    pub catalog: Catalog,                 // TrackMeta per row, CLAP, blocked set
    pub index_of: HashMap<i64, usize>,    // track id → row
    knn: Option<(usize, Vec<Vec<(usize, f32)>>)>,  // cached kNN graph and its k
}
```

Everything is in **rows** (indices into the space) internally and **track
ids** at the edges. Results are `Vec<Step>`, where
`Step { track: TrackMeta, similarity: Option<f32> }`.

Every mode respects the block list in two ways: `mask_blocked` sets blocked
rows' scores to -∞ before any ranking, and `allowed` refuses blocked
candidates. A blocked start or end returns an empty result.

## Constraints

```rust
pub struct Constraints {
    pub artist_cooldown: usize,              // default 3
    pub max_bpm_delta: Option<f32>,          // default None
    pub allow_same_album_consecutive: bool,  // default false
    pub exclude: HashSet<i64>,               // default empty
}
```

`allowed(candidate, chosen, c)` rejects a candidate that is blocked, in
`exclude`, by an artist among the last `artist_cooldown` picks, from the same
album as the previous pick (unless allowed), or more than `max_bpm_delta`
BPM from the previous pick. The UI always uses `Constraints::default()`.

## Neighbours

```rust
pub fn neighbours(&self, track_id, k, exclude_same_artist) -> Vec<Step>
```

Similarities of the seed row against every row; self → -∞; mask blocked;
optionally mask the seed's own artist; `top_k`. The simplest possible
question, and a single pass over the space (n × d multiply-adds, ~2.3M
for 28k × 81, well under a millisecond).

The generator calls it with `k = count` (20) and `exclude_same_artist =
false`.

## Radio: a greedy walk with artist pull

```rust
pub fn radio_nearest(&self, from_id, steps, artist_penalty, c) -> Vec<Step>
```

```
chosen = [start]; used = {start}
while chosen.len() < steps:
    sims = similarities(row of chosen.last())
    sims[used] = -∞; mask blocked
    for each artist already in `chosen`, seen[artist] = times it appears
    sims[j] -= artist_penalty × seen[artist of j]
    pick = first j in argsort_desc(sims) with sims[j] finite and allowed(j)
    chosen.push(pick)
```

Each step jumps from the *current* track, not the seed, so a radio wanders.
It is **deterministic**: the same start gives the same walk.

### Why the artist penalty exists

A prolific artist occupies a tight cluster, so a pure greedy walk tends to
sit inside one discography. design.md measured 14.8 distinct artists per 20
tracks without the penalty. The penalty is subtracted **once per prior
appearance** in the whole walk, so the push outward grows the longer you
stay. At the default 0.10: 19.4 distinct artists per 20, with mean
similarity between jumps barely moving (0.786 vs 0.796).

Why over the *whole* walk and not a short window? The cooldown constraint
already forbids an artist within three steps, so a penalty over that same
window measurably does nothing at any strength.

The `similarity` reported for each step is the *penalised* score.

Cost: each step is a full similarity pass plus a full `argsort_desc`
(O(n log n)), because constraints may reject many of the best candidates.
That is why the UI sets `busy` before running it (chapter 15).

## Path, two ways

Both need two ends, A and B.

### Evenly paced: `interpolate`

```
for step in 1..steps-1:
    t = step / (steps - 1)
    waypoint = (1-t)·row(A) + t·row(B)          # a straight line in the space
    pick = snap(waypoint)                        # nearest allowed, unused track
chosen = [A, picks…, B]
```

`snap` ranks every track by similarity to the waypoint and takes the first
that is unused and `allowed`. The result has exactly `steps` tracks spaced
evenly in "t".

### Shortest route: `graph_path` (the default)

The comment: "Usually better than `interpolate`: a straight line through a
high-dimensional space crosses empty regions, whereas this stays on the data
manifold." Real music occupies a thin, curved region of the space; the
midpoint of two tracks may be far from any real track, so snapping it
yields something only loosely related to either end.

1. **Build the kNN graph** (`knn_graph(k = 16)`), cached until the weights
   or block list change: for every row, its 16 most similar visible rows,
   with edge cost `1 - similarity`. This is n full similarity passes, so it
   is split across cores with `std::thread::scope`, reusing one scores buffer
   per thread and `top_k` instead of a sort. It dominates the first path
   after a load, weight change or block.
2. **Dijkstra** from A to B over that graph, with a small `0.05` penalty on
   any edge that stays with the same artist ("nudge away from same-artist
   chains"). `Entry`'s `Ord` is reversed on cost so `BinaryHeap` (a
   max-heap) pops the cheapest first.
3. Walk `previous` back from B.

The number of tracks is whatever the shortest route needs, not `count`. If
B is unreachable, the result is empty. Note that the kNN graph is
*directed* ("my 16 nearest"), and not symmetrised: a track that is nobody's
near neighbour can have outgoing edges but no incoming ones, so it can be
a start but not an end.

## Drift: towards a phrase

Drift walks from the selected track towards "a described sound".

```rust
pub fn drift_to_text(&self, from_id, clap: &[f32], steps, k, c) -> Vec<Step>
```

1. The app gets the phrase's 512-d CLAP text embedding from the server
   (`POST /api/embed`), the only navigation input that crosses the wire.
2. **`text_anchors(clap, k = 5)`**: dot the text embedding with every
   track's *raw* CLAP audio embedding (from `catalog.db`), mask blocked, take
   the top 5. These are the tracks that best match the phrase.
3. **`text_target`**: average those 5 tracks' rows *in the weighted space*.
   That is the destination.
4. Walk like `interpolate`, from the seed's row to the target: waypoints at
   `t = step/(steps-1)` for steps 1..steps, each snapped to the nearest
   allowed unused track.

### Why not project the text into the space?

`semantic_pca.bin` exists, so one could run the text vector through the
semantic PCA and use it directly. The doc comment on `text_target` explains
why not:

> CLAP has a modality gap, so those coordinates encode the gap rather than
> the content and a walk never arrives. Averaging the best-matching tracks
> stays inside the audio distribution.

The **modality gap** is a known property of contrastively trained models:
text embeddings and audio embeddings occupy two separate regions (cones) of
the shared space. Similarity *rankings* across modalities work, "which audio
best matches this text", but a text vector is never *near* any audio
vector. So instead of using the text vector as a place, drift uses it only
to *rank* real tracks, and makes the place out of them.

This is also why drift needs the raw CLAP matrix on the client, the 40-d
semantic PCA block alone would not rank against a 512-d text vector.

## Sorting the queue: `shortest_path_order`

Not a generate mode, but the same geometry: given the upcoming queue,
reorder it so each step is as short as possible, "the most gradual way
through what is already queued" (`ui/queue.rs::sort_queue`).

This is an **open travelling-salesman** problem (a path, not a tour: a
playlist ends rather than returning to its start), solved approximately:

1. Keep only ids the space holds; fewer than 3 or more than 1024
   (`SORT_CAP`) → return in input order.
2. If an anchor (the currently playing track) is given, make it node 0.
3. Build a full pairwise cosine-distance matrix.
4. **Greedy nearest neighbour**: start at node 0, repeatedly go to the
   nearest unvisited node.
5. **2-opt**: for every pair of positions *(i, j)*, if reversing
   `tour[i..=j]` shortens the path, do it. Because the path is open, reversing
   a tail segment replaces one edge, not two, the code handles `j + 1 == n`
   separately. Repeat up to 32 rounds or until nothing improves; the `1e-6`
   epsilon prevents cycling on near-equal edges.
6. Strip the anchor and map nodes back to input positions (positions, not
   rows, so a duplicated id comes out twice).

Without an anchor, the first queued track stays first "so sorting does not
also change what plays next". The tests in `paths.rs` use points on a unit
circle, where the right answer is simply angle order.

## From results to the screen

`Generator::run` (chapter 15) calls these with the engine lock held, then
post-processes with `dedup_by_recording`: drop any later step whose ISRC
(case-insensitive) matches an earlier one. The navigator refuses to revisit a
*row*, but a single, an album cut and a reissue are three rows of the same
recording.

The comment on `dedup_by_recording` explains why this lives in the UI layer
rather than `paths.rs`: `paths.rs` was kept in step with the Python
implementation as a parity oracle, and this filter had no Python
equivalent. (The Python pipeline has since been removed from the tree, so
that reason is historical, chapter 19.)

## Check yourself

1. What makes radio "wander" rather than circle the seed?
2. Why does the artist penalty reach back over the whole walk?
3. When would `interpolate` give a better result than `graph_path`?
4. What is the modality gap, and how does `text_target` avoid it?
5. Why does `shortest_path_order` track positions rather than rows?

## Exercises

1. Write a test on the circle fixture for `radio_nearest` with
   `artist_penalty = 0` and check the walk goes to the nearest angle each
   time.
2. Symmetrise the kNN graph in `knn_graph` (add j→i whenever i→j exists) and
   measure how often `graph_path` returns empty before and after on the demo
   corpus.
3. Add a `max_bpm_delta` control to the generator panel and pass it through
   `Constraints` for radio.
