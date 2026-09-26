# 2. The data model

By the end of this chapter you will know every table, what writes it and
what reads it, how the upserts merge partial data, how the schema is kept in
step between SQL and Rust, and how the slim client catalogue is made.

## One contract, two mirrors

`schema.sql` at the repo root is the contract. It is:

- **embedded into the server** at compile time
  (`server/src/db.rs`: `const SCHEMA: &str = include_str!("../../schema.sql")`)
  and executed on every connection by `db::ensure_schema`;
- **mirrored by hand** in `app/src/schema.rs` as Diesel `table!` macros,
  which both crates use for typed queries.

`CREATE TABLE IF NOT EXISTS` means running the schema against an existing
database does nothing to existing tables. So changing a table needs two
edits (`schema.sql` and `schema.rs`) *plus* a migration in `db::migrate`.
The comment at the top of `schema.rs` points out the upside: add a column to
the mirror and the compiler finds every query it affects.

Because `schema.sql` is `include_str!`'d, it is a *build input*: the
Dockerfile copies it into the build context explicitly.

`schema.sql` also sets two pragmas: `journal_mode = WAL` (readers do not
block the writer, which matters while a stage writes and requests read) and
`foreign_keys = ON`.

## The tables

### `artists`, `albums`, `tracks`: the catalogue

```sql
artists (id INTEGER PK, name, qobuz_json, similar_fetched_at)
albums  (id TEXT PK, artist_id, title, release_date, label, genre, qobuz_json)
tracks  (id INTEGER PK, album_id, artist_id, title, duration, isrc,
         qobuz_json, seed_distance INTEGER NOT NULL DEFAULT 0)
```

Things to notice:

- **Album ids are text.** Qobuz album ids are strings like
  `"0634904077969"`; track and artist ids are numbers. Both
  `crawl::as_id_string` and `qobuz::as_i64` accept either JSON shape because
  Qobuz is not consistent.
- **`qobuz_json`** keeps the raw payload. Nothing reads it at query time; it
  is there so a future column can be backfilled without re-crawling. It is
  also most of the database's size, which is why the slim catalogue drops it.
- **`seed_distance`** is how many similar-artist hops a track is from one of
  your favourites. 0 is your own library. `analyse` works through tracks in
  `seed_distance` order, so your own music is analysed first.
- **`similar_fetched_at`** records when an artist was expanded.
- `isrc` is the International Standard Recording Code, it names the
  *recording*, so one song on a single, an album and a deluxe reissue has
  three track ids but one ISRC. The client uses it to deduplicate
  (chapters 13, 15).

### `features`: what analysis produced

```sql
features (track_id PK REFERENCES tracks(id), extractor_version TEXT NOT NULL,
          descriptors_json TEXT, clap_f32 BLOB, analysed_at)
```

- `descriptors_json` is a serialised `pipeline::descriptors::Descriptors`
  (bpm, key, loudness, spectral stats… chapter 7).
- `clap_f32` is 512 little-endian `f32`s = 2048 bytes, the mean-pooled CLAP
  audio embedding (chapter 6).
- `extractor_version` is `analyse::VERSION`
  (`"clap-htsat-unfused+descriptors-1"`). A row with any other version counts
  as unanalysed and is redone. Bump it when a descriptor's definition or the
  CLAP input changes.

