// Casual RAS — unified desktop app (ADR-062).
//
// One webview, two roles chosen from a home screen:
//   • Connect (viewer): receive encoded H.264 access units on a binary Tauri Channel, decode each
//     with a WebCodecs VideoDecoder, and render to a canvas. No pixels ever cross JSON IPC.
//   • Share (host): start/stop sharing, approve/deny a viewer, watch the live indicator. The Rust
//     side emits share-ticket / share-status / share-active / share-viewer / consent-request events.
//
// Frame blob = the canonical ras_core::frame_channel header + Annex-B payload (little-endian):
//   magic:u32("RAS1") | flags:u8(bit0=key) | pad:u8 | pad:u16 | frame_id:u64 | captured_at_us:u64
// (24 bytes). Ids/timestamps are read as BigInt — a JS number corrupts u64 past 2^53.

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ── View router ────────────────────────────────────────────────────────────────────────────────
const views = {
  home: document.getElementById("home"),
  share: document.getElementById("share-view"),
  connect: document.getElementById("connect-view"),
};

function showView(name) {
  for (const [k, el] of Object.entries(views)) el.hidden = k !== name;
}

document.getElementById("go-connect").addEventListener("click", () => showView("connect"));
document.getElementById("go-share").addEventListener("click", () => {
  showView("share");
  startSharing();
});
document.querySelectorAll("[data-home]").forEach((b) =>
  b.addEventListener("click", () => {
    // Leaving a role tears its session down so nothing keeps running in the background.
    stopSession();
    stopSharing();
    showView("home");
  }),
);

// ── Signed auto-update (ADR-078) ───────────────────────────────────────────────────────────────────
// User-initiated, two-click: first click checks; if an update exists the button becomes "Install &
// restart" so applying it is an explicit second choice (Inv 1 — the local user decides; nothing is
// installed silently). The download is signature-verified in Rust against the embedded key.
(function () {
  const btn = document.getElementById("check-updates");
  const status = document.getElementById("update-status");
  if (!btn) return;
  let pendingVersion = null;
  btn.addEventListener("click", async () => {
    if (pendingVersion) {
      status.textContent = "Installing… the app will restart.";
      btn.disabled = true;
      try {
        await invoke("install_update"); // app relaunches on success
      } catch (e) {
        status.textContent = "Install failed.";
        btn.disabled = false;
        console.warn("update install:", e);
      }
      return;
    }
    status.textContent = "Checking…";
    try {
      const version = await invoke("check_for_updates");
      if (!version) {
        status.textContent = "You're up to date.";
        return;
      }
      pendingVersion = version;
      status.textContent = `Version ${version} available.`;
      btn.textContent = `Install ${version} & restart`;
    } catch (e) {
      // No key/endpoint provisioned yet, or the endpoint is unreachable — honest, not a crash.
      status.textContent = "Update check unavailable.";
      console.warn("update check:", e);
    }
  });
})();

// ── Video decode (Connect role) ──────────────────────────────────────────────────────────────────
const HEADER_LEN = 24;
const FRAME_MAGIC = 0x52415331; // "RAS1" big-endian — a frame blob
const CONFIG_MAGIC = 0x52434647; // "RCFG" big-endian — the one-shot stream-config blob
const FLAG_KEYFRAME = 0x01;
const canvas = document.getElementById("screen");
const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
const hud = document.getElementById("hud");

let decoder = null;
let sawKeyframe = false;
let decoded = 0;
let received = 0;
let lastId = null;
let gaps = 0;
let t0 = performance.now();

function toBytes(msg) {
  if (msg instanceof ArrayBuffer) return new Uint8Array(msg);
  if (ArrayBuffer.isView(msg)) return new Uint8Array(msg.buffer, msg.byteOffset, msg.byteLength);
  if (Array.isArray(msg)) return Uint8Array.from(msg);
  throw new Error("unexpected channel payload type");
}

function buildDecoder(cfg) {
  canvas.width = cfg.width;
  canvas.height = cfg.height;
  const dec = new VideoDecoder({
    output: (frame) => {
      // Draw then release promptly (tiny pool; latency over buffering — priority #2).
      ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
      frame.close();
      decoded++;
    },
    error: (e) => {
      hud.textContent = "decoder error → resetting: " + e.message;
      sawKeyframe = false;
      try { dec.reset(); } catch (_) {}
      dec.configure(decoderConfig(cfg));
      invoke("request_keyframe");
    },
  });
  dec.configure(decoderConfig(cfg));
  return dec;
}

function decoderConfig(cfg) {
  // No `description` ⇒ Annex-B input (our encoder re-sends SPS/PPS in-band on every IDR).
  return {
    codec: cfg.codec,
    codedWidth: cfg.width,
    codedHeight: cfg.height,
    optimizeForLatency: true,
  };
}

function onConfig(bytes) {
  const json = new TextDecoder().decode(bytes.subarray(4));
  const cfg = JSON.parse(json);
  decoder = buildDecoder(cfg);
  hud.textContent = `viewing ${cfg.width}×${cfg.height} @ ${cfg.fps} · ${cfg.codec}`;
  // Infinite-GOP: the lone startup IDR may predate this decoder. Ask for a fresh one now, and keep
  // asking until we actually decode a frame (covers the startup race + a dropped first keyframe).
  invoke("request_keyframe");
  const kick = setInterval(() => {
    if (decoded > 0) clearInterval(kick);
    else invoke("request_keyframe");
  }, 500);
}

function onMessage(msg) {
  const bytes = toBytes(msg);
  if (bytes.byteLength < 4) return;
  const magic = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, true);
  if (magic === CONFIG_MAGIC) return onConfig(bytes);
  if (magic === FRAME_MAGIC) return onFrame(bytes);
  // otherwise: desync/garbage — drop
}

function onFrame(bytes) {
  received++;
  if (bytes.byteLength <= HEADER_LEN) return;
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const flags = bytes[4];
  const isKey = (flags & FLAG_KEYFRAME) === FLAG_KEYFRAME;
  const frameId = dv.getBigUint64(8, true);
  const tsUs = dv.getBigUint64(16, true);
  const payload = bytes.subarray(HEADER_LEN);

  // Track loss (gap in monotonic ids) for the HUD; the real reaction lives host-side.
  const id = Number(frameId);
  if (lastId !== null && id > lastId + 1) gaps += id - lastId - 1;
  lastId = id;

  if (!decoder || decoder.state !== "configured") return;
  // A decoder must start on a keyframe; drop deltas until the first IDR arrives.
  if (!sawKeyframe) {
    if (!isKey) return;
    sawKeyframe = true;
  }

  try {
    decoder.decode(
      new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: Number(tsUs),
        data: payload,
      }),
    );
  } catch (e) {
    hud.textContent = "decode() threw: " + e.message;
  }

  if (received % 30 === 0) {
    const dt = (performance.now() - t0) / 1000;
    hud.textContent =
      `render ${(decoded / dt).toFixed(1)} fps · rx ${received} · decoded ${decoded} · ` +
      `gaps ${gaps} · id ${id}`;
  }
}

