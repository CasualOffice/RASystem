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
const banner = document.getElementById("banner");

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
  connectBtn.disabled = isLive;
  ticketInput.disabled = isLive;
  stopBtn.disabled = !isLive;
  annotations.show(isLive);
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
});
listen("share-active", (e) => {
  if (!e.payload) shareIndicator.className = "indicator idle";
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
