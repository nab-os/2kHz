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

    for (const [index, colour] of [[selected, "#ffffff"], [hover, "#7aa2f7"]]) {
      if (index < 0) continue;
      const [x, y] = toScreen(index);
      ctx.beginPath();
      ctx.arc(x, y, 7, 0, Math.PI * 2);
      ctx.strokeStyle = colour;
      ctx.lineWidth = 2;
      ctx.stroke();
    }
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
  };

  window.addEventListener("resize", resize);
  fit();
  resize();

  // Keep the channel open so Rust can keep receiving selections.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