// ── Connect (viewer) session lifecycle ───────────────────────────────────────────────────────────
const ticketInput = document.getElementById("ticket");
const connectBtn = document.getElementById("connect");
const stopBtn = document.getElementById("stop");
const controlBtn = document.getElementById("control");
const macMod = document.getElementById("macmod");
const macModCb = document.getElementById("macmod-cb");
const banner = document.getElementById("banner");
const reconnectBanner = document.getElementById("reconnect-banner");
const connStats = document.getElementById("conn-stats");

// Reconnection state from the Rust lifecycle drain (task #22 / ADR-091): show a "reconnecting…" banner
// while the controller re-dials a dropped transport; hide it once resumed (or the session ends). The
// video itself keeps its last frame until fresh frames + an IDR arrive on resume (no black screen).
listen("conn-status", (e) => {
  reconnectBanner.hidden = e.payload !== "reconnecting";
  // A host-initiated end (emergency stop / revoke / peer disconnect, surfaced as "ended") must tear the
  // viewer UI down — otherwise the chat/clipboard/file panels stay enabled on a dead session and give
  // phantom "sent" feedback (Inv 7 honesty). `setLive` is hoisted, so calling it here is fine.
  if (e.payload === "ended") {
    setLive(false);
  }
});

// Connection-quality readout (path · RTT · loss · fps · bandwidth), updated each host stats tick.
listen("conn-quality", (e) => {
  const q = e.payload;
  connStats.hidden = false;
  const mbps = (q.kbps / 1000).toFixed(1);
  connStats.textContent = `${q.path} · ${q.rtt_ms} ms · ${q.loss_pct.toFixed(1)}% loss · ${q.fps} fps · ${mbps} Mbps`;
});

// Cmd↔Ctrl primary-modifier remap (ADR-075). Explicit, user-visible, default OFF. When on, the
// operator's ⌘ is transmitted as Ctrl on the remote (and Ctrl as ⌘) — scoped to ONLY the primary
// shortcut modifier so a Mac keyboard's muscle memory works against a Windows/Linux host. It is
// purely controller-side: it rewrites which HID usage + modifier bit we send; the host is unchanged
// and still authorizes every keystroke identically (Inv 15). Never silent — the checkbox is visible.
let swapPrimaryMod = false;
if (macModCb) {
  macModCb.addEventListener("change", () => {
    swapPrimaryMod = macModCb.checked;
  });
}

// Clipboard-sharing opt-in (Share role, default OFF). Clipboard has no per-message consent gate, so a
// viewer only gets the clipboard capability when the host ticks this BEFORE the viewer connects (the
// grant's capabilities are fixed at issue time). Purely a host-side authorization choice (Inv 1/7).
const shareClipboardCb = document.getElementById("share-clipboard-cb");
if (shareClipboardCb) {
  shareClipboardCb.addEventListener("change", () => {
    invoke("set_clipboard_allowed", { allowed: shareClipboardCb.checked }).catch(() => {});
  });
}

let active = false; // a viewer session is live

function resetState() {
  try { decoder && decoder.close(); } catch (_) {}
  decoder = null;
  sawKeyframe = false;
  decoded = 0;
  received = 0;
  lastId = null;
  gaps = 0;
  t0 = performance.now();
  annotations.clear();
}

function setLive(isLive) {
  active = isLive;
  banner.hidden = !isLive;
  if (!isLive) {
    reconnectBanner.hidden = true; // clear the reconnecting banner when the session ends
    connStats.hidden = true; // and the connection-stats readout
  }
  connectBtn.disabled = isLive;
  ticketInput.disabled = isLive;
  stopBtn.disabled = !isLive;
  annotations.show(isLive);
  setControlling(false);
  controlBtn.disabled = !isLive;
  controlBtn.textContent = "Take control";
  if (macMod) macMod.hidden = !isLive;
  chat.setSessionLive(isLive); // chat/clipboard are usable only while the viewer session is live
  files.setViewerLive(isLive); // the viewer can send a file only while its session is live
}

async function startSession(ticket) {
  if (!("VideoDecoder" in window)) {
    hud.textContent = "WebCodecs VideoDecoder unavailable in this webview.";
    return;
  }
  resetState();
  const channel = new Channel();
  channel.onmessage = onMessage;
  hud.textContent = "connecting…";
  try {
    await invoke("connect_to_host", { ticket, onFrame: channel });
  } catch (e) {
    hud.textContent = "connect failed: " + e;
    setLive(false);
    return;
  }
  setLive(true);
  hud.textContent = "session up — waiting for stream config…";
}

async function stopSession() {
  try { await invoke("disconnect"); } catch (_) {}
  resetState();
  setLive(false);
  hud.textContent = "Disconnected. Paste a ticket and press Connect.";
}

connectBtn.addEventListener("click", () => {
  const ticket = ticketInput.value.trim();
  if (!ticket) {
    hud.textContent = "Paste a connection ticket first.";
    return;
  }
  startSession(ticket);
});

stopBtn.addEventListener("click", stopSession);

ticketInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !active) connectBtn.click();
});

window.addEventListener("beforeunload", () => {
  invoke("disconnect");
  invoke("stop_sharing");
});

// ── Share (host) role ────────────────────────────────────────────────────────────────────────────
const shareTicket = document.getElementById("share-ticket");
const shareCopy = document.getElementById("share-copy");
const shareStatus = document.getElementById("share-status");
const shareIndicator = document.getElementById("share-indicator");
const shareStop = document.getElementById("share-stop");
const consent = document.getElementById("consent");
const peerEl = document.getElementById("peer");

let sharing = false;

function startSharing() {
  if (sharing) return;
  sharing = true;
  shareTicket.value = "";
  shareStatus.textContent = "Preparing…";
  invoke("start_sharing").catch((e) => {
    shareStatus.textContent = String(e);
    sharing = false;
  });
}

