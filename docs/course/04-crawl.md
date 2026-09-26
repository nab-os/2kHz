# 4. The crawl

By the end of this chapter you will know how the corpus grows outward from
your favourites, why the frontier is ordered the way it is, how blocking
stops the crawl from following someone, and how the same crawl code runs
from the command line, from a background thread, and from a single button
press.

File: `server/src/crawl.rs`. Driver loops: `cli.rs::crawl_command` and
`hub.rs::crawl_loop`.

## The shape of it

The crawl is a **breadth-first search over a graph of artists**, where the
edges are Qobuz's "similar artists" and the nodes also contain albums full of
tracks:

```
favourites (distance 0)
   ├── favourite tracks   → tracks rows,  enqueue their artist @0
   ├── favourite albums   → albums rows,  enqueue the album @0 and its artist @0
   └── favourite artists  → artists rows, enqueue the artist @0

pop artist @d:
   ├── artist/get?extra=albums  → album rows, enqueue each album @d
   └── if d < max_distance:
         artist/getSimilarArtists → artist rows, enqueue each @d+1 (unless blocked)

pop album @d:
   └── album/get → track rows with seed_distance d
```

`max_distance` defaults to 2 (`api::DEFAULT_MAX_DISTANCE`), so the crawl
reaches favourites, their similar artists, and *their* similar artists.

## The frontier

A SQLite table (chapter 2) with primary key `(kind, ref_id)`. Adding an item
that already exists just lowers its priority if the new one is smaller
(`enqueue`, via `least`). Items move from `pending` to `done`, `failed` or
`skipped` and are never deleted (except by blocking, below).

`next_batch` picks:

```rust
frontier::table
    .filter(frontier::state.eq("pending"))
    .order((frontier::priority.asc(), frontier::kind.desc()))
```

Lowest distance first, and within a distance, `kind DESC`, `"artist"` sorts
after `"album"` alphabetically, so descending puts **artists before
albums**. That is why a big crawl spends its early phase discovering
thousands of artists before the track count moves: design.md calls this out
as intended, not a stall. The effect is that the graph's shape is mapped
before the expensive tracklists are fetched.

## One step at a time

```rust
pub async fn step(conn, client, max_tracks, max_distance) -> Result<StepResult>
```

1. If `tracks` already has `max_tracks` rows → `BudgetReached`.
2. Pop one pending item, or → `Exhausted`.
3. If its priority exceeds `max_distance`, or it belongs to a blocked
   artist → mark `skipped`, return `Skipped`.
4. Expand it (`expand_artist` / `expand_album`).
5. Success → mark `done`, return `Expanded`. Failure → mark `failed`, return
   `Failed { error }`. A dead id or a region-locked album must not stop the
   crawl.

`Stats::absorb` folds a `StepResult` into counters and returns `false` for
`BudgetReached` / `Exhausted`, which is the loop's exit condition.

Why one step rather than a loop inside? Because of **who holds the Qobuz
client**. Early in the project the app ran the crawl itself while you
browsed; taking the client for one step and handing it back meant searching
and playback never waited more than one item. That property survives on the
server: `hub::crawl_loop` releases everything between steps and checks the
stop flag.

## Expanding

`expand_artist(conn, client, artist_id, distance, max_distance)`:

- `artist_albums_raw(artist_id, 1000)`: upsert every album, enqueue it at
  the same distance;
- stamp `similar_fetched_at`;
- if `distance < max_distance`, `similar_artists_raw(artist_id, 50)`,
  upsert each; **enqueue only if not blocked**.

The comment explains the last point: "a blocked artist is a dead end, not
just a hidden one: following them would pull their whole neighbourhood in."

`expand_album(conn, client, album_id, distance)` fetches `album/get`,
upserts the album, and upserts every track in `tracks.items` with the
album's id, the album artist's id, and `seed_distance = distance`.

Note the artist attribution: tracks inside an album get the *album's*
artist id passed explicitly, not their per-track performer. That is a
choice with a consequence, compilations and featured credits are filed
under the album artist. The README warns that blocking matches on
`artists.id`, so "a featured credit that Qobuz files under a different
artist id can still surface".

## Blocking inside the crawl

Three layers:

1. `db::block_artist` **deletes the artist's pending frontier entry** as
   well as inserting into `blocked_artists`, so an in-flight crawl stops
   expanding them.
2. `step` checks `is_blocked(kind, ref_id)`, for an artist by id, for an
   album by looking up its stored `artist_id`.
3. `expand_artist` never enqueues a blocked similar artist.

## Seeding

`seed(conn, client, cap)` reads favourite tracks, albums and artists with
`favorites_raw` and upserts them at distance 0, enqueuing as in the diagram
above. The CLI passes a cap of 5000 per kind. `--no-seed` skips this and
just resumes the frontier.

## Three drivers

### The command line

`cli::crawl_command`:

- `--artist ID` → `discover_artist`: upsert and enqueue every album at
  priority 0, and enqueue the artist too (so a later crawl takes the
  similar-artist hop from them). It deliberately does *not* fetch every
  tracklist, a prolific artist is hundreds of albums, minutes at 2/s.
- `--album ID` → `crawl_one_album`: fetch one tracklist now.
- otherwise: `seed` (unless `--no-seed`), then `crawl(...)`, which loops
  `step` and prints progress every 32 steps.

`--rate` changes the requests-per-second of this client's bucket.

### The server's background crawl

`Hub::crawl_start(max_distance)` (chapter 10 covers threads in general):

- returns immediately if one is already running;
- grabs the shared `RateLimit` from the Hub's client, which also fails the
  *button press* if `.env` is missing, rather than failing silently on a
  thread;
- spawns a named OS thread `two-khz-crawl` with its own current-thread tokio
  runtime;
- runs `crawl_loop`: a fresh DB connection and a fresh `QobuzClient` sharing
  the budget; loop `step` with `max_tracks = i64::MAX` ("the frontier and
  the stop button are the limits"); after each step update a
  `Mutex<CrawlStatus>` with counters, the last line, total tracks and
  pending frontier size.

Clients poll `GET /api/crawl` every 200ms to render it. Note the server
crawl does **not** seed: it only works the frontier. Seeding is a CLI
action.

### "Fetch" buttons in the app

`POST /api/artists/{id}/fetch` and `/api/albums/{id}/fetch` run
`discover_artist` / `crawl_one_album` via `Hub::fetch_*`, each on an
`off_thread` worker with its own connection and client. These are `play`
scope because they are bounded, one album is one request, one discography
a handful. The UI then tells you to run analyse.

## Check yourself

1. Why do artists come before albums at the same distance?
2. A similar artist is blocked *after* being enqueued. Which of the three
   blocking layers catches it?
3. What stops a background crawl on the server? (There are three answers.)
4. Why doesn't `discover_artist` fetch tracklists?
5. Which `seed_distance` does a track get if it appears on a favourite album
   *and* later on a distance-2 artist's album?

## Exercises

1. On a scratch database, call `enqueue` for the same artist at priorities
   2, 1, 3 and read the stored priority. (This is in
   `db::tests::catalogue_round_trip`, find it.)
2. Change `next_batch` to order `kind ASC` and reason about how a 5000-track
   budget would be spent differently. Do not commit it.
3. Add a `--max-distance` to `POST /api/crawl/start` in the UI's pipeline
   view (the route already accepts it; the view always sends the default).
