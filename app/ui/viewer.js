/* viewer.js — the WebCodecs remote-screen viewer, extracted faithfully from the proven main.js path
 * (H.264/VP9/VP8 decode → canvas, RAS1 framing, baseline-profile coercion, codec negotiation, the
 * reconfigure-retry cap, the latency drop-guard). Self-contained: takes its dependencies (invoke,
 * Channel, a canvas, status/fatal callbacks) so both the legacy app and the new shell can use it.
 *
 * ON-DEVICE VERIFICATION PENDING: this logic is a faithful port of code that works in the running app,
 * but WebCodecs + a live two-machine iroh session can only be verified on real hardware. Inv 8: no
 * stream content is ever logged; status strings are capability/engine metadata only.
 */
(function (global) {
  "use strict";

  const HEADER_LEN = 24;
  const FRAME_MAGIC = 0x52415331; // "RAS1" — a frame blob
  const CONFIG_MAGIC = 0x52434647; // "RCFG" — the one-shot stream-config blob
  const FLAG_KEYFRAME = 0x01;
  const MAX_DEC_RETRIES = 3;
  const DECODE_QUEUE_MAX = 3;

  function toBytes(msg) {
    if (msg instanceof ArrayBuffer) return new Uint8Array(msg);
    if (ArrayBuffer.isView(msg)) return new Uint8Array(msg.buffer, msg.byteOffset, msg.byteLength);
    if (Array.isArray(msg)) return Uint8Array.from(msg);
    throw new Error("unexpected channel payload type");
  }

  // H.264: the Rust side advertises a MAIN-profile string but the encoders emit Constrained Baseline;
  // coerce to Baseline (0x42E0) keeping the level byte so Chromium engines accept it. VP9/VP8 pass through.
  function baselineCodec(codec) {
    const m = /^avc1\.[0-9A-Fa-f]{6}$/.exec(codec || "");
    if (!m) return codec;
    return "avc1.42E0" + codec.slice(-2);
  }
  function decoderConfig(cfg) {
    const isH264 = /^avc1\./.test(cfg.codec || "");
    return {
      codec: isH264 ? baselineCodec(cfg.codec) : cfg.codec,
      codedWidth: cfg.width,
      codedHeight: cfg.height,
      optimizeForLatency: true,
    };
  }

  // Probe this webview for decodable codecs, most-preferred first (0=H.264, 1=VP9, 2=VP8). Rides the
  // signed AccessRequest; an empty result ⇒ host fails safe to VP9 (never a black screen).
  async function getViewerCodecPreferences() {
    const prefs = [];
    if (typeof VideoDecoder === "undefined" || typeof VideoDecoder.isConfigSupported !== "function") return prefs;
    const probe = async (codec, tag) => {
      try {
        const { supported } = await VideoDecoder.isConfigSupported({ codec, codedWidth: 1280, codedHeight: 720, optimizeForLatency: true });
        if (supported) prefs.push(tag);
      } catch (_) {}
    };
    await probe("avc1.42E01E", 0);
    await probe("vp09.00.31.08", 1);
    await probe("vp8", 2);
    return prefs;
  }

  const FATAL_VIDEO_MSG =
    "This system's browser engine can't decode the video stream (the negotiated codec isn't supported " +
    "by this webview's WebCodecs implementation).";

  /** Create a viewer bound to a canvas. Deps: { invoke, Channel } from window.__TAURI__.core.
   *  Callbacks: onStatus(str) — a content-free HUD line; onFatal(str) — decode unavailable; onLive(bool). */
  function createViewer(opts) {
    const canvas = opts.canvas;
    const cctx = canvas.getContext("2d");
    const invoke = opts.invoke, Channel = opts.Channel;
    const onStatus = opts.onStatus || function () {};
    const onFatal = opts.onFatal || function () {};
    const onLive = opts.onLive || function () {};
    const onAudioMsg = opts.onAudio || function () {};

    let decoder = null, sawKeyframe = false, decoded = 0, received = 0, gaps = 0, lastId = null,
        decErrors = 0, t0 = 0, lastLatencyKeyReq = 0, live = false, kickTimer = null;

    function reset() {
      try { decoder && decoder.close(); } catch (_) {}
      decoder = null; sawKeyframe = false; decoded = 0; received = 0; gaps = 0; lastId = null;
      decErrors = 0; t0 = performance.now(); lastLatencyKeyReq = 0;
      if (kickTimer) { clearInterval(kickTimer); kickTimer = null; }
    }

    function buildDecoder(cfg) {
      canvas.width = cfg.width; canvas.height = cfg.height;
      const dec = new VideoDecoder({
        output: (frame) => { cctx.drawImage(frame, 0, 0, canvas.width, canvas.height); frame.close(); decoded++; decErrors = 0; },
        error: (e) => {
          decErrors++;
          if (decErrors > MAX_DEC_RETRIES) {
            try { dec.close(); } catch (_) {}
            if (decoder === dec) decoder = null;
            onStatus("video decode unavailable on this engine"); onFatal(FATAL_VIDEO_MSG); return;
          }
          onStatus("decoder error → resetting (" + decErrors + "/" + MAX_DEC_RETRIES + "): " + e.message);
          sawKeyframe = false;
          try { dec.reset(); } catch (_) {}
          try { dec.configure(decoderConfig(cfg)); } catch (_) {}
          invoke("request_keyframe");
        },
      });
      dec.configure(decoderConfig(cfg));
      return dec;
    }

    async function onConfig(bytes) {
      const cfg = JSON.parse(new TextDecoder().decode(bytes.subarray(4)));
      const config = decoderConfig(cfg);
      try {
        if (typeof VideoDecoder.isConfigSupported === "function") {
          const { supported } = await VideoDecoder.isConfigSupported(config);
          if (!supported) { onStatus("video decode unavailable on this engine"); onFatal(FATAL_VIDEO_MSG); return; }
        }
      } catch (_) {}
      decErrors = 0;
      decoder = buildDecoder(cfg);
      onStatus("viewing " + cfg.width + "×" + cfg.height + " @ " + cfg.fps + " · " + config.codec);
      invoke("request_keyframe");
      kickTimer = setInterval(() => { if (decoded > 0) { clearInterval(kickTimer); kickTimer = null; } else invoke("request_keyframe"); }, 500);
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
      const id = Number(frameId);
      if (lastId !== null && id > lastId + 1) gaps += id - lastId - 1;
      lastId = id;
      if (!decoder || decoder.state !== "configured") return;
      if (!sawKeyframe) { if (!isKey) return; sawKeyframe = true; }
      if (decoder.decodeQueueSize > DECODE_QUEUE_MAX && !isKey) {
        const now = performance.now();
        if (now - lastLatencyKeyReq > 400) { lastLatencyKeyReq = now; invoke("request_keyframe").catch(() => {}); }
        return;
      }
      try {
        decoder.decode(new EncodedVideoChunk({ type: isKey ? "key" : "delta", timestamp: Number(tsUs), data: payload }));
      } catch (e) { onStatus("decode() threw: " + e.message); }
      if (received % 30 === 0) {
        const dt = (performance.now() - t0) / 1000;
        onStatus("render " + (decoded / dt).toFixed(1) + " fps · rx " + received + " · q " + decoder.decodeQueueSize);
      }
    }

    function onMessage(msg) {
      const bytes = toBytes(msg);
      if (bytes.byteLength < 4) return;
      const magic = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, true);
      if (magic === CONFIG_MAGIC) return void onConfig(bytes).catch((e) => onStatus("bad stream config: " + (e && e.message ? e.message : e)));
      if (magic === FRAME_MAGIC) return onFrame(bytes);
    }

    async function connect(source) {
      if (!("VideoDecoder" in window)) { onStatus("WebCodecs VideoDecoder unavailable in this webview."); onFatal(FATAL_VIDEO_MSG); return false; }
      reset();
      const channel = new Channel();
      channel.onmessage = onMessage;
      const onAudio = new Channel(); // host→controller Opus packets → the shell's audio player
      onAudio.onmessage = onAudioMsg;
      onStatus(source.contactId ? "reaching your contact…" : "connecting…");
      const viewerCodecs = await getViewerCodecPreferences();
      try {
        if (source.contactId) await invoke("connect_to_contact", { id: source.contactId, onFrame: channel, onAudio, viewerCodecs });
        else await invoke("connect_to_host", { ticket: source.ticket, onFrame: channel, onAudio, viewerCodecs });
      } catch (e) { onStatus("connect failed: " + e); onLive(false); return false; }
      live = true; onLive(true); onStatus("session up — waiting for stream config…");
      return true;
    }

    return {
      connectContact: (id) => connect({ contactId: id }),
      connectTicket: (ticket) => connect({ ticket }),
      requestKeyframe: () => { try { invoke("request_keyframe"); } catch (_) {} },
      // Feed one RCFG/RAS1 blob (already-received frames pushed from Rust, e.g. call video) into the
      // decoder → canvas, without a connect. Reuses the full decode path (config gate, keyframe gate,
      // reconfigure-retry cap).
      feed: (msg) => onMessage(msg),
      // Reset the decoder + canvas (call ended).
      resetSink: () => reset(),
      isLive: () => live,
      async disconnect() {
        try { await invoke("disconnect"); } catch (_) {}
        reset(); live = false; onLive(false);
      },
    };
  }

  global.RASViewer = { createViewer };
})(window);