function stopSharing() {
  if (!sharing) return;
  sharing = false;
  invoke("stop_sharing").catch(() => {});
  consent.hidden = true;
}

shareStop.addEventListener("click", () => {
  stopSharing();
  showView("home");
});

shareCopy.addEventListener("click", async () => {
  shareTicket.select();
  try {
    await navigator.clipboard.writeText(shareTicket.value);
    shareCopy.textContent = "Copied";
    setTimeout(() => (shareCopy.textContent = "Copy"), 1200);
  } catch (_) {
    // Fallback: the text is already selected for a manual Cmd-C.
  }
});

listen("share-ticket", (e) => { shareTicket.value = e.payload; });
listen("share-status", (e) => { shareStatus.textContent = e.payload; });
listen("share-viewer", (e) => {
  const live = !!e.payload;
  shareIndicator.textContent = live ? "● REMOTE VIEWING ACTIVE" : "● IDLE";
  shareIndicator.className = live ? "indicator live" : "indicator idle";
  // A connected viewer means a live session on the host side — chat/clipboard become usable.
  chat.setSessionLive(live);
  files.setHostLive(live); // host can receive a file while a viewer is connected
});
listen("share-active", (e) => {
  if (!e.payload) {
    shareIndicator.className = "indicator idle";
    chat.setSessionLive(false); // sharing torn down entirely — no session
    files.setHostLive(false); // no session — dismiss any file offer/notice
  }
});

// Local consent (Invariant 1: the local user authorizes each viewer).
listen("consent-request", (e) => {
  peerEl.textContent = e.payload || "unknown";
  consent.hidden = false;
});
listen("consent-closed", () => { consent.hidden = true; });
document.getElementById("allow").addEventListener("click", () => {
  consent.hidden = true;
  invoke("respond_consent", { allow: true });
});
document.getElementById("deny").addEventListener("click", () => {
  consent.hidden = true;
  invoke("respond_consent", { allow: false });
});

// Control-lease consent (Phase 3, Invariant 1): a distinct, higher-stakes Allow/Deny for OS input.
const controlConsent = document.getElementById("control-consent");
const controlCaps = document.getElementById("control-caps");
listen("control-consent-request", (e) => {
  const caps = Array.isArray(e.payload) ? e.payload.join(", ") : "input";
  controlCaps.textContent = caps || "input";
  controlConsent.hidden = false;
});
listen("control-consent-closed", () => { controlConsent.hidden = true; });
document.getElementById("control-allow").addEventListener("click", () => {
  controlConsent.hidden = true;
  invoke("respond_control_consent", { allow: true });
});
document.getElementById("control-deny").addEventListener("click", () => {
  controlConsent.hidden = true;
  invoke("respond_control_consent", { allow: false });
});
// The sharer's indicator reflects whether the viewer currently has OS control.
listen("share-control", (e) => {
  const controlling = !!e.payload;
  shareIndicator.textContent = controlling ? "● REMOTE CONTROL ACTIVE" : "● REMOTE VIEWING ACTIVE";
  shareIndicator.className = controlling ? "indicator control" : "indicator live";
});

// ── Remote pointer (this viewer's cursor → shown on the host's screen) ─────────────────────────
// Track the cursor over the shared video and stream its position to the host (throttled). Purely
// visual — not remote control. Coordinates normalized to the *video content* rect (object-fit).
let lastPointerAt = 0;

function videoContentRect() {
  const box = canvas.getBoundingClientRect();
  const vw = canvas.width, vh = canvas.height;
  if (!vw || !vh) return { left: box.left, top: box.top, width: box.width, height: box.height };
  const scale = Math.min(box.width / vw, box.height / vh);
  const w = vw * scale, h = vh * scale;
  return { left: box.left + (box.width - w) / 2, top: box.top + (box.height - h) / 2, width: w, height: h };
}

function trackPointer(e) {
  if (!active) return;
  const now = performance.now();
  if (now - lastPointerAt < 40) return; // ~25 Hz is plenty for a pointer
  lastPointerAt = now;
  const r = videoContentRect();
  if (r.width <= 0 || r.height <= 0) return;
  let nx = (e.clientX - r.left) / r.width;
  let ny = (e.clientY - r.top) / r.height;
  const inside = nx >= 0 && nx <= 1 && ny >= 0 && ny <= 1;
  nx = Math.min(1, Math.max(0, nx));
  ny = Math.min(1, Math.max(0, ny));
  invoke("send_pointer", {
    x: Math.round(nx * 65535),
    y: Math.round(ny * 65535),
    visible: inside,
  });
}

window.addEventListener("pointermove", trackPointer);
window.addEventListener("pointerleave", () => {
  if (active) invoke("send_pointer", { x: 0, y: 0, visible: false });
});

// ── Remote control (Phase 3): forward this viewer's clicks/keys to the host's OS ─────────────────
// Only when we hold the lease (`controlling`). The host re-checks every event (lease/generation/seq/
// capability, Inv 15) — this is a request, never authority. Coordinates are normalized to the video
// content rect, exactly like the visual pointer.
let controlling = false;
let lastMoveAt = 0;
// Last lock state we told the host, so we only send on a change (ADR-074). null = unknown → the next
// key event resyncs from scratch (e.g. right after taking control).
let lastCaps = null;
let lastNum = null;

function setControlling(on) {
  controlling = on && active;
  if (!controlling) {
    lastCaps = null;
    lastNum = null;
  }
  if (controlBtn) {
    controlBtn.textContent = controlling ? "Controlling — click to stop" : "Take control";
    controlBtn.classList.toggle("armed", controlling);
  }
  if (banner) {
    banner.textContent = controlling ? "● CONTROLLING remote screen" : "● LIVE — viewing remote screen";
  }
}

// Normalized 0..=65535 of the video content rect, or null if outside it.
function normInput(e) {
  const r = videoContentRect();
  if (r.width <= 0 || r.height <= 0) return null;
  const nx = (e.clientX - r.left) / r.width;
  const ny = (e.clientY - r.top) / r.height;
  if (nx < 0 || nx > 1 || ny < 0 || ny > 1) return null;
  return { nx: Math.round(nx * 65535), ny: Math.round(ny * 65535) };
}

