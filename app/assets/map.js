// Canvas map for the suggestion space.
//
// Owns rendering and all pan/zoom/hover, so dragging never crosses into Rust.
// Points arrive once as binary; only selections go back.

(async () => {
  const canvas = document.getElementById("map");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");

  const PALETTE = [
    "#7aa2f7", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7",
    "#7dcfff", "#ff9e64", "#73daca", "#c0caf5", "#b4f9f8",
  ];

  // Reloadable: hiding an artist removes their points, and the payload is
  // rebuilt server-side, so the views have to be rebound rather than patched.
  let meta, n, ids, xy, genre;

  async function loadPoints() {
    meta = await (await fetch("/points/meta")).json();
    const buffer = await (await fetch("/points/data")).arrayBuffer();
    n = meta.n;
    // Views match the layout documented in map.rs.
    ids = new Float64Array(buffer, 0, n);
    xy = new Float32Array(buffer, 8 * n, 2 * n);
    genre = new Uint16Array(buffer, 16 * n, n);
  }

  await loadPoints();

  let view = { scale: 1, offsetX: 0, offsetY: 0 };
  let hover = -1;
  let selected = -1;
  let route = [];
  let routeIndex = new Map();

  function resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    canvas.width = rect.width * dpr;
    canvas.height = rect.height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  function fit() {
    const rect = canvas.getBoundingClientRect();
    const spanX = Math.max(meta.bounds.max_x - meta.bounds.min_x, 1e-6);
    const spanY = Math.max(meta.bounds.max_y - meta.bounds.min_y, 1e-6);
    const pad = 30;
    view.scale = Math.min((rect.width - pad * 2) / spanX, (rect.height - pad * 2) / spanY);
    view.offsetX = pad - meta.bounds.min_x * view.scale;
    view.offsetY = pad - meta.bounds.min_y * view.scale;
  }

  const toScreen = (i) => [
    xy[i * 2] * view.scale + view.offsetX,
    xy[i * 2 + 1] * view.scale + view.offsetY,
  ];

  function draw() {
    const rect = canvas.getBoundingClientRect();
    ctx.clearRect(0, 0, rect.width, rect.height);

    if (!meta.has_layout) {
      ctx.fillStyle = "#8b90a3";
      ctx.font = "13px system-ui, sans-serif";
      ctx.fillText("No layout yet, run: uv run qsuggest layout", 20, 30);
      return;
    }

    // Points. Dimmed when a route is showing, so the path reads clearly.
    const dim = route.length > 0;
    const radius = Math.max(1.5, Math.min(4, view.scale * 0.08));
    for (let i = 0; i < n; i++) {
      const [x, y] = toScreen(i);
      if (x < -10 || y < -10 || x > rect.width + 10 || y > rect.height + 10) continue;
      ctx.beginPath();
      ctx.arc(x, y, radius, 0, Math.PI * 2);
      ctx.fillStyle = PALETTE[genre[i] % PALETTE.length];
      ctx.globalAlpha = dim && !routeIndex.has(i) ? 0.18 : 0.8;
      ctx.fill();
    }
    ctx.globalAlpha = 1;

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
        ctx.arc(x, y, 5, 0, Math.PI * 2);
        ctx.fillStyle = k === 0 ? "#9ece6a" : k === route.length - 1 ? "#f7768e" : "#ffffff";
        ctx.fill();
      });
    }

    // Markers last, selection under hover, so the selected track reads as one
    // marker rather than two.
    if (selected >= 0) marker(selected, "#ffffff", rect, true);
    if (hover >= 0 && hover !== selected) marker(hover, "#7aa2f7", rect, false);
  }

  const TAU = Math.PI * 2;

  /// A point worth looking at: halo, ring, core, and what it actually is.
  function marker(index, colour, rect, persistent) {
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
    if (text) label(text, x, y, colour, rect);
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
  function label(text, x, y, colour, rect) {
    ctx.font = "12px system-ui, -apple-system, sans-serif";
    const padX = 8;
    const h = 22;
    const maxWidth = Math.min(280, rect.width - 16);
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
    if (lx + w > rect.width - 6) lx = x - w - 16;
    if (lx < 6) lx = 6;
    if (ly < 6) ly = y + 16;
    if (ly + h > rect.height - 6) ly = rect.height - h - 6;

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

  // Linear scan for the nearest point. At tens of thousands of points this is
  // well under a frame; swap in a quadtree only if it ever stops being.
  function nearest(px, py, maxDistance = 14) {
    let best = -1;
    let bestDistance = maxDistance * maxDistance;
    for (let i = 0; i < n; i++) {
      const [x, y] = toScreen(i);
      const d = (x - px) * (x - px) + (y - py) * (y - py);
      if (d < bestDistance) {
        bestDistance = d;
        best = i;
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
      draw();
      return;
    }
    const found = nearest(e.offsetX, e.offsetY);
    if (found !== hover) {
      hover = found;
      canvas.style.cursor = found >= 0 ? "pointer" : "grab";
      draw();
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
      draw();
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
    draw();
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
      draw();
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
    draw();
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
      draw();
      dioxus.send({ type: "select", track_id: ids[found] });
    }
  }, { passive: true });

  // Android hands the gesture to the system mid-flight often enough that a
  // missing handler here leaves `touchCount` stale until the next touchstart.
  canvas.addEventListener("touchcancel", (e) => {
    reseed(e.touches);
    dragMoved = true;
  }, { passive: true });

  canvas.addEventListener("touchmove", (e) => {
    e.preventDefault();

    if (e.touches.length >= 2) {
      const now = spread(e.touches);
      const mid = centre(e.touches);
      if (pinch > 0 && now > 0) {
        const factor = now / pinch;
        view.offsetX = mid.x - (mid.x - view.offsetX) * factor;
        view.offsetY = mid.y - (mid.y - view.offsetY) * factor;
        view.scale *= factor;
      }
      pinch = now;
      last = mid;
      dragMoved = true;
      draw();
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
    draw();
  }, { passive: false });

  canvas.addEventListener("touchend", (e) => {
    if (e.touches.length < 2) pinch = 0;
    if (dragMoved || !touchStart) return;

    const found = nearest(touchStart.x, touchStart.y, 22);
    if (found >= 0) {
      selected = found;
      // No cursor means no hover; showing the label for what was just tapped
      // is the closest equivalent.
      hover = found;
      draw();
      dioxus.send({ type: "select", track_id: ids[found] });
    }
  }, { passive: true });

  // Rust calls this when the selection changes elsewhere, the in-space list,
  // an "in space" button, a generated route.
  window.qsuggestSetSelected = (trackId) => {
    if (trackId === null || trackId === undefined) {
      selected = -1;
      draw();
      return;
    }

    let found = -1;
    for (let i = 0; i < n; i++) {
      if (ids[i] === trackId) {
        found = i;
        break;
      }
    }
    if (found < 0) return;

    selected = found;

    // Bring it into view, but do not re-centre a point that is already
    // plainly visible.
    const rect = canvas.getBoundingClientRect();
    const [x, y] = toScreen(found);
    const margin = 40;
    if (x < margin || y < margin || x > rect.width - margin || y > rect.height - margin) {
      view.offsetX += rect.width / 2 - x;
      view.offsetY += rect.height / 2 - y;
    }

    draw();
  };

  // Rust calls this when a path is built. Ids only, tiny payload.
  window.qsuggestSetRoute = (trackIds) => {
    const position = new Map();
    for (let i = 0; i < n; i++) position.set(ids[i], i);
    route = trackIds.map((id) => position.get(id)).filter((i) => i !== undefined);
    routeIndex = new Map(route.map((i) => [i, true]));
    draw();
  };

  // Rust calls this after the block list changes. Selection and route are
  // dropped: their indices referred to the old point set.
  window.qsuggestReloadPoints = async () => {
    await loadPoints();
    selected = -1;
    hover = -1;
    route = [];
    routeIndex = new Map();
    draw();
    // The indices changed, but the track id did not; re-resolve it rather
    // than leaving the map unmarked after every rebuild.
    const wanted = window.qsuggestSelected;
    if (wanted !== undefined && wanted !== null) window.qsuggestSetSelected(wanted);
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
    const wanted = window.qsuggestSelected;
    if (wanted !== undefined && wanted !== null) {
      window.qsuggestSetSelected(wanted);
    }
  }
  applyPendingSelection();

  // Keep the channel open so Rust can keep receiving selections.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