The design principle (design.md, "Extract once, keep what cannot be
recomputed"): store the things that cost a download to get back, derive
everything else at build time.

### `frontier`: the crawl queue

```sql
frontier (kind TEXT, ref_id TEXT, priority INTEGER, state TEXT DEFAULT 'pending',
          PRIMARY KEY (kind, ref_id))
```

`kind` is `'artist'` or `'album'`; `priority` is the hop distance; `state`
moves `pending → done | failed | skipped`. Because it lives in SQLite the
crawl survives restarts. Chapter 4.

### `layout`: map coordinates

`(track_id PK, x REAL, y REAL)`. Rewritten wholesale by `layout::run`, so a
track that has left the space does not keep stale coordinates.

### `failures`: do not retry forever

`(track_id PK, stage, reason, failed_at)`. `analyse` skips these unless
`--retry-failed`. A later successful analysis deletes the row
(`analyse::store`).

### `blocked_artists`: hidden everywhere

`(artist_id PK, name, reason, blocked_at)`. A filter, not a delete (see
chapter 1, pattern 6).

### `devices`: not in `schema.sql`

Paired devices live in the same database file (so one file is the whole
backup) but the table is created by `auth::AuthStore::ensure_schema` and
declared with its own `diesel::table!` inside `server/src/auth.rs`. It is
deliberately absent from the shared `schema.rs`: clients have no business
knowing it exists. Chapter 11.

## Upserts that keep what they know

The crawler sees the same entity many times, in payloads of varying
richness: a full `album/get`, a stub album nested inside a track, a stub
artist inside an album. Each write must *add* information, never erase it.

`crawl::upsert_track` (and its siblings) use SQLite's
`INSERT … ON CONFLICT DO UPDATE` through Diesel, with a custom `coalesce`
SQL function declared in `db.rs`:

```rust
tracks::isrc.eq(coalesce(excluded(tracks::isrc), tracks::isrc)),
```

`excluded(col)` is the value from the row being inserted. `coalesce(new,
old)` keeps the new value if it is non-null, else the old one. So a sparse
payload cannot null out a field a fuller one filled in.

Two columns use different rules:

- `title` and `artists.name` take the new value unconditionally (a title is
  always present, and the latest is best).
- `seed_distance` keeps the **minimum**:
  `least(tracks::seed_distance, excluded(tracks::seed_distance))`, where
  `least` is SQLite's two-argument scalar `MIN`, declared in `db.rs` under
  the name `least`. A track reachable both from a favourite (0) and via two
  hops (2) is distance 0.

`frontier` priorities use the same `least` rule (`crawl::enqueue`).

The unit test `db::tests::catalogue_round_trip` exercises exactly these
cases: sparser-then-fuller payloads, shortest distance winning, lowest
priority winning.

## The one definition of "blocked"

```rust
#[diesel::dsl::auto_type]
pub fn not_blocked() -> _ {
    let blocked = blocked_artists::table.select(blocked_artists::artist_id.nullable());
    tracks::artist_id.is_null().or(tracks::artist_id.ne_all(blocked))
}
```

`#[auto_type]` lets a function return a Diesel expression without spelling
out its enormous type. Every stage filters tracks with this, so the
definition cannot drift. Note the `is_null()` branch: a track with no artist
is never considered blocked.

## Migrations

`db::migrate` currently has one migration: if `features` has an
`essentia_json` column, it is the Python pipeline's table, whose contents
are not comparable with what the Rust analyser writes, so it is dropped and
recreated. Every row would have been re-analysed anyway because of
`extractor_version`.

## The slim catalogue, `catalog.db`

Clients need titles, artists, albums, genres, BPM, coordinates, the CLAP
embeddings (for text steering) and the block list. They do not need
`qobuz_json`, `frontier`, `failures` or the full descriptors. `catalog::build`
projects exactly that:

1. Ensure the source has a schema (this also migrates it).
2. Create `catalog.partial` and set `PRAGMA page_size = 16384` **before any
   table exists**, a page size can only be chosen for an empty database. At
   the default 4096 bytes, one 2048-byte CLAP blob fits per page and half of
   each page is wasted; at 16KB, seven fit. This alone roughly halves the
   file.
3. Apply the same `schema.sql`, so `catalog.db` is still a valid 2kHz
   database and `Catalog::load` needs no special case.
4. `ATTACH` the source and `INSERT … SELECT` table by table, in one
   transaction, tracks before features and layout (they have the only
   foreign keys). `descriptors_json` is reduced to
   `json_object('bpm', json_extract(descriptors_json, '$.bpm'))`.
5. `PRAGMA journal_mode = DELETE; VACUUM`, so the result is a single
   self-contained file with nothing left in a WAL.
6. Rename into place.

This stays raw SQL rather than Diesel because the attached `src` schema is
invisible to Diesel's table types.

When does it run? Whenever the server's pipeline `generation` changes,
including once at startup (`main::watch_generation`), and on demand with
`two-khz-server build-catalog`.

## Reading it back on the client

`app/src/db.rs::Catalog::load(db_path, track_ids)`:

- one left-joined query over `tracks`, `artists`, `albums`, `features`,
  `layout` into `TrackMeta` rows;
- **reordered to match `space.json`'s `track_ids`**, so catalogue index *i*
  is row *i* of `space.bin`. A track id the catalogue lacks becomes a
  placeholder titled `<missing ID>` rather than shifting every index;
- the CLAP blobs loaded into one flat `Vec<f32>` in the same order;
- the blocked artist ids into a `HashSet`.

Keeping those indices aligned is the invariant the whole client relies on.

## Check yourself

1. You add a `tempo_confidence` column to `features`. Which three places
   must change?
2. A crawl sees the same track first nested in a playlist (no ISRC), then in
   its album (with ISRC). What ends up stored, and which clause guarantees it?
3. Why is `devices` not in `schema.sql`?
4. Why does `catalog::build` set the page size before applying the schema?
5. What would break on the client if `Catalog::load` returned tracks in id
   order instead of `track_ids` order?

## Exercises

1. Build a demo corpus (chapter 18) and open both databases with `sqlite3`.
   Compare `PRAGMA page_size`, the file sizes, and
   `SELECT descriptors_json FROM features LIMIT 1` in each.
2. Read `db::tests::catalogue_round_trip` line by line and predict each
   assertion before reading it.
3. Write a migration in `db::migrate` for the column from question 1. Keep it
   idempotent: it must be safe to run on every connect.