function modifierBits(e) {
  let bits = (e.shiftKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.altKey ? 4 : 0) | (e.metaKey ? 8 : 0);
  // When the ⌘↔Ctrl swap is on, swap the Ctrl (0x02) and Cmd (0x08) flag bits so the modifier state
  // the host applies to each keystroke matches the swapped modifier-key HID usages below.
  if (swapPrimaryMod) {
    const ctrl = bits & 0x02;
    const cmd = bits & 0x08;
    bits = (bits & ~0x0a) | (ctrl ? 0x08 : 0) | (cmd ? 0x02 : 0);
  }
  return bits;
}

// Swap the left/right Control (0xe0/0xe4) and GUI/⌘ (0xe3/0xe7) HID usages when the toggle is on.
// Only the primary shortcut modifier is remapped; every other key passes through untouched.
function remapHid(hid) {
  if (!swapPrimaryMod) return hid;
  switch (hid) {
    case 0xe0: return 0xe3;
    case 0xe3: return 0xe0;
    case 0xe4: return 0xe7;
    case 0xe7: return 0xe4;
    default: return hid;
  }
}

// JS KeyboardEvent.code → USB-HID Keyboard/Keypad usage (page 0x07). Unmapped keys are ignored.
function codeToHid(code) {
  if (/^Key[A-Z]$/.test(code)) return 0x04 + (code.charCodeAt(3) - 65);
  if (/^Digit[1-9]$/.test(code)) return 0x1e + (code.charCodeAt(5) - 49);
  if (code === "Digit0") return 0x27;
  return {
    Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c,
    Minus: 0x2d, Equal: 0x2e, BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31,
    Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36, Period: 0x37, Slash: 0x38,
    CapsLock: 0x39, ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52,
    ControlLeft: 0xe0, ShiftLeft: 0xe1, AltLeft: 0xe2, MetaLeft: 0xe3,
    ControlRight: 0xe4, ShiftRight: 0xe5, AltRight: 0xe6, MetaRight: 0xe7,
  }[code];
}

controlBtn.addEventListener("click", async () => {
  if (!active) return;
  if (controlling) { setControlling(false); return; }
  controlBtn.textContent = "Requesting…";
  invoke("request_control");
  // Poll until the host grants (its owner must Allow) — up to ~15 s, then give up.
  for (let i = 0; i < 60; i++) {
    await new Promise((r) => setTimeout(r, 250));
    let held = false;
    try { held = await invoke("is_controlling"); } catch (_) {}
    if (held) { setControlling(true); return; }
    if (!active) return;
  }
  setControlling(false);
});

// Pointer + keyboard forwarding. All guarded on `controlling`; when active they preventDefault so the
// webview itself doesn't act on the input.
window.addEventListener("pointermove", (e) => {
  if (!controlling) return;
  const now = performance.now();
  if (now - lastMoveAt < 8) return; // ~120 Hz cap
  lastMoveAt = now;
  const p = normInput(e);
  if (p) invoke("input_pointer_move", { nx: p.nx, ny: p.ny });
});
function forwardButton(e, down) {
  if (!controlling) return;
  const p = normInput(e);
  if (!p) return;
  e.preventDefault();
  const button = e.button === 2 ? "right" : e.button === 1 ? "middle" : "left";
  invoke("input_pointer_button", { nx: p.nx, ny: p.ny, button, down });
}
window.addEventListener("pointerdown", (e) => forwardButton(e, true));
window.addEventListener("pointerup", (e) => forwardButton(e, false));
window.addEventListener("contextmenu", (e) => { if (controlling) e.preventDefault(); });
window.addEventListener("wheel", (e) => {
  if (!controlling) return;
  e.preventDefault();
  const clamp = (v) => Math.max(-32768, Math.min(32767, Math.round(-v / 40)));
  invoke("input_pointer_wheel", { dx: clamp(e.deltaX), dy: clamp(e.deltaY) });
}, { passive: false });
function forwardKey(e, down) {
  if (!controlling) return;
  // Lock keys (Caps/Num) are synced as authoritative STATE, never forwarded as key edges (ADR-074):
  // forwarding the raw toggle would race the state sync and cancel it. Handle before the HID lookup
  // (NumLock has no HID mapping here on purpose, so it must be caught first).
  if (e.code === "CapsLock" || e.code === "NumLock") {
    e.preventDefault();
    syncLockState(e);
    return;
  }
  const hid = codeToHid(e.code);
  if (hid === undefined) return;
  e.preventDefault();
  syncLockState(e);
  invoke("input_key", { hidUsage: remapHid(hid), down, modifiers: modifierBits(e) });
}
// Push the controller's authoritative Caps/Num *state* to the host on change (ADR-074). `getModifierState`
// is read off the same event that carries the keystroke, so lock changes land in-order with the keys
// that caused them; the host slaves its OS lock keys to this. Value can lag by one event on the lock
// key's own keydown (browser-dependent), but the keyup and every subsequent key resync it.
function syncLockState(e) {
  const caps = e.getModifierState("CapsLock");
  const num = e.getModifierState("NumLock");
  if (caps === lastCaps && num === lastNum) return;
  lastCaps = caps;
  lastNum = num;
  invoke("input_set_lock_state", { capsLock: caps, numLock: num });
}
window.addEventListener("keydown", (e) => forwardKey(e, true));
window.addEventListener("keyup", (e) => forwardKey(e, false));

