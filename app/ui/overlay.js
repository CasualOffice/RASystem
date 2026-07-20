// Casual RAS host — remote-pointer overlay.
//
// A transparent, click-through, always-on-top canvas covering the screen. It draws the connected
// controller's pointer ("look here") from `pointer` events the Rust side emits to this window.
// Coordinates are normalized 0..=65535 over the shared frame (the whole display), so we map them
// straight to the overlay. This never receives input (the window is click-through) — purely visual.

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

// Latest pointer state; `at` is used to fade out if updates stop arriving.
let ptr = { x: 0, y: 0, visible: false, at: 0 };
const STALE_MS = 2000;

listen("pointer", (e) => {
  const p = e.payload;
  ptr = { x: p.x, y: p.y, visible: !!p.visible, at: performance.now() };
});

function draw(now) {
  g.clearRect(0, 0, cv.width, cv.height);
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
    g.fillText("viewer", px + 30 * s, py + 32 * s);
  }
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);
