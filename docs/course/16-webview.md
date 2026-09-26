# 16. The webview bridge

By the end of this chapter you will know how Rust and JavaScript talk inside
the app, how 28,000 points are drawn at interactive speed in a WebKitGTK
canvas, how audio playback works, and how one CSS file makes one component
tree behave as a phone app and a desktop app.

Files: `app/assets/*.js`, `app/assets/style.css`, and their Rust ends in
`app.rs`, `ui/player.rs`, `ui/queue.rs`.

## Why there is JavaScript at all

Dioxus desktop and mobile render into a system webview. Rust can create and
patch DOM elements, but it cannot call canvas drawing APIs, handle
high-frequency pointer events without a round trip, or own an `<audio>`
element's playback. So those jobs live in small JavaScript files that are
compiled into the binary with `include_str!` and run with `document::eval`.

## Three ways across

**1. `eval` with a channel**: for small messages, both ways.

```rust
let mut handle = document::eval(include_str!("../assets/map.js"));
while let Ok(message) = handle.recv::<serde_json::Value>().await { … }
```

Inside the script, `dioxus.send(obj)` delivers `obj` to that `recv`. The
channel lives as long as the script's top-level async function has not
returned, which is why every long-lived script ends with an infinite
`await` loop:

```js
while (true) { await new Promise((r) => setTimeout(r, 1000)); }
```

**2. `eval` of one-off calls**: Rust → JS, fire and forget:

```rust
document::eval("window.twoKhzSetRoute && window.twoKhzSetRoute([1,2,3]);");
```

The `&&` guard matters: scripts are installed by `use_future`s and may not
have run yet. `twoKhzSetSelected` goes further and records the value in
`window.twoKhzSelected` so `map.js` can pick it up when it starts.

**3. The asset protocol**: for bulk binary data. The shell registers:

```rust
crate::platform::use_asset_handler("points", move |request, responder| { … });
```

so `fetch("/points/meta")` and `fetch("/points/data")` from JS are answered
by Rust with `map::meta` (JSON) and `map::payload` (bytes). The comment in
`map.rs`: "50k points through `eval` would freeze it."

### Install-once and re-run safety

Several scripts may be evaluated more than once (a component remounting, a
hot reload). They guard their document-level listeners with a flag
(`window.twoKhzLongPressInstalled`, `…QueueDragInstalled`,
`…CoversInstalled`) but **rebind the send function every run**
(`window.twoKhzLongPressSend = (m) => dioxus.send(m)`), because `dioxus` in
each run is *that* eval's channel. Installing listeners once but letting
them call through a swappable sender means a re-run replaces the channel
instead of stacking a second copy of every listener.

## The map: `map.js`

The file's opening comment is worth reading in full. The key measurement:
in WebKitGTK, filling 28k small canvas paths costs ~300ms a frame, and
batching them into one path per colour is *worse* (~1100ms) because Cairo
tessellates a 28k-subpath shape badly. Canvas path APIs cannot draw this many
points interactively.

So the point cloud is **rasterised by hand into an offscreen buffer**, once,
and every frame is a blit of that buffer.

### Loading

`loadPoints()` fetches `/points/meta` and `/points/data`, and makes
zero-copy typed-array views over the buffer exactly as `map.rs` laid it out:
`Float64Array` ids at 0, `Float32Array` x/y at 8n, `Uint16Array` genres at
16n. It builds `byId` (track id → point index) and a spatial grid.
`twoKhzReloadPoints()` repeats this after a block or a new space, dropping
the selection and route (their indices referred to the old point set).

### The grid

A uniform grid over layout space, ~2 points per cell, built with a counting
sort: `order` lists point indices grouped by cell, and
`start[c]..start[c+1]` is cell *c*'s slice. Both hot loops, rasterising
the visible rectangle and finding the point under the cursor, ask "which
points are in this rectangle", and at any zoom beyond "fit" that is a small
fraction of the corpus. `nearest(px, py)` only visits cells within 14px of
the cursor, so hover cost does not depend on corpus size.

### The cloud layer

- The buffer covers the viewport plus half a viewport of margin each way, in
  device pixels, and is **opaque** (pre-filled with the panel colour), which
  turns alpha compositing into a plain linear interpolation.
- Each point is drawn by stamping a precomputed **coverage kernel**, a disc,
  3×3 supersampled for soft edges, premultiplied by alpha, into an
  `ImageData` by hand. The stamp is rebuilt only when radius or dimming
  changes.
- Colour is by genre, from a 10-colour palette.
- Each frame: if the buffer is valid, **blit** it (a 1:1 crop while panning,
  ~2ms; a scaled blit mid-zoom, ~9ms, soft until the gesture settles).
- Rebuild only when the gesture **settles** at a new scale, or when a drag
  runs off the buffer's edge (rate-limited).
- `schedule()` collapses many pan/wheel events into one
  `requestAnimationFrame`.

### Overlays

The route (a polyline plus its points at full strength while the rest are
dimmed), the hover marker and the selected marker (halo, ring, core, and a
label chip that flips sides rather than clamping to the edge) are a handful
of shapes, drawn on top with the normal canvas API each frame.

### Input

- Mouse: drag pans, wheel zooms about the cursor, click selects
  (`{type: "select", track_id}`) or clears on the background
  (`track_id: null`), right-click sends `{type: "menu", track_id, x, y}`.
- Touch: one finger pans, two pinch-zoom (and pan by the midpoint), tap
  selects and shows the label, long press opens the menu. The gesture is
  **re-measured whenever the finger count changes**: subtracting positions
  from a different gesture shape used to jump the map by half the finger
  separation.

