# 15. The UI surfaces

By the end of this chapter you will know what every panel on screen does,
where its state lives, and the handful of bugs whose fixes shaped it. Each
section names its file in `app/src/ui/`.

## The browse list: `library.rs`

### Views and the shelf

```rust
pub enum View {
    Search(String), FavouriteTracks, FavouriteAlbums, FavouriteArtists,
    Playlists, Playlist { id, name }, Album(RemoteAlbum), Artist(RemoteArtist),
}
pub struct Shelf { tracks, albums, artists, playlists, similar }
```

One `Shelf` holds whatever the current view loaded, search fills three
lists at once, an artist fills `albums` and `similar`. `load(view)` maps each
view to backend calls (e.g. `Artist` → `artist_albums` + `similar_artists`,
where a missing similar list is not fatal).

`Album` and `Artist` carry the whole object, so the page can show the same
cover, badges and actions as the detail sheet (`AlbumDetail`/`ArtistDetail`
with `inline: true`) above the list.

### Navigation is explicit, and the fetch is not where you'd expect

- `go(view)` pushes the current view onto `history`, then `show(view)`;
- `back()` pops;
- `show(view)` sets the view, **empties** the shelf, sets `loading`, stores
  `pending = Some(view)`, and bumps `epoch`;
- `drive()`, called from a `use_effect` in `LibraryPanel`, takes
  `pending` and spawns the fetch.

Why not fetch inside `show`? `show` is called from row click handlers, and
emptying the shelf unmounts those rows. A task spawned in a dying component
is dropped with it, which once left the panel on "loading…" forever. So the
fetch is spawned from `LibraryPanel`, which lives as long as the window.

Why `epoch`? `pending` is one slot, but the spawned tasks are not. Two views
asked for in quick succession race, and the slower one would land last and
overwrite the newer view. Every fetch remembers the epoch it started with
and discards its result if the epoch has moved. With search-as-you-type,
this race became routine.

`search_to(query)` **replaces** a search view rather than pushing another,
so "back" does not walk you through your own spelling; `leave_search()`
restores what the search covered when the box is emptied.

### One list, two sections

The list has a space section ("in your space", from `SpaceMatches`) and a
Qobuz section. The space section only appears for `FavouriteTracks` and
`Search`, the space is a flat set of tracks, with nothing to say about
albums or artists. Qobuz tracks already shown in the space section are
dropped from the Qobuz section (`qobuz_only`).

### Rows are addressed by index, carefully

The context menu names a shelf row by its **index** into
`shelf.visible_tracks(&blocklist)`, and re-reads that list when an item is
chosen. Two rules keep that correct:

- rendering and click handlers must both go through `visible_*`, once,
  filtering in one and indexing the unfiltered list in the other made row N
  open row N+1;
- `qobuz_only` **enumerates before filtering**, so each row keeps its index
  into `visible_tracks` even when some are hidden because the space section
  shows them.

Space rows are addressed by **track id** (`MenuTarget::SpaceTrack`) instead,
because they come from a different list.

### Other bits

- `tracks_view: Grid | List`: one toggle for both track sections.
- `liked_only`: narrow a search to favourites. The favourite id sets
  (`Liked`) are fetched once, lazily, the first time anything asks; the
  heart in the detail sheet reads the same cache, and `set_liked` updates it
  optimistically.
- `request_analysis(kind, id, label)`: the "fetch into catalogue" actions;
  posts a notice telling you to run analyse afterwards.
- `SpaceRows` renders 80 rows at first and grows on "show more" (the memo
  keeps up to 720 ready).

## The detail sheet: `sheet.rs`

`DetailSheet` reads `Detail` and dispatches on `DetailSubject`:
`TrackDetail`, `AlbumDetail` or `ArtistDetail`. Each is **keyed by the
subject's id**, which forces a fresh mount when you jump from one album to
another, otherwise Dioxus would treat it as a prop change on the same
instance and briefly show the previous album's tracklist under the new
heading.

Below 900px it is a bottom sheet over the list with a backdrop; above, it
docks beside the list (pure CSS, chapter 16).

### `TrackDetail`