// ── Annotations (viewer-side markup) ───────────────────────────────────────────────────────────
// A transparent overlay the viewer draws on: pen / arrow / rectangle / highlighter. Not remote
// control — nothing is injected into the host's OS. Strokes are local to this canvas (v1). When the
// tool is "off" the overlay ignores pointer events, so the app is strictly view-only by default.
const annotations = (function () {
  const cv = document.getElementById("annot");
  const g = cv.getContext("2d");
  const bar = document.getElementById("tools");
  let tool = "off";
  let color = "#ff3b30";
  let strokes = [];
  let cur = null;
  let dpr = 1;

  function fit() {
    dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth;
    const h = cv.clientHeight;
    cv.width = Math.max(1, Math.round(w * dpr));
    cv.height = Math.max(1, Math.round(h * dpr));
    render();
  }

  function pt(e) {
    const r = cv.getBoundingClientRect();
    return { x: (e.clientX - r.left) * dpr, y: (e.clientY - r.top) * dpr };
  }

  function drawStroke(s) {
    const pts = s.pts;
    if (!pts.length) return;
    g.strokeStyle = s.color;
    g.lineJoin = "round";
    g.lineCap = "round";
    if (s.tool === "hi") {
      g.globalAlpha = 0.35;
      g.lineWidth = 18 * dpr;
    } else {
      g.globalAlpha = 1;
      g.lineWidth = 3 * dpr;
    }
    const a = pts[0];
    const b = pts[pts.length - 1];
    g.beginPath();
    if (s.tool === "rect") {
      g.strokeRect(a.x, a.y, b.x - a.x, b.y - a.y);
    } else if (s.tool === "arrow") {
      g.moveTo(a.x, a.y);
      g.lineTo(b.x, b.y);
      g.stroke();
      const ang = Math.atan2(b.y - a.y, b.x - a.x);
      const head = 16 * dpr;
      g.beginPath();
      g.moveTo(b.x, b.y);
      g.lineTo(b.x - head * Math.cos(ang - Math.PI / 6), b.y - head * Math.sin(ang - Math.PI / 6));
      g.moveTo(b.x, b.y);
      g.lineTo(b.x - head * Math.cos(ang + Math.PI / 6), b.y - head * Math.sin(ang + Math.PI / 6));
      g.stroke();
    } else {
      g.moveTo(pts[0].x, pts[0].y);
      for (let i = 1; i < pts.length; i++) g.lineTo(pts[i].x, pts[i].y);
      g.stroke();
    }
    g.globalAlpha = 1;
  }

  function render() {
    g.clearRect(0, 0, cv.width, cv.height);
    for (const s of strokes) drawStroke(s);
    if (cur) drawStroke(cur);
  }

  cv.addEventListener("pointerdown", (e) => {
    if (tool === "off") return;
    cv.setPointerCapture(e.pointerId);
    cur = { tool, color, pts: [pt(e)] };
    render();
  });
  cv.addEventListener("pointermove", (e) => {
    if (!cur) return;
    const p = pt(e);
    if (cur.tool === "pen" || cur.tool === "hi") cur.pts.push(p);
    else cur.pts[1] = p;
    render();
  });
  function endStroke() {
    if (!cur) return;
    if (cur.pts.length > 1 || cur.tool === "pen" || cur.tool === "hi") strokes.push(cur);
    cur = null;
    render();
  }
  cv.addEventListener("pointerup", endStroke);
  cv.addEventListener("pointercancel", endStroke);

  bar.querySelectorAll("button[data-tool]").forEach((btn) => {
    btn.addEventListener("click", () => {
      tool = btn.dataset.tool;
      bar.querySelectorAll("button[data-tool]").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      cv.classList.toggle("drawing", tool !== "off");
    });
  });
  bar.querySelectorAll(".swatch").forEach((sw) => {
    sw.addEventListener("click", () => {
      color = sw.dataset.color;
      bar.querySelectorAll(".swatch").forEach((b) => b.classList.remove("active"));
      sw.classList.add("active");
    });
  });
  document.getElementById("undo").addEventListener("click", () => {
    strokes.pop();
    render();
  });
  document.getElementById("clearannot").addEventListener("click", () => {
    strokes = [];
    render();
  });
  const firstSwatch = bar.querySelector(".swatch");
  if (firstSwatch) firstSwatch.classList.add("active");

  window.addEventListener("resize", fit);

  return {
    show(on) {
      bar.hidden = !on;
      if (on) fit();
    },
    clear() {
      strokes = [];
      cur = null;
      tool = "off";
      cv.classList.remove("drawing");
      bar.querySelectorAll("button[data-tool]").forEach((b) => b.classList.remove("active"));
      const off = bar.querySelector('button[data-tool="off"]');
      if (off) off.classList.add("active");
      render();
    },
  };
})();