On the Rust side (`Shell`), a `menu` message is matched *first*, because it
also carries a `track_id` and would otherwise be swallowed by the selection
branch.

## Audio: `player.js`

Owns `<audio id="player">` (rendered hidden inside `PlayerBar`). It polls
briefly for the element to exist, then defines:

`twoKhzPlayUrl(url)` (also resets `currentTime`, which survives a src swap
in some webviews), `twoKhzResume`, `twoKhzPause`, `twoKhzStop` (removes the
src and reloads), `twoKhzSeek(fraction)`, `twoKhzVolume(level)`,
`twoKhzSetMetadata(title, artist, album, artwork)`.

It sends back: `playing`, `ended`, `failed` (usually an expired signed URL),
and `time`, throttled to twice a second "to leave room on the channel for
the map's selection messages".

**MediaSession**: `twoKhzSetMetadata` fills `navigator.mediaSession.metadata`,
which is what WebKitGTK bridges to MPRIS on Linux, the OS's "now playing",
and where hardware media keys and mouse side buttons come from. Play/pause
actions are handled in JS; previous/next are sent to Rust as
`{type: "transport", action}` because only the queue knows what "next" is.

Seeking and progress never round-trip through Rust.

## Lazy covers: `covers.js`

The `Cover` component (`ui/mod.rs`) renders a `div` with
`data-cover="<url>"` and no background. `covers.js`:

- an `IntersectionObserver` with a 200px root margin notices covers near the
  viewport;
- a `MutationObserver` on the whole body finds covers in rows that appear
  later;
- a queue with **at most 6 concurrent loads** preloads each URL with an
  `Image()` and only then sets it as the background.

Why a background and not `<img loading="lazy">`? Because `cover_url`
*guesses* URLs (chapter 3), so 404s are expected. A failed background
leaves the placeholder tint; a failed `<img>` shows a broken-image glyph,
and hiding that would need an `onerror` hook per cover. Why the throttle?
WebKitGTK asked for 500 images at once stops scrolling smoothly. Covers
marked `eager` (the player's) get their background inline.

## Long press: `long-press.js`

Touch has no right-click, which once left every row action unreachable on a
phone. This script:

- on a non-mouse `pointerdown` inside a `[data-menu]` element, starts a 500ms
  timer; moving more than 10px or scrolling cancels it;
- on firing, sends `{target: <data-menu tag>, x, y}` and sets
  `swallowNextClick`, so the finger lifting does not also activate the row;
- also arms `swallowNextClick` on any `contextmenu` event, WebKitGTK fires
  a stray `click` after a right-click, which used to open the detail sheet
  on top of the menu that had just opened (a 300ms timeout disarms it in
  case no click follows);
- closes the map on Escape (`{closeMap: true}`).

The tag is parsed back with `MenuTarget::parse` (chapter 15).

## Queue drag: `queue-drag.js`

Pointer events, not the HTML5 drag API (which never fires on touch). Only
the grip has `touch-action: none`, so dragging elsewhere on a row still
scrolls. During a drag it only applies `translateY` transforms to show
where the row would land; row heights are measured once at the start
(re-measuring would read already-translated rows and drift). On release it
clears every transform and sends `{from, to}`; Rust's `move_to` reorders the
`Vec`, and Dioxus re-renders. "Reordering the DOM here would be undone by the
next render, and fight the diff besides."

## The CSS: `style.css`

One stylesheet, inlined by `App`. The principle from `ux-rethink.md`:
**phone first; tablet/desktop is the same tree given more width**: no second
layout to maintain.

- **Colours** are CSS variables on `:root` (a dark palette).
- **`.hidden { display: none !important; }`** is how the map and the explore
  body are hidden without unmounting.
- **The sheet**: by default `.sheet` is a 420px flex item beside the list
  (docked). Inside `@media (max-width: 900px)` it becomes
  `position: fixed; inset: auto 0 0 0; max-height: 85vh`, a bottom sheet,
  and `.sheet-backdrop` becomes a visible scrim. Same component, two
  presentations.
- **The map** is `.map-wrap { position: absolute; inset: 0; z-index: 20 }`
  inside `.body`, so it overlays the list.
- **Hover-reveal** of row buttons is wrapped in
  `@media (hover: hover) and (pointer: fine)`, on a touch screen there is
  no hover, and unguarded, every per-row control would be permanently
  invisible on a phone.
- **Safe areas**: below 900px `.app` pads with `env(safe-area-inset-*)` to
  clear notches and gesture bars; `height: 100dvh` tracks mobile browser
  chrome.
- A 560px breakpoint tightens things further for small phones.

The CSS carries some hard-won comments; the ones on `.sheet > *` and
`.sheet .list` explain a specificity bug where lists inside the sheet
collapsed to a sliver and painted over what followed.

## Check yourself

1. Which of the three channels would you use to send 10,000 numbers from
   Rust to JS, and why?
2. Why does each long-lived script end in an infinite `await` loop?
3. Why is the map rasterised by hand instead of drawn with `arc()` and
   `fill()`?
4. What would break if `queue-drag.js` reordered the DOM itself?
5. How does one `DetailSheet` component become both a docked panel and a
   bottom sheet?

## Exercises

1. Run the desktop app with `TWO_KHZ_WINDOW=393x850` to see the phone
   layout without a phone.
2. Add a `twoKhzFit()` JS function that resets the map view, and a "fit"
   chip in `.map-bar` that calls it.
3. Colour map points by BPM instead of genre: extend `map::payload` with a
   `Float32Array` of BPMs and pick a colour ramp in `renderCloud`.
