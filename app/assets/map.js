// Canvas map for the suggestion space.
//
// Owns rendering and all pan/zoom/hover, so dragging never crosses into Rust.
// Points arrive once as binary; only selections go back.
//
// At ~30k points the naive shapes of this file all became visible on a slow
// machine: a fill per point, a full scan per mousemove, a draw per event. The
// three structures below, a uniform grid, per-colour batching, and a
// frame-coalesced draw, exist only to keep those costs off the drag path.

(async () => {
  const canvas = document.getElementById("map");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");

  const PALETTE = [
    "#7aa2f7", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7",
    "#7dcfff", "#ff9e64", "#73daca", "#c0caf5", "#b4f9f8",
  ];

  const TAU = Math.PI * 2;

  // Reloadable: hiding an artist removes their points, and the payload is
  // rebuilt server-side, so the views have to be rebound rather than patched.
  let meta, n, ids, xy, genre;
  // Track id -> point index. Rebuilt with the points, because the indices move.
  let byId = new Map();

  async function loadPoints() {
    meta = await (await fetch("/points/meta")).json();
    const buffer = await (await fetch("/points/data")).arrayBuffer();
    n = meta.n;
    // Views match the layout documented in map.rs.
    ids = new Float64Array(buffer, 0, n);
    xy = new Float32Array(buffer, 8 * n, 2 * n);
    genre = new Uint16Array(buffer, 16 * n, n);
    byId = new Map();
    for (let i = 0; i < n; i++) byId.set(ids[i], i);
    buildGrid();
  }

  // ------------------------------------------------------------------- grid
  //
  // A uniform grid over layout space, built once per point load. Both the
  // hot loops need the same thing, the points inside a rectangle, and at
  // any zoom past "fit" that is a small fraction of the corpus.

  let grid = null;

  function buildGrid() {
    if (n === 0) {
      grid = null;
      return;
    }
    const b = meta.bounds;
    const spanX = Math.max(b.max_x - b.min_x, 1e-6);
    const spanY = Math.max(b.max_y - b.min_y, 1e-6);
    // ~2 points per cell on average: enough that a hover test touches tens of
    // points, few enough that the cell table stays small.
    const side = Math.max(1, Math.min(512, Math.round(Math.sqrt(n / 2))));
    // A hair wider than the data, so max_x lands inside the last cell.
    const cellW = (spanX * 1.0001) / side;
    const cellH = (spanY * 1.0001) / side;

    const cellOf = new Int32Array(n);
    const counts = new Int32Array(side * side + 1);
    for (let i = 0; i < n; i++) {
      let cx = ((xy[i * 2] - b.min_x) / cellW) | 0;
      let cy = ((xy[i * 2 + 1] - b.min_y) / cellH) | 0;
      if (cx < 0) cx = 0; else if (cx >= side) cx = side - 1;
      if (cy < 0) cy = 0; else if (cy >= side) cy = side - 1;
      const cell = cy * side + cx;
      cellOf[i] = cell;
      counts[cell + 1]++;
    }
    for (let c = 0; c < side * side; c++) counts[c + 1] += counts[c];

    // Counting sort: `order` lists point indices grouped by cell, and
    // `start[c]..start[c + 1]` is the slice belonging to cell c.
    const cursor = counts.slice(0, side * side);
    const order = new Int32Array(n);
    for (let i = 0; i < n; i++) order[cursor[cellOf[i]]++] = i;

    grid = { side, minX: b.min_x, minY: b.min_y, cellW, cellH, start: counts, order };
  }

  /// Clamp a layout coordinate to a column/row index.
  function col(x) {
    const c = ((x - grid.minX) / grid.cellW) | 0;
    return c < 0 ? 0 : c >= grid.side ? grid.side - 1 : c;
  }
  function row(y) {
    const c = ((y - grid.minY) / grid.cellH) | 0;
    return c < 0 ? 0 : c >= grid.side ? grid.side - 1 : c;
  }

  await loadPoints();

  let view = { scale: 1, offsetX: 0, offsetY: 0 };
  let hover = -1;
  let selected = -1;
  let route = [];

  // The canvas rect, cached. Reading it inside draw forces a layout on every
  // frame of a drag, which is exactly when there is no budget for one.
  let width = 0;
  let height = 0;

  function measure() {
    const rect = canvas.getBoundingClientRect();
    width = rect.width;
    height = rect.height;
    return rect;
  }

  function resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = measure();
    canvas.width = rect.width * dpr;
    canvas.height = rect.height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  function fit() {
    const rect = measure();
    const spanX = Math.max(meta.bounds.max_x - meta.bounds.min_x, 1e-6);
    const spanY = Math.max(meta.bounds.max_y - meta.bounds.min_y, 1e-6);
    const pad = 30;
    view.scale = Math.min((rect.width - pad * 2) / spanX, (rect.height - pad * 2) / spanY);
    view.offsetX = pad - meta.bounds.min_x * view.scale;
    view.offsetY = pad - meta.bounds.min_y * view.scale;
  }

  const screenX = (i) => xy[i * 2] * view.scale + view.offsetX;
  const screenY = (i) => xy[i * 2 + 1] * view.scale + view.offsetY;
  const toScreen = (i) => [screenX(i), screenY(i)];

  // A pan or a wheel tick can fire several times between two frames. Painting
  // each one is wasted work that arrives on screen as lag, so collapse them.
  let pending = false;
  function schedule() {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => {
      pending = false;
      draw();
    });
  }

  // Screen positions staged per palette colour, so the whole cloud costs one
  // path and one fill per colour instead of one of each per point. Grown on
  // demand and reused; across all buckets they hold at most n points.
  const bucketXY = PALETTE.map(() => new Float32Array(512));
  const bucketCount = new Int32Array(PALETTE.length);

  function stage(slot, x, y) {
    const k = bucketCount[slot];
    let buf = bucketXY[slot];
    if ((k + 1) * 2 > buf.length) {
      const grown = new Float32Array(buf.length * 2);
      grown.set(buf);
      bucketXY[slot] = buf = grown;
    }
    buf[k * 2] = x;
    buf[k * 2 + 1] = y;
    bucketCount[slot] = k + 1;
  }

  /// Paint everything staged, then reset the buckets for the next pass.
  function flush(radius, alpha) {
    // Below a couple of pixels a disc and a square are the same smudge, and
    // the square is far cheaper to tessellate, which matters precisely when
    // zoomed out, where the radius is smallest and the points are most.
    const square = radius <= 2;
    const size = radius * 2;
    ctx.globalAlpha = alpha;
    for (let s = 0; s < PALETTE.length; s++) {
      const k = bucketCount[s];
      if (k === 0) continue;
      const buf = bucketXY[s];
      ctx.beginPath();
      for (let j = 0; j < k; j++) {
        const x = buf[j * 2];
        const y = buf[j * 2 + 1];
        if (square) {
          ctx.rect(x - radius, y - radius, size, size);
        } else {
          ctx.moveTo(x + radius, y);
          ctx.arc(x, y, radius, 0, TAU);
        }
      }
      ctx.fillStyle = PALETTE[s];
      ctx.fill();
      bucketCount[s] = 0;
    }
    ctx.globalAlpha = 1;
  }

  /// Stage every point whose cell overlaps the viewport. Cells are coarse, so
  /// each candidate still gets an exact bounds test.
  function stageVisible(radius) {
    const margin = 10;
    const x0 = (-margin - view.offsetX) / view.scale;
    const x1 = (width + margin - view.offsetX) / view.scale;
    const y0 = (-margin - view.offsetY) / view.scale;
    const y1 = (height + margin - view.offsetY) / view.scale;
    // Scale can be negative only if someone inverts the view; guard anyway.
    const cx0 = col(Math.min(x0, x1));
    const cx1 = col(Math.max(x0, x1));
    const cy0 = row(Math.min(y0, y1));
    const cy1 = row(Math.max(y0, y1));

    const { side, start, order } = grid;
    for (let cy = cy0; cy <= cy1; cy++) {
      const base = cy * side;
      // Cells in a row are contiguous in `order`, so one slice covers the
      // whole span rather than one lookup per cell.
      const from = start[base + cx0];
      const to = start[base + cx1 + 1];
      for (let k = from; k < to; k++) {
        const i = order[k];
        const x = screenX(i);
        if (x < -margin || x > width + margin) continue;
        const y = screenY(i);
        if (y < -margin || y > height + margin) continue;
        stage(genre[i] % PALETTE.length, x, y);
      }
    }
  }

  function draw() {
    ctx.clearRect(0, 0, width, height);

    if (!meta.has_layout) {
      ctx.fillStyle = "#8b90a3";
      ctx.font = "13px system-ui, sans-serif";
      ctx.fillText("No layout yet, run: uv run two-khz layout", 20, 30);
      return;
    }

    // Points. Dimmed when a route is showing, so the path reads clearly.
    const dim = route.length > 0;
    const radius = Math.max(1.5, Math.min(4, view.scale * 0.08));
    if (grid) {
      stageVisible(radius);
      flush(radius, dim ? 0.18 : 0.8);
      // The route's own points stay at full strength. A handful of points, so
      // a second pass is cheaper than branching inside the first.
      if (dim) {
        for (const i of route) stage(genre[i] % PALETTE.length, screenX(i), screenY(i));
        flush(radius, 0.8);
      }
    }

    // Route polyline.
    if (route.length > 1) {
      ctx.beginPath();
      route.forEach((i, k) => {
        const [x, y] = toScreen(i);
        k === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
      });
      ctx.strokeStyle = "#ffffff";
      ctx.lineWidth = 1.5;
      ctx.stroke();

      route.forEach((i, k) => {
        const [x, y] = toScreen(i);
        ctx.beginPath();
        ctx.arc(x, y, 5, 0, TAU);
        ctx.fillStyle = k === 0 ? "#9ece6a" : k === route.length - 1 ? "#f7768e" : "#ffffff";
        ctx.fill();
      });
    }

    // Markers last, selection under hover, so the selected track reads as one
    // marker rather than two.
    if (selected >= 0) marker(selected, "#ffffff", true);
    if (hover >= 0 && hover !== selected) marker(hover, "#7aa2f7", false);
  }

  /// A point worth looking at: halo, ring, core, and what it actually is.
  function marker(index, colour, persistent) {
    const [x, y] = toScreen(index);

    // Halo, so the marker separates from a dense cluster.
    ctx.beginPath();
    ctx.arc(x, y, persistent ? 15 : 12, 0, TAU);
    ctx.fillStyle = colour === "#ffffff"
      ? "rgba(255,255,255,0.12)"
      : "rgba(122,162,247,0.16)";
    ctx.fill();

    ctx.beginPath();
    ctx.arc(x, y, persistent ? 9 : 7.5, 0, TAU);
    ctx.strokeStyle = colour;
    ctx.lineWidth = 2;
    ctx.stroke();

    // A filled core keeps the marker legible once the ring is bigger than
    // the point it is ringing.
    ctx.beginPath();
    ctx.arc(x, y, 3, 0, TAU);
    ctx.fillStyle = colour;
    ctx.fill();

    // The selected point keeps a second, wider ring so it stays
    // distinguishable from whatever is merely under the cursor.
    if (persistent) {
      ctx.beginPath();
      ctx.arc(x, y, 13, 0, TAU);
      ctx.strokeStyle = colour;
      ctx.globalAlpha = 0.45;
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }

    const text = (meta.labels && meta.labels[index]) || "";
    if (text) label(text, x, y, colour);
  }

  function roundRect(x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }

  /// Name the point, flipping the chip rather than clamping it, a chip
  /// pinned to the edge covers what you are trying to read.
  function label(text, x, y, colour) {
    ctx.font = "12px system-ui, -apple-system, sans-serif";
    const padX = 8;
    const h = 22;
    const maxWidth = Math.min(280, width - 16);
    let shown = text;
    let w = ctx.measureText(shown).width + padX * 2;
    if (w > maxWidth) {
      while (shown.length > 4 && ctx.measureText(shown + "…").width + padX * 2 > maxWidth) {
        shown = shown.slice(0, -1);
      }
      shown += "…";
      w = ctx.measureText(shown).width + padX * 2;
    }

    let lx = x + 16;
    let ly = y - h - 12;
    if (lx + w > width - 6) lx = x - w - 16;
    if (lx < 6) lx = 6;
    if (ly < 6) ly = y + 16;
    if (ly + h > height - 6) ly = height - h - 6;

    ctx.fillStyle = "rgba(18,19,26,0.94)";
    roundRect(lx, ly, w, h, 6);
    ctx.fill();
    ctx.strokeStyle = colour;
    ctx.globalAlpha = 0.7;
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.globalAlpha = 1;

    ctx.fillStyle = "#e5e7ef";
    ctx.textBaseline = "middle";
    ctx.fillText(shown, lx + padX, ly + h / 2);
  }

  /// Nearest point to a screen position, searched through the grid. This runs
  /// on every mousemove, so it must not depend on the size of the corpus:
  /// only the cells within `maxDistance` are visited.
  function nearest(px, py, maxDistance = 14) {
    if (!grid) return -1;
    const reach = maxDistance / view.scale;
    const lx = (px - view.offsetX) / view.scale;
    const ly = (py - view.offsetY) / view.scale;
    const cx0 = col(lx - reach);
    const cx1 = col(lx + reach);
    const cy0 = row(ly - reach);
    const cy1 = row(ly + reach);

    const { side, start, order } = grid;
    let best = -1;
    let bestDistance = maxDistance * maxDistance;
    for (let cy = cy0; cy <= cy1; cy++) {
      const base = cy * side;
      const from = start[base + cx0];
      const to = start[base + cx1 + 1];
      for (let k = from; k < to; k++) {
        const i = order[k];
        const dx = screenX(i) - px;
        const dy = screenY(i) - py;
        const d = dx * dx + dy * dy;
        if (d < bestDistance) {
          bestDistance = d;
          best = i;
        }
      }
    }
    return best;
  }

  let dragging = false;
  let dragMoved = false;
  let last = { x: 0, y: 0 };

  canvas.addEventListener("mousedown", (e) => {
    dragging = true;
    dragMoved = false;
    last = { x: e.offsetX, y: e.offsetY };
  });

  canvas.addEventListener("mousemove", (e) => {
    if (dragging) {
      view.offsetX += e.offsetX - last.x;
      view.offsetY += e.offsetY - last.y;
      last = { x: e.offsetX, y: e.offsetY };
      dragMoved = true;
      schedule();
      return;
    }
    const found = nearest(e.offsetX, e.offsetY);
    if (found !== hover) {
      hover = found;
      canvas.style.cursor = found >= 0 ? "pointer" : "grab";
      schedule();
    }
  });

  window.addEventListener("mouseup", () => {
    dragging = false;
  });

  canvas.addEventListener("click", (e) => {
    if (dragMoved) return;
    const found = nearest(e.offsetX, e.offsetY);
    if (found >= 0) {
      selected = found;
      schedule();
      // Small message: exactly what eval is for.
      dioxus.send({ type: "select", track_id: ids[found] });
    }
  });

  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const factor = Math.exp(-e.deltaY * 0.001);
    view.offsetX = e.offsetX - (e.offsetX - view.offsetX) * factor;
    view.offsetY = e.offsetY - (e.offsetY - view.offsetY) * factor;
    view.scale *= factor;
    schedule();
  }, { passive: false });

  // ------------------------------------------------------------------ touch
  //
  // A webview synthesises click from tap, but not pan or zoom. One finger
  // pans, two pinch; tap sets `hover`, the only way to see a label.

  let touchStart = null;
  // Finger separation at the last sample, and how many fingers it was measured
  // from, a delta is only meaningful within one gesture shape.
  let pinch = 0;
  let touchCount = 0;

  function centre(touches) {
    const rect = canvas.getBoundingClientRect();
    let x = 0;
    let y = 0;
    for (const t of touches) {
      x += t.clientX - rect.left;
      y += t.clientY - rect.top;
    }
    return { x: x / touches.length, y: y / touches.length };
  }

  function spread(touches) {
    const dx = touches[0].clientX - touches[1].clientX;
    const dy = touches[0].clientY - touches[1].clientY;
    return Math.hypot(dx, dy);
  }

  /// Re-measure the gesture from the fingers that are down right now.
  function reseed(touches) {
    if (touches.length === 0) {
      pinch = 0;
      touchCount = 0;
      return;
    }
    last = centre(touches);
    pinch = touches.length >= 2 ? spread(touches) : 0;
    touchCount = touches.length;
  }

  canvas.addEventListener("touchstart", (e) => {
    // A gesture starts with the first finger. Later fingers must not restart
    // it, or a pinch gets counted as a tap.
    if (touchCount === 0) {
      dragMoved = false;
      touchStart = centre(e.touches);
    }
    reseed(e.touches);
  }, { passive: true });

  canvas.addEventListener("touchmove", (e) => {
    e.preventDefault();

    // Finger count changed, so `last` describes a different gesture.
    // Re-measure rather than subtracting incomparable positions, that jumped
    // the map by half the finger separation when one finger left a pinch.
    if (e.touches.length !== touchCount) {
      reseed(e.touches);
      return;
    }

    if (e.touches.length >= 2) {
      const now = spread(e.touches);
      const mid = centre(e.touches);
      if (pinch > 0 && now > 0) {
        const factor = now / pinch;
        view.offsetX = mid.x - (mid.x - view.offsetX) * factor;
        view.offsetY = mid.y - (mid.y - view.offsetY) * factor;
        view.scale *= factor;
      }
      // Two fingers pan as well as zoom, by however far their midpoint went.
      view.offsetX += mid.x - last.x;
      view.offsetY += mid.y - last.y;
      pinch = now;
      last = mid;
      dragMoved = true;
      schedule();
      return;
    }

    const at = centre(e.touches);
    view.offsetX += at.x - last.x;
    view.offsetY += at.y - last.y;
    last = at;
    // A few pixels of slop, or every tap counts as a drag and never selects.
    if (touchStart && Math.hypot(at.x - touchStart.x, at.y - touchStart.y) > 8) {
      dragMoved = true;
    }
    schedule();
  }, { passive: false });

  canvas.addEventListener("touchend", (e) => {
    // Fingers still down: the gesture continues in a new shape, so re-measure
    // from what remains instead of carrying the old midpoint forward.
    if (e.touches.length > 0) {
      reseed(e.touches);
      return;
    }

    reseed(e.touches);
    if (dragMoved || !touchStart) return;

    const found = nearest(touchStart.x, touchStart.y, 22);
    if (found >= 0) {
      selected = found;
      // No cursor means no hover; showing the label for what was just tapped
      // is the closest equivalent.
      hover = found;
      schedule();
      dioxus.send({ type: "select", track_id: ids[found] });
    }
  }, { passive: true });

  // Android hands the gesture to the system mid-flight often enough that a
  // missing handler here leaves `touchCount` stale until the next touchstart.
  canvas.addEventListener("touchcancel", (e) => {
    reseed(e.touches);
    dragMoved = true;
  }, { passive: true });

  // Rust calls this when the selection changes elsewhere, the in-space list,
  // an "in space" button, a generated route.
  window.twoKhzSetSelected = (trackId) => {
    if (trackId === null || trackId === undefined) {
      selected = -1;
      schedule();
      return;
    }

    const found = byId.get(trackId);
    if (found === undefined) return;

    selected = found;

    // Bring it into view, but do not re-centre a point that is already
    // plainly visible.
    const x = screenX(found);
    const y = screenY(found);
    const margin = 40;
    // A hidden pane measures 0x0; leave the view alone rather than centring
    // on a rectangle that does not exist yet.
    if (width > 0 && height > 0 &&
        x < margin || y < margin || x > width - margin || y > height - margin) {
      view.offsetX += width / 2 - x;
      view.offsetY += height / 2 - y;
    }

    schedule();
  };

  // Rust calls this when a path is built. Ids only, tiny payload.
  window.twoKhzSetRoute = (trackIds) => {
    route = trackIds.map((id) => byId.get(id)).filter((i) => i !== undefined);
    schedule();
  };

  // Rust calls this after the block list changes. Selection and route are
  // dropped: their indices referred to the old point set.
  window.twoKhzReloadPoints = async () => {
    await loadPoints();
    selected = -1;
    hover = -1;
    route = [];
    schedule();
    // The indices changed, but the track id did not; re-resolve it rather
    // than leaving the map unmarked after every rebuild.
    const wanted = window.twoKhzSelected;
    if (wanted !== undefined && wanted !== null) window.twoKhzSetSelected(wanted);
  };

  window.addEventListener("resize", resize);

  // The canvas can resize without the window doing so: switching Explore panes
  // with `display: none` takes it from 0x0 to full with no resize event. `fit`
  // waits for a real rect for the same reason.
  let fitted = false;
  function measured() {
    const rect = canvas.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    if (!fitted) {
      fit();
      fitted = true;
    }
    resize();
  }

  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(measured).observe(canvas);
  }

  measured();

  // Rust may have chosen a track before this file finished loading; it leaves
  // the id here for exactly that case.
  function applyPendingSelection() {
    const wanted = window.twoKhzSelected;
    if (wanted !== undefined && wanted !== null) {
      window.twoKhzSetSelected(wanted);
    }
  }
  applyPendingSelection();

  // Keep the channel open so Rust can keep receiving selections.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