// ── In-session chat + clipboard sync ─────────────────────────────────────────────────────────────
// A compact, collapsible corner panel usable on BOTH roles while a session is live. It talks to the
// Rust side through a fixed contract:
//   invoke("send_chat", { text })          — send a chat line on the active session
//   invoke("send_clipboard", { text })     — push local clipboard text to the peer
//   listen("chat-message", payload:String) — an incoming chat line from the remote peer
//   listen("clipboard-received", payload:Number) — peer sent us clipboard; payload is a byte count
// Chat/clipboard CONTENT is only ever placed in the DOM — never console.log'd (Invariant 8). The
// panel is gated on the session being live (`setSessionLive`), which each role drives from its own
// signals: the viewer's `setLive(...)`, the host's `share-viewer`/`share-active` events.
const chat = (function () {
  const panel = document.getElementById("chat-panel");
  const toggle = document.getElementById("chat-toggle");
  const unread = document.getElementById("chat-unread");
  const log = document.getElementById("chat-log");
  const jump = document.getElementById("chat-jump");
  const notice = document.getElementById("chat-notice");
  const noticeText = document.getElementById("chat-notice-text");
  const noticeDismiss = document.getElementById("chat-notice-dismiss");
  const form = document.getElementById("chat-form");
  const input = document.getElementById("chat-input");
  const sendBtn = document.getElementById("chat-send");
  const clipBtn = document.getElementById("chat-clipboard");
  const paste = document.getElementById("chat-paste");
  const pasteInput = document.getElementById("chat-paste-input");
  const pasteSend = document.getElementById("chat-paste-send");
  const pasteCancel = document.getElementById("chat-paste-cancel");

  // Defensive: if the markup is absent, expose a no-op so callers never throw.
  if (!panel) {
    return { setSessionLive() {} };
  }

  let live = false; // a session is active on this role
  let open = false; // panel expanded (remembered within a session)
  let unreadCount = 0; // messages arrived while collapsed
  let noticeTimer = null;
  const NO_SESSION_HINT = "Connect a session to chat";
  const SEND_HINT = "Send message";
  const CLIP_HINT = "Send your clipboard text to the other side";

  // ── Panel open/close ──────────────────────────────────────────────────────────────
  function setOpen(next) {
    open = next;
    panel.classList.toggle("chat-collapsed", !open);
    toggle.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      clearUnread();
      scrollToBottom(true);
      // Focus the input when opening via keyboard/click, but only if a session is live.
      if (live) setTimeout(() => input.focus(), 60);
    }
  }
  toggle.addEventListener("click", () => setOpen(!open));

  // ── Unread badge (only while collapsed) ───────────────────────────────────────────
  function bumpUnread() {
    unreadCount++;
    unread.textContent = String(unreadCount > 99 ? "99+" : unreadCount);
    unread.hidden = false;
  }
  function clearUnread() {
    unreadCount = 0;
    unread.hidden = true;
  }

  // ── Auto-scroll with a "new messages" pill when the user has scrolled up ───────────
  function atBottom() {
    return log.scrollHeight - log.scrollTop - log.clientHeight < 24;
  }
  function scrollToBottom(force) {
    if (force || atBottom()) {
      log.scrollTop = log.scrollHeight;
      jump.hidden = true;
    }
  }
  log.addEventListener("scroll", () => {
    if (atBottom()) jump.hidden = true;
  });
  jump.addEventListener("click", () => {
    log.scrollTop = log.scrollHeight;
    jump.hidden = true;
  });

  function timeLabel() {
    const d = new Date();
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }

  // Append a message bubble. `text` is placed via textContent — never interpolated into HTML and
  // never logged (Inv 8). `mine` sticks the view to the bottom; a received message only auto-scrolls
  // if the reader was already at the bottom, otherwise it offers the "new messages" pill.
  function appendMessage(text, mine) {
    const emptyNode = document.getElementById("chat-empty");
    if (emptyNode) emptyNode.remove();
    const stick = atBottom();
    const bubble = document.createElement("div");
    bubble.className = "chat-msg " + (mine ? "me" : "them");
    const txt = document.createElement("span");
    txt.className = "chat-text";
    txt.textContent = text;
    bubble.appendChild(txt);
    const t = document.createElement("span");
    t.className = "chat-time";
    t.textContent = timeLabel();
    bubble.appendChild(t);
    log.appendChild(bubble);

    if (mine || stick) {
      scrollToBottom(true);
    } else {
      jump.hidden = false; // reader scrolled up — offer a pill instead of yanking them down
    }
    if (!mine && !open) bumpUnread();
  }

  // ── Notice (clipboard sent / received), auto-dismiss after ~3s ─────────────────────
  function showNotice(text, kind) {
    noticeText.textContent = text;
    notice.classList.remove("fading");
    notice.classList.toggle("ok", kind === "ok");
    notice.hidden = false;
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => {
      notice.classList.add("fading");
      noticeTimer = setTimeout(() => {
        notice.hidden = true;
        notice.classList.remove("fading");
      }, 320);
    }, 3000);
  }
  function hideNotice() {
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = null;
    notice.hidden = true;
    notice.classList.remove("fading");
  }
  noticeDismiss.addEventListener("click", hideNotice);

  // ── Send chat ──────────────────────────────────────────────────────────────────────
  async function sendChat() {
    if (!live) return;
    const text = input.value.trim();
    if (!text) return;
    input.value = "";
    autosize();
    input.focus(); // keep focus after send
    appendMessage(text, true);
    try {
      await invoke("send_chat", { text });
    } catch (_) {
      // No active session (or a transient send error) — surface gracefully in the log, not console.
      showNotice("Couldn't send — no active session.", "");
    }
  }
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    sendChat();
  });
  // Enter sends, Shift+Enter inserts a newline. Trimmed empties are ignored by sendChat().
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      sendChat();
    }
  });
  // Auto-grow the single-row textarea up to its max-height.
  function autosize() {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 96) + "px";
  }
  input.addEventListener("input", autosize);

  // ── Clipboard send ───────────────────────────────────────────────────────────────
  async function pushClipboard(text) {
    if (!live || !text) return;
    try {
      await invoke("send_clipboard", { text });
      showNotice("Clipboard sent · " + text.length + " chars", "ok");
    } catch (_) {
      showNotice("Couldn't send clipboard — no active session.", "");
    }
  }
  clipBtn.addEventListener("click", async () => {
    if (!live) return;
    try {
      const text = await navigator.clipboard.readText();
      if (!text) {
        showNotice("Clipboard is empty.", "");
        return;
      }
      pushClipboard(text);
    } catch (_) {
      // Permission denied / not available in this webview — offer the inline paste fallback.
      paste.hidden = false;
      pasteInput.value = "";
      pasteInput.focus();
    }
  });
  pasteSend.addEventListener("click", () => {
    const text = pasteInput.value;
    paste.hidden = true;
    if (text.trim()) pushClipboard(text);
  });
  pasteCancel.addEventListener("click", () => {
    paste.hidden = true;
    pasteInput.value = "";
  });

  // ── Enable/disable the controls for the current session state ──────────────────────
  function setEnabled(on) {
    input.disabled = !on;
    sendBtn.disabled = !on;
    clipBtn.disabled = !on;
    input.title = on ? "" : NO_SESSION_HINT;
    sendBtn.title = on ? SEND_HINT : NO_SESSION_HINT;
    clipBtn.title = on ? CLIP_HINT : NO_SESSION_HINT;
  }

  // ── Session lifecycle ──────────────────────────────────────────────────────────────
  // Called by each role when its session goes live / ends. Ending clears the log (Inv 8 hygiene —
  // no stale chat lingering after a session) and resets the panel to collapsed.
  function setSessionLive(isLive) {
    isLive = !!isLive;
    if (isLive === live) {
      setEnabled(isLive);
      return;
    }
    live = isLive;
    panel.hidden = !isLive;
    setEnabled(isLive);
    if (isLive) {
      setOpen(false); // start collapsed & unobtrusive; the user expands when they want to chat
    } else {
      // Session ended: wipe content and transient UI (no stale chat lingering — Inv 8 hygiene).
      resetLog();
      input.value = "";
      autosize();
      clearUnread();
      hideNotice();
      paste.hidden = true;
      pasteInput.value = "";
      jump.hidden = true;
      open = false;
      panel.classList.add("chat-collapsed");
      toggle.setAttribute("aria-expanded", "false");
    }
  }

  // Clear the message log back to its empty state.
  function resetLog() {
    log.textContent = "";
    const p = document.createElement("p");
    p.id = "chat-empty";
    p.className = "chat-empty";
    p.textContent = "No messages yet — say hi 👋";
    log.appendChild(p);
  }

  // ── Incoming events from Rust ──────────────────────────────────────────────────────
  listen("chat-message", (e) => {
    const text = typeof e.payload === "string" ? e.payload : String(e.payload ?? "");
    if (!text) return;
    appendMessage(text, false);
  });
  listen("clipboard-received", (e) => {
    const n = Number(e.payload) || 0;
    showNotice("Received clipboard · " + n + " bytes", "ok");
  });

  return { setSessionLive };
})();

