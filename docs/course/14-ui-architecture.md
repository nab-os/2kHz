# 14. The UI architecture

By the end of this chapter you will be able to read any component in
`app/src/ui/`, because you will know the Dioxus primitives it uses, the
contexts the shell provides, and the reactive plumbing that keeps the
screen, the engine, the server and the webview in step.

Files: `app/src/main.rs`, `app/src/app.rs`, `app/src/ui/mod.rs`.

## Dioxus in five minutes

Dioxus is a React-like framework. On desktop and Android it renders HTML
into a system webview and runs your Rust in the host process; events travel
from the webview to Rust, and DOM patches travel back.

**Components** are functions returning an `Element`, written with `rsx!`:

```rust
#[component]
fn Greeting(name: String) -> Element {
    rsx! { div { class: "hello", "Hello, {name}" } }
}
```

Props are the function's arguments; `#[component]` generates a props struct.
A component with no props, or equal props, is **memoised**: it re-renders
only when a signal it read changes.

**Signals** are reactive cells: `Signal<T>`, created with `use_signal(|| init)`
or `Signal::new(init)`. They are `Copy`, which is why event handlers can
capture them freely.

- `sig()` or `sig.read()`: read **and subscribe**: the current component
  (or memo/effect) re-runs when it changes;
- `sig.peek()`: read **without** subscribing;
- `sig.set(v)`, `sig.write()`: change it, notifying subscribers.

The difference between `read` and `peek` matters constantly in this code.
Event handlers and background loops use `peek`, so they do not accidentally
subscribe; renders use `read`.

**Hooks** (called at the top of a component, same order every render):

- `use_memo(|| …)`: a derived value, recomputed when signals it read change;
- `use_effect(|| …)`: a side effect, re-run when signals it read change;
- `use_future(|| async { … })`: a task started once when the component
  mounts, living as long as it;
- `use_context_provider(|| value)` / `use_context::<T>()`: dependency
  injection down the tree;
- `use_callback(|arg| …)`: a stable, `Copy` callable.

**Tasks**: `spawn(fut)` runs a future *scoped* to the current component (it
dies when the component unmounts); `spawn_forever(fut)` is not scoped.

**`document::eval(js)`** runs JavaScript in the webview and returns a handle
with a two-way channel: JS calls `dioxus.send(value)`, Rust calls
`handle.recv().await`. Chapter 16 is all about this.

The rule that bites: **a component must call the same hooks in the same
order every render.** Changing the count between renders panics. That is
why `App` is split into two components rather than returning early.

## The root: `App`

```rust
#[component]
pub fn App() -> Element {
    let ready = use_signal(crate::engine_ready);
    rsx! {
        style { {include_str!("../assets/style.css")} }
        if ready() { Shell {} } else { Setup { ready } }
    }
}
```

The whole stylesheet is inlined with `include_str!`. `Setup` and `Shell`
each have their own hooks, so switching between them is safe.

### `Setup`: first pairing

Two inputs (address, token) and a connect button. On connect:

1. save `server.json` **first**, `backend::init` fills a `OnceLock`, so a
   second attempt in this process could not replace a wrong address;
2. `backend::init(Wiring::remote(...).into_backend())`;
3. `sync_and_load()`: sync, set the data dir statics, `init_engine`;
4. on success `ready.set(true)` swaps in the `Shell`; on failure, show the
   error with a hint to run build-space and layout on the server.

### `Settings`: changing it later

A modal with the same two fields, **save** and **forget pairing**. It asks
for a restart instead of reconnecting live: `backend()` hands out
`&'static Backend` references that callers hold across awaits, and swapping
one live would be a much larger change. It also contains `HiddenArtists`,
the collapsible block list with "unhide" buttons.

## The shell

`Shell` is where the app's state lives. It creates every shared piece of
state as a **context**, so any component can `use_context::<T>()` it.

### The contexts

| context | type (in `ui/`) | holds |
|---|---|---|
| `Search` | `mod.rs` | `text` (the box) and `submitted` (what Qobuz was asked) |
| `Weights` | `mod.rs` | current slider values per block |
| `Selection` | `mod.rs` | the space track "in hand" (map highlight, generator seed) |
| `Detail` | `mod.rs` | what the detail sheet shows: a track, album or artist |
| `Player` | `player.rs` | queue, index, playing, position, quality, volume, drawers |
| `Library` | `library.rs` | the current Qobuz view, its shelf, history, loading |
| `Generator` | `generate.rs` | mode, inputs, result, recipe, busy/stale |
| `Crawler` | `crawler.rs` | polled crawl status |
| `Pipeline` | `pipeline.rs` | polled stage status, log, counts, generation |
| `ContextMenu` | `menu.rs` | the open row menu, if any |
| `MapView` | `mod.rs` | `map_open`, `map_route` |
| `Blocklist` | `mod.rs` | hidden artists + `block`/`unblock` callbacks |
| `LocalIds` | `mod.rs` | memo: visible space track ids |
| `SpaceReach` | `mod.rs` | memo: albums/artists with a visible space track |
| `SpaceMatches` | `mod.rs` | memo: space rows matching the search box |

Each context is a small `Copy` struct of signals with methods, `Library`,
`Generator`, `Pipeline` and `Player` are effectively little controllers.

### Selection vs Detail