Cover, title, artist link, album, badges (in your space, BPM, release date,
not streamable), credits (`performers` split on `;`), then actions:
**Play**, **Play next**, **Queue**, **♡ Like**, **On the map** (space tracks
only, it selects the track, opens the map in browse mode and *closes the
sheet*, which is the one place `Selection` and `Detail` part ways), and for
space tracks the generate verbs: **Find neighbours**, **Start radio**,
**Path A/B** (labelled "Path B, go" when A is already set), **Drift from
here**. Then **Hide {artist}**. For space tracks, `GenerateSection` follows.

### `AlbumDetail` / `ArtistDetail`

Fetch their tracklist / discography on mount (`use_future`), offer
play/queue/fetch/hide, and a button to open the full page in the library
(`library.go(View::Album(..))`), hidden when `inline` is already that page.

### `GenerateSection` and `Generator` (`generate.rs`)

Mode tabs (neighbours, radio, path, drift); switching tabs changes only the
inputs shown, it no longer clears the result. Inputs per mode: path shows A
and B with "set" buttons and a **shortest / evenly paced** switch; drift
shows a phrase box (or an error if the server has no text tower); radio has
an **artist pull** slider (0.10 default); there is a **tracks** count (20).
Then **generate**, **play**, **export**, **clear**.

`Generator` in `generate.rs` is the controller:

- `run(selected)` peeks every input, builds a **`Recipe`** from the same
  peeked values (so the label can never describe a different run), sets
  `busy`, and `spawn_forever`s the work. Forever, not scoped: `request` is
  called from the context menu, which closes in the same click, a scoped
  task would die before being polled, leaving `busy` stuck on "working…".
- Inside: take the engine lock and call the `Navigator` (chapter 13). Drift
  first awaits `backend().embed(phrase)` **outside** the lock, holding the
  engine across a round trip would freeze every slider.
- `dedup_by_recording`, then set `result`, `produced_by`, clear `stale` and
  `busy`.
- `request(mode, seed)`, `set_path_end(A|B, id)` (runs as soon as both ends
  are known), `aim_drift()` (switches mode but does not run, "guessing a
  phrase would be inventing the user's intent"), `rerun(&recipe)` (restores
  the inputs that made a result, rather than re-running whatever the panel
  now shows).
- `invalidate()` marks a result **stale** when weights move, the rows stay,
  with a "regenerate" chip; `clear()` is only for when you ask.

The result list has a heading from `describe(recipe)`, "Radio from Artist,
Title", "Path A → B, evenly paced", "Drift from X towards “darker and
slower”", because results now outlive the selection and the tab that made
them. **On the map** calls `map.show_route()`, which dims everything off the
route.

**Export** creates a private Qobuz playlist named
"two_khz (N tracks)" via `POST /api/playlists`.

### `WeightsDisclosure`

"Tune the space", collapsed by default. One slider per block from the
manifest (0 to 3, step 0.1), showing "changed" when any differs from the
defaults, and a **reset**. Every move: update the `Weights` signal, call
`engine.set_weights`, `generator.invalidate()`. The note under it: "Map
positions are fixed until the layout step is re-run."

Because the block names come from the manifest, a new block added on the
server grows a slider automatically.

### `PathPill`

Shown when `from` is set and `to` is not: "Path from **X**, pick an end",
with **reopen** and **×**. It survives closing the sheet, backgrounding the
app, or finding B from a different screen, the default condition on a
phone.

## The player: `player.rs`

```rust
pub struct Player {
    queue: Signal<Vec<RemoteTrack>>, index, playing, position: (f64, f64),
    quality, status, volume, muted, queue_open, full_open,
}
```

Playback itself is an `<audio id="player">` element owned by `player.js`
(chapter 16). Rust hands it URLs and gets events back.

### The queue never holds a recording twice

Every path into the queue goes through `unduplicated(tracks, seen)`, keyed
on `RemoteTrack::identity()` (ISRC, else id, chapter 3):

- `play_list(queue, index)` replaces the queue and plays the row you
  clicked, **looked up again by identity** after deduplication so "the
  third row" still plays the third row;
- `enqueue(tracks)` appends, starting playback if nothing was queued;
- `play_next(tracks)` inserts after the current track.

### Keeping `index` on the playing track

- `remove_at(at)`: removing above shifts `index` down; removing the playing
  track hands over to whatever slides in, or stops if it was last.
