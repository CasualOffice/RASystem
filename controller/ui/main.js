// Casual RAS controller — WebCodecs video path (ADR-022, S3).
//
// Receives encoded H.264 access units on a binary Tauri Channel, decodes each with a WebCodecs
// VideoDecoder, and renders the VideoFrame to a canvas. No pixels ever cross JSON IPC; the only
// JSON is the one-shot stream descriptor returned by `start_mirror`.
//
// Frame blob = the canonical ras_core::frame_channel header + Annex-B payload (little-endian):
//   magic:u32("RAS1") | flags:u8(bit0=key) | pad:u8 | pad:u16 | frame_id:u64 | captured_at_us:u64
// (24 bytes). Ids/timestamps are read as BigInt — a JS number corrupts u64 past 2^53.

const { invoke, Channel } = window.__TAURI__.core;

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
  // A Raw channel body arrives as an ArrayBuffer (or a typed view, depending on size threshold).
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
      // Terminal decode error: reset and ask the host for a fresh IDR (KeyframeReason::DecoderReset).
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
  hud.textContent = `mirroring ${cfg.width}×${cfg.height} @ ${cfg.fps} · ${cfg.codec}`;
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

async function main() {
  if (!("VideoDecoder" in window)) {
    hud.textContent = "WebCodecs VideoDecoder unavailable in this webview.";
    return;
  }
  const channel = new Channel();
  channel.onmessage = onMessage;
  window.addEventListener("beforeunload", () => invoke("stop_mirror"));

  try {
    // The negotiated stream config + frames arrive on the channel (RCFG then RAS1 blobs).
    await invoke("start_mirror", { onFrame: channel });
  } catch (e) {
    hud.textContent = "start_mirror failed: " + e;
    return;
  }
  hud.textContent = "session up — waiting for stream config…";
}

main();