`Selection` and `Detail` used to be one signal. That made "close the sheet
but keep the point highlighted on the map" impossible: closing meant
deselecting, and the map reads the selection. They are separate now, and
`open_track(selection, detail, id)` sets both, the one function that means
"look at this track", used by rows, map clicks and menu items. The sheet's
own "On the map" button is the one place they deliberately move apart.

### The reactive plumbing

In order of appearance in `Shell`:

**1. The generation effect.** Subscribes to `pipeline.generation`. When it
changes (and is > 0): `backend().sync_space()`, then `reload_engine`, then
clear the selection, detail sheet and generator result (their ids may no
longer exist), and tell the map to reload its points. Either way, recount
`(in_space, on_map)` from the engine for the pipeline view, only the shell
can see the engine.

**2. The block list.** A `use_future` loads it once and pushes the ids into
the engine (`set_blocked`). `apply_block` and `lift_block` callbacks call
the server, re-read the list, update the engine, update the signal, and
reload the map points, "all three, or the halves disagree". These become
the `Blocklist` context.

**3. Derived memos.** `local_ids` and `space_reach` subscribe to the block
list and generation and read the engine. `haystacks` builds one lowercased
`(artist, title, album)` per catalogue row, once per space load, the
filter used to `format!` a fresh string per track per keystroke, "an
allocation storm where a scan would do". `filtered` is the local search
(below).

**4. Long-lived evals** (chapter 16): `use_transport` installs `player.js`;
`covers.js` for lazy art; `long-press.js`, whose messages open the context
menu (or close the map on Escape); `map.js`, whose messages select tracks or
open the menu for a point.

**5. The asset handler.** `use_asset_handler("points", …)` answers the
webview's `fetch("/points/meta")` and `fetch("/points/data")` from Rust with
`map::meta` and `map::payload`, bulk binary data crosses here, never
through `eval`.

**6. Mirroring to the map.** Two `use_effect`s push the selection
(`twoKhzSetSelected`) and, when the map is showing a route, the generator's
result ids (`twoKhzSetRoute`). The selection is also stored in
`window.twoKhzSelected`, because `map.js` is installed by a future and may
not exist yet when the effect first runs.

**7. The initial view.** `open_initial(library)` shows favourite tracks.

## Search: one box, two queries

There is one search box, but two very different queries behind it.

**Local**, every keystroke: `filtered` is a memo over `search.text`:

- split the text into lowercase terms;
- for each visible catalogue row, keep it if **every term** appears in its
  artist, title or album (`Haystack::contains`);
- rank rows where the artist or title *starts with* the first term ahead of
  the rest;
- return up to 720 rows (`SHOWN`) and the total.

The tests in `app.rs` prove that testing terms against each field separately
is equivalent to testing against "artist title album" joined, only because
terms are split on whitespace and so can never span a field boundary.

**Remote**, debounced: a `use_future` loop wakes every 200ms and compares
the text with what it saw last tick. When the text has been **stable for two
ticks**:

- empty (and a search was showing) → `library.leave_search()`;
- at least 2 characters and different from `submitted` → set `submitted`,
  `library.search_to(text)`.

Enter bypasses the debounce. A polling loop rather than a cancellable timer
because "the cancellation semantics of a respawned Dioxus task are subtler
than the 200ms of latency this costs".

### The controlled-input race

`SearchBox` is its own component, deliberately **propless**, so Dioxus
memoises it and it re-renders only when the text changes. The doc comment
tells the story: a controlled input in a webview is a race, the keystroke
travels to Rust, a render patches `value` back, and anything typed in
between is overwritten. When the box lived inside `Shell`, it re-rendered
whenever anything else did (e.g. a search result landing, exactly while
you are still typing), and characters went missing.

## The layout, top to bottom

```
header:  2kHz · N tracks · (M hidden) · [search box] · [pipeline] · ⚙
.body (hidden when the pipeline is showing):
    LibraryPanel              the one browse list
    .map-wrap (canvas#map)    hidden by class, NEVER unmounted
    DetailSheet               overlays below 900px, docks beside the list above
PipelineView                  when "pipeline" is toggled
PathPill                      "Path from X, pick an end"
QueueView                     the queue drawer
PlayerBar                     the mini player, always there
FullPlayer                    the "now playing" overlay
ContextMenuView               last, so it paints over everything
```

The map is the one element that must never be unmounted or moved: `map.js`
caches the canvas node, its 2D context, its listeners and a
`ResizeObserver` when it is evaluated, with no re-init path. A conditional
render would leave it drawing into a detached node; re-mounting would
install a second copy of the script.

`docs/ux-rethink.md` is the design history behind this layout, a
phone-first rethink that replaced a two-pane desktop layout with one list,
a sheet, and overlays.

## Check yourself

1. What is the difference between `sig.read()` and `sig.peek()`, and why do
   background loops use `peek`?
2. Why is `App` two components rather than an early return?
3. What does the shell do when the pipeline's `generation` changes?
4. Why is `SearchBox` a separate, propless component?
5. Why is the map hidden with a CSS class rather than conditionally rendered?

## Exercises

1. Find every `use_context_provider` in `Shell` and match each to a row of
   the table above.
2. Add a context of your own: a `Toast` signal shown above the player bar,
   and use it from `request_analysis` instead of the library's `notice`.
3. Chapter 19 describes a wrinkle: after a resync, `reload_engine` resets
   the engine to default weights but the `Weights` signal keeps its values.
   Fix it in the generation effect.