- `move_by(at, ±1)`: a swap; follow the index if it was one of the two.
- `move_to(from, to)`: what a drag reports; `index_after_move` is a pure
  function tested exhaustively against an actual `Vec` reorder for every
  (index, from, to) in a 7-long queue.

### `play_at(index)`

Set the index and a provisional duration; refuse unstreamable tracks; **skip
tracks by a hidden artist** (a queue built before a block may hold them);
fetch a stream URL at the chosen quality; `twoKhzPlayUrl(url)`; and
`twoKhzSetMetadata(title, artist, album, art)` for the OS's "now playing".

`step(±1)` stops at the ends rather than wrapping. `toggle` pauses, resumes,
or starts a queued-but-never-started queue.

### `use_transport`

The long-lived eval that installs `player.js` and folds its messages into
signals: `time` → position, `playing` → playing, `ended` → play the next
one, `failed` → "the stream URL may have expired", `transport` → previous or
next (media keys and mouse side buttons arrive as MediaSession actions).

### `PlayerBar` and `FullPlayer`

The bar: art and title (either opens the full player), transport, seek (a
0 to 1000 range → `twoKhzSeek(fraction)`), volume and mute (mute sends 0 rather
than setting `audio.muted`, so unmuting restores the slider), quality
select, **map** toggle, and the queue chip ("3/12"). The full player is the
same controls with big art, plus **queue** and **on the map** exits that
close it on the way, overlays do not stack.

## The queue drawer: `queue.rs`

A list with a drag grip per row (`queue-drag.js`, chapter 16), **sort by
distance**, **clear**, **close**. `sort_queue` reorders only what follows the
current track using `shortest_path_order` anchored on the playing track
(chapter 13); tracks the space has never analysed keep their order and go to
the end, and the status line says how many.

(The module's doc comment still says reordering is "by up/down rather than
drag". Both exist now (up/down in the row menu, drag on the grip) and the
comment predates the drag.)

## The context menu: `menu.rs`

One menu for the whole window, opened by right-click, the `⋯` button on a
row, or a 500ms long press on touch.

```rust
pub enum MenuTarget {
    ShelfTrack(usize), ShelfAlbum(usize), ShelfArtist { index, similar },
    QueueEntry(usize), SpaceTrack(i64),
}
```

`tag()` / `parse()` convert a target to and from a string like
`"shelf-artist:4:similar"`, because a long press is detected in JS and comes
back through the eval channel, and a DOM `data-menu` attribute holds only
text. A round-trip test covers every variant: a mismatch would not fail
loudly, it would just make long-press silently do nothing.

`ContextMenuView` renders one item component per target
(`ShelfTrackItems`, `QueueItems`, …), each **re-reading its list when an
item is clicked**. A shared `GenerationItems` component offers the four
walks for any space track. An effect nudges the menu back inside the window
once it has a size, like a native menu.

## The pipeline view: `pipeline.rs` and `crawler.rs`

`use_pipeline_watch` is one loop, every 200ms: status (and `generation`),
the log mirror, and every 10th tick the corpus counts. `use_crawl_status`
polls the crawl the same way. The server can't push into Dioxus signals, so
views poll published status.

The view shows:

- **Corpus** counts, with hints derived from the same questions the stages
  ask: "The space holds X tracks but Y are ready, run build space" when
  `buildable ≠ in_space`; "N tracks in the space have no coordinates, run
  layout" when `in_space > on_map`.
- **Stages**: "run analyse → space → layout" (the full run), a per-stage
  **run** button (or "queued", or **stop** for a running crawl), and a
  global **stop**.
- **Output**: the log.
- **Devices**: pair (with play/pipeline scope; the token is shown once),
  list with last-seen, revoke.

## Check yourself

1. Why does `LibraryPanel` rather than `Library::show` spawn the fetch?
2. What bug does `epoch` prevent?
3. Why is the detail sheet keyed by subject id?
4. Why does `Generator::run` use `spawn_forever`?
5. What does a "stale" result mean, and what does "regenerate" restore?
6. Why does `MenuTarget` need a string form at all?

## Exercises

1. Add an "Add all to queue" button to the generate result.
2. The revoke button in `Devices` calls `refresh()` right after spawning the
   revoke, so the list can be refetched before the revoke lands. Fix it.
3. Add a `MenuTarget::PlaylistRow(usize)` with a "Play playlist" item, and
   extend the round-trip test.
