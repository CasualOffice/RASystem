// Casual RAS host — always-on-top overlay.
//
// A transparent, click-through, always-on-top canvas covering the screen. It renders the
// always-visible "remote active" indicator badge (Invariant 7) and mirrors the viewer's annotation
// strokes onto the host's shared display. This never receives input (the window is click-through) —
// purely visual.
//
// Does NOT render a separate remote-pointer ("look here") cursor: the host's own OS cursor is already
// baked into the captured video (one cursor, ADR-100), so drawing a second cursor-like arrow here read
// as a confusing "multi cursor" artifact on the host (reported on Linux) — removed.

const { listen } = window.__TAURI__.event;

// Always-visible "remote active" indicator (Invariant 7). The in-app indicator + Stop live in the main
// window, which the host user can minimize / move / occlude; this badge lives on the always-on-top,
// maximized overlay covering the shared display, so an active session is ALWAYS visible on that display
// regardless of the main window's state. Driven by the same share-viewer / share-control / share-active
// events as the in-app badge (Tauri `emit` broadcasts to every window, including this one).
const badge = document.getElementById("active-badge");
const badgeText = document.getElementById("active-text");
let viewing = false;
let controlling = false;
function renderBadge() {
  if (controlling) badgeText.textContent = "REMOTE CONTROL ACTIVE";
  else if (viewing) badgeText.textContent = "REMOTE VIEWING ACTIVE";
  badge.classList.toggle("on", viewing || controlling);
}
listen("share-viewer", (e) => {
  viewing = !!e.payload;
  renderBadge();
});
listen("share-control", (e) => {
  controlling = !!e.payload;
  renderBadge();
});
listen("share-active", (e) => {
  // The whole share ended (or failed to start) — clear both, so the badge never outlives the session.
  if (!e.payload) {
    viewing = false;
    controlling = false;
    renderBadge();
  }
});

const cv = document.getElementById("ptr");
// `alpha: true` is the 2D default, but set it explicitly so the backing store is guaranteed
// transparent across engines — the overlay must composite over the host desktop, never paint white.
const g = cv.getContext("2d", { alpha: true });

let dpr = 1;
function fit() {
  dpr = window.devicePixelRatio || 1;
  cv.width = Math.round(window.innerWidth * dpr);
  cv.height = Math.round(window.innerHeight * dpr);
}
fit();
window.addEventListener("resize", fit);

// ── Annotations mirrored from the viewer (ADR-097) ──────────────────────────────────────────────
// The viewer's markup, rendered on the host's shared display. Points are normalized 0..=65535 over
// the shared frame (same space as the pointer), so they map straight onto the overlay canvas.
let annotStrokes = [];
const MAX_ANNOT_STROKES = 256; // bound host-side memory; oldest drop first
const colorHex = (n) => "#" + ((n & 0xffffff) >>> 0).toString(16).padStart(6, "0");
listen("annotate", (e) => {
  const p = e.payload || {};
  if (p.op === "clear") annotStrokes = [];
  else if (p.op === "undo") annotStrokes.pop();
  else if (p.op === "stroke") {
    annotStrokes.push({ tool: p.tool | 0, color: colorHex(p.color | 0), points: p.points || [] });
    if (annotStrokes.length > MAX_ANNOT_STROKES) annotStrokes.shift();
  }
});
// Clear markup when the whole share ends, so it never outlives the session.
listen("share-active", (e) => {
  if (!e.payload) annotStrokes = [];
});

// Draw one annotation stroke (tool: 0=pen, 1=highlighter, 2=arrow, 3=rect). Coords normalized.
function drawAnnot(s) {
  const pts = s.points;
  if (!pts || !pts.length) return;
  const X = (n) => (n / 65535) * cv.width;
  const Y = (n) => (n / 65535) * cv.height;
  g.strokeStyle = s.color;
  g.lineJoin = "round";
  g.lineCap = "round";
  if (s.tool === 1) {
    g.globalAlpha = 0.35;
    g.lineWidth = 18 * dpr;
  } else {
    g.globalAlpha = 1;
    g.lineWidth = 3 * dpr;
  }
  const a = pts[0];
  const b = pts[pts.length - 1];
  g.beginPath();
  if (s.tool === 3) {
    g.strokeRect(X(a[0]), Y(a[1]), X(b[0]) - X(a[0]), Y(b[1]) - Y(a[1]));
  } else if (s.tool === 2) {
    g.moveTo(X(a[0]), Y(a[1]));
    g.lineTo(X(b[0]), Y(b[1]));
    g.stroke();
    const ang = Math.atan2(Y(b[1]) - Y(a[1]), X(b[0]) - X(a[0]));
    const head = 16 * dpr;
    g.beginPath();
    g.moveTo(X(b[0]), Y(b[1]));
    g.lineTo(X(b[0]) - head * Math.cos(ang - Math.PI / 6), Y(b[1]) - head * Math.sin(ang - Math.PI / 6));
    g.moveTo(X(b[0]), Y(b[1]));
    g.lineTo(X(b[0]) - head * Math.cos(ang + Math.PI / 6), Y(b[1]) - head * Math.sin(ang + Math.PI / 6));
    g.stroke();
  } else {
    g.moveTo(X(pts[0][0]), Y(pts[0][1]));
    for (let i = 1; i < pts.length; i++) g.lineTo(X(pts[i][0]), Y(pts[i][1]));
    g.stroke();
  }
  g.globalAlpha = 1;
}

function draw() {
  g.clearRect(0, 0, cv.width, cv.height);
  // Viewer annotations onto the host's shared display. No remote-pointer cursor is drawn here — the
  // host's own OS cursor (baked into the capture) is the single cursor.
  for (const s of annotStrokes) drawAnnot(s);
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);
