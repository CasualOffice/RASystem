// Casual RAS host — remote-pointer overlay.
//
// A transparent, click-through, always-on-top canvas covering the screen. It draws the connected
// controller's pointer ("look here") from `pointer` events the Rust side emits to this window.
// Coordinates are normalized 0..=65535 over the shared frame (the whole display), so we map them
// straight to the overlay. This never receives input (the window is click-through) — purely visual.

const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;

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

// Latest pointer state; `at` is used to fade out if updates stop arriving.
let ptr = { x: 0, y: 0, visible: false, at: 0 };
const STALE_MS = 2000;

listen("pointer", (e) => {
  const p = e.payload;
  ptr = { x: p.x, y: p.y, visible: !!p.visible, at: performance.now() };
});

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

// ── Sharer-side drawing (ADR-097) ────────────────────────────────────────────────────────────────
// The Share role lets the SHARER draw on their OWN screen (over the captured display, which this
// overlay covers exactly). The Share view's toolbar drives `host-annot-mode`: when a tool is active
// the Rust side makes this overlay interactive (not click-through) so pointer events reach it; when
// off, the overlay is click-through again (view-only default, Inv 7). Each completed stroke is drawn
// locally (so the sharer sees it) AND sent to the viewer via `host_annotate`. Points are normalized
// 0..=65535 over this canvas = the captured display rect (the same space the viewer maps into). This
// UI is excluded from the shared capture, so the viewer never sees the overlay itself, only the
// mirrored strokes on their video.
let hostMode = { active: false, tool: 0, color: 0xff3b30 };
let hostStrokes = [];
let hostCur = null;
listen("host-annot-mode", (e) => {
  const p = e.payload || {};
  hostMode = { active: !!p.active, tool: p.tool | 0, color: (p.color | 0) & 0xffffff };
  cv.style.cursor = hostMode.active ? "crosshair" : "default";
  if (!hostMode.active) hostCur = null;
});
// Clear sharer markup when the whole share ends.
listen("share-active", (e) => {
  if (!e.payload) {
    hostStrokes = [];
    hostCur = null;
  }
});
// Undo/clear originate on the Share-view toolbar (not on this canvas); mirror them here so the
// sharer's local markup matches what the viewer sees.
listen("host-annot-op", (e) => {
  if (e.payload === "clear") hostStrokes = [];
  else if (e.payload === "undo") hostStrokes.pop();
});

// Normalize a pointer event to 0..=65535 over this overlay canvas (= the captured display rect).
function hostNormPt(e) {
  let nx = e.clientX / Math.max(1, window.innerWidth);
  let ny = e.clientY / Math.max(1, window.innerHeight);
  nx = Math.max(0, Math.min(1, nx));
  ny = Math.max(0, Math.min(1, ny));
  return [Math.round(nx * 65535), Math.round(ny * 65535)];
}
function sendHostStroke(s) {
  const points = s.points.filter(Boolean);
  if (!points.length) return;
  invoke("host_annotate", { op: "stroke", tool: s.tool, color: s.color, points }).catch(() => {});
}
cv.addEventListener("pointerdown", (e) => {
  if (!hostMode.active) return;
  cv.setPointerCapture(e.pointerId);
  hostCur = { tool: hostMode.tool, color: colorHex(hostMode.color), points: [hostNormPt(e)] };
});
cv.addEventListener("pointermove", (e) => {
  if (!hostCur) return;
  const n = hostNormPt(e);
  if (hostCur.tool === 0 || hostCur.tool === 1) {
    hostCur.points.push(n);
  } else {
    hostCur.points[1] = n; // arrow/rect keep only endpoints
  }
});
function endHostStroke() {
  if (!hostCur) return;
  if (hostCur.points.length > 1 || hostCur.tool === 0 || hostCur.tool === 1) {
    hostStrokes.push({ tool: hostCur.tool, color: hostCur.color, points: hostCur.points });
    if (hostStrokes.length > MAX_ANNOT_STROKES) hostStrokes.shift();
    sendHostStroke({ tool: hostCur.tool, color: hostMode.color, points: hostCur.points });
  }
  hostCur = null;
}
cv.addEventListener("pointerup", endHostStroke);
cv.addEventListener("pointercancel", endHostStroke);

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

function draw(now) {
  g.clearRect(0, 0, cv.width, cv.height);
  // Viewer annotations first, so the live pointer cursor draws on top of them.
  for (const s of annotStrokes) drawAnnot(s);
  // Sharer's own markup (drawn on their screen, mirrored to the viewer).
  for (const s of hostStrokes) drawAnnot(s);
  if (hostCur) drawAnnot(hostCur);
  const fresh = now - ptr.at < STALE_MS;
  if (ptr.visible && fresh) {
    const px = (ptr.x / 65535) * cv.width;
    const py = (ptr.y / 65535) * cv.height;
    const s = dpr;

    // Pulsing ring to draw the eye.
    const pulse = 1 + 0.25 * Math.sin(now / 200);
    g.beginPath();
    g.arc(px, py, 16 * s * pulse, 0, Math.PI * 2);
    g.strokeStyle = "rgba(255,59,48,0.9)";
    g.lineWidth = 3 * s;
    g.stroke();

    // Arrow cursor.
    g.beginPath();
    g.moveTo(px, py);
    g.lineTo(px + 22 * s, py + 8 * s);
    g.lineTo(px + 10 * s, py + 11 * s);
    g.lineTo(px + 8 * s, py + 22 * s);
    g.closePath();
    g.fillStyle = "#ff3b30";
    g.fill();
    g.strokeStyle = "#fff";
    g.lineWidth = 1.5 * s;
    g.stroke();

    // Label.
    g.font = `${13 * s}px ui-sans-serif, system-ui, sans-serif`;
    g.fillStyle = "rgba(0,0,0,0.6)";
    g.fillRect(px + 24 * s, py + 18 * s, 58 * s, 20 * s);
    g.fillStyle = "#fff";
    g.fillText(controlling ? "control" : "viewer", px + 30 * s, py + 32 * s);
  }
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);