// ── File transfer ────────────────────────────────────────────────────────────────────────────────
// Two halves over a fixed Rust contract (a file's *contents* are never console.log'd — Inv 8):
//   Sender (Connect/viewer side):
//     invoke("file_begin",  { filename, size })     — offer a file; awaits a file-accepted/rejected
//     invoke("file_chunk",  { bytes: [...u8] })      — one sequential chunk (256 KiB) as a byte array
//     invoke("file_end")                             — finalize after the last chunk
//     listen("file-accepted")  / listen("file-rejected")  — the peer's decision on our offer
//   Receiver (Share/host side):
//     listen("file-offer", { filename, size })       — an incoming offer to accept/deny
//     invoke("respond_file_offer", { accept })        — the local user's decision (Inv 1)
//     listen("file-received", { filename, size })     — a completed transfer landed
// The "Send file" affordance is live-gated exactly like chat (setSessionLive), and only surfaces on
// the viewer role — the host receives, it does not push.
const files = (function () {
  const CHUNK = 256 * 1024; // 256 KiB per file_chunk invoke
  const OFFER_TIMEOUT_MS = 60000; // auto-deny an unanswered incoming offer
  const ACCEPT_TIMEOUT_MS = 60000; // give up if the peer never accepts/rejects our offer

  // Sender controls (Connect bar + progress card).
  const sendBtn = document.getElementById("send-file");
  const picker = document.getElementById("file-picker");
  const card = document.getElementById("filesend");
  const nameEl = document.getElementById("filesend-name");
  const stateEl = document.getElementById("filesend-state");
  const pctEl = document.getElementById("filesend-pct");
  const trackEl = document.getElementById("filesend-track");
  const fillEl = document.getElementById("filesend-fill");
  const cancelBtn = document.getElementById("filesend-cancel");

  // Receiver controls (offer modal + received notice).
  const offer = document.getElementById("file-offer");
  const offerName = document.getElementById("file-offer-name");
  const offerSize = document.getElementById("file-offer-size");
  const offerTimeout = document.getElementById("file-offer-timeout");
  const offerAccept = document.getElementById("file-offer-accept");
  const offerDeny = document.getElementById("file-offer-deny");
  const recvNotice = document.getElementById("file-recv-notice");
  const recvText = document.getElementById("file-recv-text");
  const recvDismiss = document.getElementById("file-recv-dismiss");

  // Defensive: if the markup is absent, expose no-ops so callers never throw.
  if (!sendBtn || !card || !offer) {
    return { setViewerLive() {}, setHostLive() {} };
  }

  const NO_SESSION_HINT = "Connect a session to send a file";
  const SEND_HINT = "Send a file to the other side";

  let viewerLive = false; // a viewer (Connect) session is up → sending is possible
  let sending = false; // a transfer is in flight (guards against concurrent sends)
  let cancelled = false; // the user hit cancel mid-transfer
  // Promise resolvers for the peer's accept/reject of the current offer.
  let acceptResolve = null;
  let acceptReject = null;
  let acceptTimer = null;
  // Receiver: countdown + auto-deny timers for an open offer.
  let offerTimer = null;
  let offerCountdown = null;
  let recvTimer = null;

  // Human-readable byte size (1 KB = 1024 B), e.g. "3.4 MB". Never rounds a nonzero size to "0".
  function fmtSize(bytes) {
    const b = Number(bytes) || 0;
    if (b < 1024) return b + " B";
    const units = ["KB", "MB", "GB", "TB"];
    let v = b / 1024;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return (v >= 10 || Number.isInteger(v) ? v.toFixed(0) : v.toFixed(1)) + " " + units[i];
  }

  // ── Sender: progress card presentation ─────────────────────────────────────────────
  function setCardState(cls) {
    card.classList.remove("state-wait", "state-ok", "state-err");
    if (cls) card.classList.add(cls);
  }
  function showCard(filename) {
    nameEl.textContent = filename;
    nameEl.title = filename;
    cancelBtn.disabled = false;
    card.hidden = false;
  }
  function setProgress(sent, total) {
    const pct = total > 0 ? Math.min(100, Math.round((sent / total) * 100)) : 0;
    fillEl.style.width = pct + "%";
    trackEl.setAttribute("aria-valuenow", String(pct));
    pctEl.textContent = pct + "%";
    stateEl.textContent = fmtSize(sent) + " / " + fmtSize(total);
    stateEl.className = "filesend-state";
  }
  function setState(text, kind) {
    stateEl.textContent = text;
    stateEl.className = "filesend-state" + (kind ? " " + kind : "");
  }
  function hideCardLater(delay) {
    setTimeout(() => {
      // Only auto-hide if nothing new started in the meantime.
      if (!sending) card.hidden = true;
    }, delay);
  }

  // ── Sender: the accept/reject wait ─────────────────────────────────────────────────
  function waitForAccept() {
    return new Promise((resolve, reject) => {
      acceptResolve = resolve;
      acceptReject = reject;
      acceptTimer = setTimeout(() => {
        settleAccept(false, "timeout");
      }, ACCEPT_TIMEOUT_MS);
    });
  }
  function settleAccept(accepted, reason) {
    if (acceptTimer) {
      clearTimeout(acceptTimer);
      acceptTimer = null;
    }
    const res = acceptResolve;
    const rej = acceptReject;
    acceptResolve = null;
    acceptReject = null;
    if (accepted && res) res();
    else if (!accepted && rej) rej(reason || "declined");
  }

  listen("file-accepted", () => settleAccept(true));
  listen("file-rejected", () => settleAccept(false, "declined"));

  // ── Sender: the transfer itself ────────────────────────────────────────────────────
  async function sendFile(file) {
    if (sending || !viewerLive) return;
    sending = true;
    cancelled = false;
    setSendBusy(true);
    showCard(file.name);
    setCardState("state-wait");
    setState("Waiting for the other side to accept…", "");
    pctEl.textContent = "";

    try {
      await invoke("file_begin", { filename: file.name, size: file.size });
    } catch (e) {
      failTransfer("Couldn't start the transfer.");
      return;
    }

    // Wait for the peer's decision before reading any bytes.
    try {
      await waitForAccept();
    } catch (reason) {
      if (reason === "timeout") failTransfer("No response — transfer timed out.");
      else declineTransfer();
      return;
    }
    if (cancelled) return finishCancel();

    // Accepted → stream the file sequentially in CHUNK-sized slices.
    setCardState("");
    setProgress(0, file.size);
    let offsetBytes = 0;
    try {
      while (offsetBytes < file.size) {
        if (cancelled) return finishCancel();
        const end = Math.min(offsetBytes + CHUNK, file.size);
        const buf = await file.slice(offsetBytes, end).arrayBuffer();
        if (cancelled) return finishCancel();
        // The Rust side wants a plain byte array (JS number[] / Uint8Array).
        await invoke("file_chunk", { bytes: Array.from(new Uint8Array(buf)) });
        offsetBytes = end;
        setProgress(offsetBytes, file.size);
      }
      await invoke("file_end");
    } catch (e) {
      // Never surface file contents; a generic, honest message only (Inv 8).
      failTransfer("Transfer failed — the connection may have dropped.");
      return;
    }

    // Success.
    setCardState("state-ok");
    setState("Sent ✓", "ok");
    pctEl.textContent = "100%";
    cancelBtn.disabled = true;
    sending = false;
    setSendBusy(false);
    hideCardLater(2600);
  }

  function failTransfer(msg) {
    // Best-effort tell the host we're aborting, then present the error.
    try { invoke("file_end"); } catch (_) {}
    setCardState("state-err");
    setState(msg, "err");
    pctEl.textContent = "";
    cancelBtn.disabled = true;
    sending = false;
    setSendBusy(false);
    hideCardLater(4000);
  }
  function declineTransfer() {
    setCardState("state-err");
    setState("The other side declined.", "err");
    pctEl.textContent = "";
    cancelBtn.disabled = true;
    sending = false;
    setSendBusy(false);
    hideCardLater(3200);
  }
  function finishCancel() {
    try { invoke("file_end"); } catch (_) {}
    setCardState("state-err");
    setState("Canceled.", "err");
    pctEl.textContent = "";
    cancelBtn.disabled = true;
    sending = false;
    setSendBusy(false);
    hideCardLater(2200);
  }

  function setSendBusy(busy) {
    sendBtn.classList.toggle("busy", busy);
    sendBtn.disabled = busy || !viewerLive;
    sendBtn.title = viewerLive ? SEND_HINT : NO_SESSION_HINT;
  }

  // ── Sender: wiring ─────────────────────────────────────────────────────────────────
  sendBtn.addEventListener("click", () => {
    if (!viewerLive || sending) return;
    picker.value = ""; // allow re-picking the same file
    picker.click();
  });
  picker.addEventListener("change", () => {
    const file = picker.files && picker.files[0];
    if (file) sendFile(file);
  });
  cancelBtn.addEventListener("click", () => {
    if (!sending) {
      card.hidden = true; // dismiss a finished/errored card
      return;
    }
    cancelled = true;
    // If we're still waiting on the peer's accept, unblock that wait too.
    settleAccept(false, "cancelled");
  });

  // ── Receiver: incoming offer ───────────────────────────────────────────────────────
  function clearOfferTimers() {
    if (offerTimer) { clearTimeout(offerTimer); offerTimer = null; }
    if (offerCountdown) { clearInterval(offerCountdown); offerCountdown = null; }
  }
  function respondOffer(accept) {
    clearOfferTimers();
    offer.hidden = true;
    try { invoke("respond_file_offer", { accept }); } catch (_) {}
  }
  function openOffer(filename, size) {
    clearOfferTimers();
    offerName.textContent = filename;
    offerName.title = filename;
    offerSize.textContent = "· " + fmtSize(size);
    offer.hidden = false;
    // Focus the safe default (Deny) so a stray Enter doesn't auto-accept.
    setTimeout(() => offerDeny.focus(), 60);
    // Countdown → auto-deny if the local user doesn't answer.
    let left = Math.round(OFFER_TIMEOUT_MS / 1000);
    offerTimeout.textContent = "Auto-declines in " + left + "s if no response.";
    offerCountdown = setInterval(() => {
      left -= 1;
      offerTimeout.textContent =
        left > 0 ? "Auto-declines in " + left + "s if no response." : "Declining…";
    }, 1000);
    offerTimer = setTimeout(() => respondOffer(false), OFFER_TIMEOUT_MS);
  }
  offerAccept.addEventListener("click", () => respondOffer(true));
  offerDeny.addEventListener("click", () => respondOffer(false));
  // Esc denies (the safe default) while the modal is open.
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !offer.hidden) respondOffer(false);
  });

  listen("file-offer", (e) => {
    const p = e.payload || {};
    const filename = typeof p.filename === "string" ? p.filename : "file";
    const size = Number(p.size) || 0;
    openOffer(filename, size);
  });

  // ── Receiver: completed transfer notice ────────────────────────────────────────────
  function showRecvNotice(filename, size) {
    recvText.textContent = "Received " + filename + " · " + fmtSize(size);
    recvText.title = filename;
    recvNotice.classList.remove("fading");
    recvNotice.hidden = false;
    if (recvTimer) clearTimeout(recvTimer);
    recvTimer = setTimeout(() => {
      recvNotice.classList.add("fading");
      recvTimer = setTimeout(() => {
        recvNotice.hidden = true;
        recvNotice.classList.remove("fading");
      }, 320);
    }, 4000);
  }
  function hideRecvNotice() {
    if (recvTimer) { clearTimeout(recvTimer); recvTimer = null; }
    recvNotice.hidden = true;
    recvNotice.classList.remove("fading");
  }
  recvDismiss.addEventListener("click", hideRecvNotice);
  listen("file-received", (e) => {
    const p = e.payload || {};
    const filename = typeof p.filename === "string" ? p.filename : "file";
    const size = Number(p.size) || 0;
    showRecvNotice(filename, size);
  });

  // ── Session lifecycle ──────────────────────────────────────────────────────────────
  // The viewer role can *send*; the host role can *receive*. Ending a session tears down any
  // in-flight transfer/prompt for that role so nothing lingers.
  function setViewerLive(isLive) {
    viewerLive = !!isLive;
    setSendBusy(sending);
    if (!viewerLive) {
      // A dropped viewer session cancels an in-flight send and clears the card.
      if (sending) {
        cancelled = true;
        settleAccept(false, "cancelled");
      }
      sending = false;
      setSendBusy(false);
      card.hidden = true;
    }
  }
  function setHostLive(isLive) {
    if (!isLive) {
      // Host session ended — dismiss any open offer / notice.
      clearOfferTimers();
      offer.hidden = true;
      hideRecvNotice();
    }
  }

  return { setViewerLive, setHostLive };
})();
