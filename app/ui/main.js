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
  contacts: document.getElementById("contacts-view"),
};

function showView(name) {
  for (const [k, el] of Object.entries(views)) el.hidden = k !== name;
}

document.getElementById("go-connect").addEventListener("click", () => showView("connect"));
document.getElementById("go-share").addEventListener("click", () => {
  showView("share");
  startSharing();
});
document.getElementById("go-contacts").addEventListener("click", () => {
  showView("contacts");
  loadContacts();
  loadMyIdentity();
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

// "Copy diagnostics": app version + a content-free recent-events log tail, to the OS clipboard — so
// when something breaks on-device the user can paste a useful trail into an issue report (Inv 8: the
// log holds no secrets/pixels/keystrokes).
(function () {
  const btn = document.getElementById("copy-diagnostics");
  const status = document.getElementById("update-status");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    try {
      const diag = await invoke("read_diagnostics");
      await navigator.clipboard.writeText(diag);
      if (status) status.textContent = "Diagnostics copied to clipboard.";
    } catch (e) {
      if (status) status.textContent = "Couldn't copy diagnostics.";
      console.warn("diagnostics:", e);
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
const fatalEl = document.getElementById("fatal-error");
const fatalTextEl = document.getElementById("fatal-error-text");

let decoder = null;
let sawKeyframe = false;
let decoded = 0;
let received = 0;
let lastId = null;
let gaps = 0;
let t0 = performance.now();
// Consecutive decoder errors since the last successfully-decoded frame. Caps the reconfigure retry
// loop so an unsupported-but-valid codec can't silently re-arm forever (permanent black canvas).
let decErrors = 0;
const MAX_DEC_RETRIES = 3;

// Persistent, honest capability message (Inv 8: engine capability only, never stream content). Shown
// when the video (or audio) can't be decoded on this webview engine; cleared when a fresh session
// starts. Does not touch the always-visible LIVE indicator (Inv 7).
function fatalError(msg) {
  if (!fatalEl) return;
  fatalTextEl.textContent = msg;
  fatalEl.hidden = false;
  // Record it in the app log file so an on-device black-screen (e.g. WebKitGTK can't decode H.264)
  // leaves a trail. The message is a capability/engine string — content-free (Inv 8).
  try {
    window.__TAURI__?.log?.error("viewer fatal: " + msg);
  } catch (_) {}
}
function clearFatalError() {
  if (!fatalEl) return;
  fatalEl.hidden = true;
  fatalTextEl.textContent = "";
}

function toBytes(msg) {
  if (msg instanceof ArrayBuffer) return new Uint8Array(msg);
  if (ArrayBuffer.isView(msg)) return new Uint8Array(msg.buffer, msg.byteOffset, msg.byteLength);
  if (Array.isArray(msg)) return Uint8Array.from(msg);
  throw new Error("unexpected channel payload type");
}

const FATAL_VIDEO_MSG =
  "This system's browser engine can't decode the video (H.264/WebCodecs is unavailable in " +
  "WebKitGTK on Linux). The macOS and Windows viewers work; Linux viewing is not yet supported.";

function buildDecoder(cfg) {
  canvas.width = cfg.width;
  canvas.height = cfg.height;
  const dec = new VideoDecoder({
    output: (frame) => {
      // Draw then release promptly (tiny pool; latency over buffering — priority #2).
      ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
      frame.close();
      decoded++;
      decErrors = 0; // a real frame decoded → the codec works; reset the retry cap
    },
    error: (e) => {
      // Cap the reconfigure retries: an unsupported-but-valid codec fires this async error on every
      // configure(), so re-arming unconditionally is an infinite silent loop → permanent black canvas.
      // After MAX_DEC_RETRIES consecutive failures with no decoded frame, stop and tell the user
      // honestly (Inv 8: no stream content — engine capability only).
      decErrors++;
      if (decErrors > MAX_DEC_RETRIES) {
        try { dec.close(); } catch (_) {}
        if (decoder === dec) decoder = null;
        hud.textContent = "video decode unavailable on this engine";
        fatalError(FATAL_VIDEO_MSG);
        return;
      }
      hud.textContent = `decoder error → resetting (${decErrors}/${MAX_DEC_RETRIES}): ` + e.message;
      sawKeyframe = false;
      try { dec.reset(); } catch (_) {}
      try { dec.configure(decoderConfig(cfg)); } catch (_) {}
      invoke("request_keyframe");
    },
  });
  dec.configure(decoderConfig(cfg));
  return dec;
}

// Codec string for the WebCodecs config. IMPORTANT: the Rust side (ras-media
// VideoCodec::webcodecs_string, crates/ras-media/src/lib.rs) sends a MAIN-profile string
// ("avc1.4D40LL", profile_idc 0x4D) in the RCFG blob, but the actual encoders (VideoToolbox /
// OpenH264) emit a BASELINE Annex-B stream (profile_idc 0x42). Chromium-family engines can reject a
// Main-profile config for a Baseline stream, so here — JS-side only, per the edit scope — we override
// the profile+constraint bytes to Baseline (0x42E0) while preserving the level byte the Rust side
// chose. FLAG FOR HUMAN: the correct long-term fix is to make Rust emit the Baseline string (or derive
// it from the SPS); this JS override is the minimal safe patch that keeps the two in agreement.
function baselineCodec(codec) {
  // Expect "avc1.PPCCLL" (8 hex chars after the dot). Keep the level (last 2), force Baseline+constraints.
  const m = /^avc1\.[0-9A-Fa-f]{6}$/.exec(codec || "");
  if (!m) return codec; // unknown shape — pass through unchanged
  const level = codec.slice(-2);
  return "avc1.42E0" + level;
}

function decoderConfig(cfg) {
  // No `description` ⇒ Annex-B input (our encoder re-sends SPS/PPS in-band on every IDR — the
  // Chromium annexb path). The codec string is coerced to Baseline to match the emitted stream.
  return {
    codec: baselineCodec(cfg.codec),
    codedWidth: cfg.width,
    codedHeight: cfg.height,
    optimizeForLatency: true,
  };
}

async function onConfig(bytes) {
  const json = new TextDecoder().decode(bytes.subarray(4));
  const cfg = JSON.parse(json);
  const config = decoderConfig(cfg);

  // Real support gate (WebCodecs spec): configure() with an unsupported-but-valid codec does NOT throw
  // synchronously — it fires the async error callback later. Prechecking here surfaces an honest,
  // persistent message instead of a silent reconfigure loop. Feature-detect isConfigSupported itself,
  // since some engines expose VideoDecoder without the static method.
  try {
    if (typeof VideoDecoder.isConfigSupported === "function") {
      const { supported } = await VideoDecoder.isConfigSupported(config);
      if (!supported) {
        hud.textContent = "video decode unavailable on this engine";
        fatalError(FATAL_VIDEO_MSG);
        return;
      }
    }
  } catch (_) {
    // isConfigSupported threw (unusual) — fall through to configure(), which the retry cap still bounds.
  }

  decErrors = 0;
  decoder = buildDecoder(cfg);
  hud.textContent = `viewing ${cfg.width}×${cfg.height} @ ${cfg.fps} · ${config.codec}`;
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
  // onConfig is async (it awaits the codec-support gate). Catch here so a bad config blob can never
  // become an unhandled promise rejection.
  if (magic === CONFIG_MAGIC) return void onConfig(bytes).catch((e) => {
    hud.textContent = "bad stream config: " + (e && e.message ? e.message : e);
  });
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

// ── Output-audio playback (Connect role) ─────────────────────────────────────────────────────────
// The host may transmit its system audio as Opus, one packet per Tauri Channel message. Each blob is:
//   magic ASCII "RAU1" (4) | sampleRate:u32 LE (4) | channels:u8 (1) | seq:u64 LE (8) | Opus bytes …
// (18-byte header, then the raw Opus packet). We decode with a WebCodecs AudioDecoder configured on the
// first packet, then play each decoded AudioData through one AudioContext, scheduling buffers back-to-
// back on a running clock. If we fall behind (packet loss / a stall), we re-snap the cursor to `now` so
// latency never grows unbounded — priority #2 (latency) over gapless fidelity. Audio CONTENT (the PCM /
// Opus bytes) is never logged (Inv 8). Playback defaults ON; a mute toggle + a "click to enable" gesture
// affordance handle autoplay policy.
const AUDIO_MAGIC = 0x52415531; // "RAU1" big-endian
const AUDIO_HEADER_LEN = 17; // 4 magic + 4 sampleRate + 1 channels + 8 seq

const audioPlayer = (function () {
  const btn = document.getElementById("audio-btn");

  let ctx = null; // one AudioContext for the session
  let decoder = null; // WebCodecs AudioDecoder
  let cfg = null; // { sampleRate, channels } from the first packet
  let nextStartTime = 0; // playback clock cursor (AudioContext time)
  let muted = false; // user chose to silence (decode still runs, output gated by gain)
  let gain = null; // master gain node — the mute point (never touches decoder state)
  let started = false; // we've received at least one packet
  let needsGesture = false; // AudioContext is suspended and needs a user gesture to resume
  let firstSeq = null; // seq of the first packet, so timestamps start near zero
  // Codec support: null = not yet probed, true = usable, false = this engine has no Opus AudioDecoder.
  // When false we never build the decoder or show the audio button (video is unaffected). A cap on
  // consecutive decoder errors stops an infinite reset/retry loop (mirrors the video path).
  let opusSupported = null;
  let audioErrors = 0;
  const MAX_AUDIO_RETRIES = 3;
  let noticedUnsupported = false; // HUD the honest "no audio on this engine" note only once

  // ── Button presentation ─────────────────────────────────────────────────────────────
  function refreshBtn() {
    if (!btn) return;
    btn.hidden = !started;
    btn.classList.toggle("muted", muted);
    btn.classList.toggle("playing", !muted && !needsGesture);
    btn.classList.toggle("needs-gesture", needsGesture);
    btn.setAttribute("aria-pressed", muted ? "false" : "true");
    const label = btn.querySelector(".audio-label");
    if (needsGesture) {
      if (label) label.textContent = "Enable audio";
      btn.title = "Click to enable audio from the shared machine";
      btn.setAttribute("aria-label", "Click to enable audio");
    } else if (muted) {
      if (label) label.textContent = "Muted";
      btn.title = "Audio muted — click to unmute";
      btn.setAttribute("aria-label", "Unmute shared audio");
    } else {
      if (label) label.textContent = "Audio";
      btn.title = "Audio playing — click to mute";
      btn.setAttribute("aria-label", "Mute shared audio");
    }
  }

  function applyGain() {
    if (gain && ctx) gain.gain.setValueAtTime(muted ? 0 : 1, ctx.currentTime);
  }

  // ── AudioContext / graph ────────────────────────────────────────────────────────────
  function ensureContext(sampleRate) {
    if (ctx) return;
    const AC = window.AudioContext || window.webkitAudioContext;
    if (!AC) return;
    // Match the source rate so no resampling is needed on the copy path (Chrome/Safari honor the hint).
    try {
      ctx = new AC({ sampleRate, latencyHint: "interactive" });
    } catch (_) {
      ctx = new AC();
    }
    gain = ctx.createGain();
    gain.gain.value = muted ? 0 : 1;
    gain.connect(ctx.destination);
    nextStartTime = 0;
    // Autoplay policy: a fresh context may be suspended until a user gesture. Reflect that in the button.
    needsGesture = ctx.state === "suspended";
    if (needsGesture) tryResume();
  }

  function tryResume() {
    if (!ctx) return;
    ctx.resume().then(
      () => {
        needsGesture = ctx.state === "suspended";
        refreshBtn();
      },
      () => {
        needsGesture = true;
        refreshBtn();
      },
    );
  }

  // ── Decoder ───────────────────────────────────────────────────────────────────────
  function buildDecoder() {
    const dec = new AudioDecoder({
      output: (audioData) => {
        audioErrors = 0; // a packet decoded → the codec works; reset the retry cap
        try { playAudioData(audioData); } finally { audioData.close(); }
      },
      error: () => {
        // Cap the reset/retry loop: an unusable Opus decoder fires this on every packet, and blindly
        // rebuilding is an infinite loop. After MAX_AUDIO_RETRIES, give up on audio for this session
        // (video is unaffected). Never log audio state (Inv 8).
        audioErrors++;
        if (audioErrors > MAX_AUDIO_RETRIES) {
          opusSupported = false;
          resetDecoder();
          hideAudioUnsupported();
          return;
        }
        resetDecoder();
      },
    });
    dec.configure({
      codec: "opus",
      sampleRate: cfg.sampleRate,
      numberOfChannels: cfg.channels,
    });
    return dec;
  }

  // Audio is unavailable on this engine: hide the button and note it honestly, once. Content-free.
  function hideAudioUnsupported() {
    if (btn) btn.hidden = true;
    if (!noticedUnsupported) {
      noticedUnsupported = true;
      // eslint-disable-next-line no-console
      console.info("shared audio unavailable: this webview engine has no usable Opus decoder");
    }
  }

  // Probe Opus support asynchronously on the first packet's format. Feature-detects isConfigSupported
  // (some engines expose AudioDecoder without it — then we optimistically try, still capped by the
  // error retry limit above).
  async function probeOpus(sampleRate, channels) {
    const config = { codec: "opus", sampleRate, numberOfChannels: channels };
    try {
      if (typeof AudioDecoder.isConfigSupported === "function") {
        const { supported } = await AudioDecoder.isConfigSupported(config);
        opusSupported = !!supported;
      } else {
        opusSupported = true; // no probe available — try, capped by MAX_AUDIO_RETRIES
      }
    } catch (_) {
      opusSupported = true; // probe threw — try, capped by MAX_AUDIO_RETRIES
    }
    if (opusSupported === false) hideAudioUnsupported();
  }

  function resetDecoder() {
    try { decoder && decoder.close(); } catch (_) {}
    decoder = null;
    // cfg is kept; the next packet re-derives it (and rebuilds) if it changed.
  }

  // ── Copy a decoded AudioData into an AudioBuffer and schedule it back-to-back ─────────
  function playAudioData(audioData) {
    if (!ctx) return;
    const channels = audioData.numberOfChannels;
    const frames = audioData.numberOfFrames;
    const rate = audioData.sampleRate || cfg.sampleRate;
    if (!frames || !channels) return;

    const buffer = ctx.createBuffer(channels, frames, rate);
    for (let ch = 0; ch < channels; ch++) {
      const plane = new Float32Array(frames);
      // f32-planar is the WebCodecs default output for Opus; copy per channel into the AudioBuffer.
      audioData.copyTo(plane, { planeIndex: ch, format: "f32-planar" });
      buffer.copyToChannel(plane, ch);
    }

    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(gain);

    const now = ctx.currentTime;
    // If our cursor has fallen behind (underrun / first packet), re-snap to now with a tiny lead so we
    // don't schedule in the past and don't accumulate latency. Otherwise chain seamlessly.
    if (nextStartTime < now + 0.01) nextStartTime = now + 0.02;
    src.start(nextStartTime);
    nextStartTime += buffer.duration;
  }

  // ── Ingest one Opus packet from the Rust audio Channel ───────────────────────────────
  function onPacket(bytes, sampleRate, channels, seq) {
    if (!("AudioDecoder" in window)) return; // no WebCodecs audio in this webview — silent, video unaffected
    if (opusSupported === false) return; // probed unusable — drop silently, video unaffected
    if (firstSeq === null) firstSeq = seq;

    // (Re)configure on the first packet or if the stream's format changed mid-session.
    if (!cfg || cfg.sampleRate !== sampleRate || cfg.channels !== channels) {
      cfg = { sampleRate, channels };
      ensureContext(sampleRate);
      resetDecoder();
      opusSupported = null; // re-probe for the new format
      audioErrors = 0;
    }
    if (opusSupported === null) {
      // First packet (or a format change): kick off the async support probe and drop this packet.
      // The next packet proceeds once opusSupported resolves (Opus packets are self-contained, so a
      // dropped leading packet is a harmless PLC-covered glitch — priority #2, latency over fidelity).
      opusSupported = false; // provisional "don't re-kick"; probeOpus resets it to the real verdict
      probeOpus(sampleRate, channels).then(() => {}, () => {});
      return;
    }
    if (!ctx) return;
    if (!decoder || decoder.state !== "configured") {
      try { decoder = buildDecoder(); } catch (_) { return; }
    }

    if (!started) {
      started = true;
      refreshBtn();
    }

    // Opus packets are self-contained ("key"); timestamp derived from seq at the stream's frame duration.
    // Opus @ 20 ms → each packet advances the presentation clock; µs = (seq - firstSeq) * 20000. This is
    // only a decode-order hint (we schedule on the AudioContext clock, not these timestamps).
    const tsUs = Number(seq - firstSeq) * 20000;
    try {
      decoder.decode(
        new EncodedAudioChunk({
          type: "key",
          timestamp: tsUs >= 0 ? tsUs : 0,
          data: bytes,
        }),
      );
    } catch (_) {
      resetDecoder();
    }
  }

  // ── Mute / gesture button ───────────────────────────────────────────────────────────
  if (btn) {
    btn.addEventListener("click", () => {
      if (needsGesture) {
        // The click IS the gesture — resume the context, then leave audio unmuted.
        tryResume();
        return;
      }
      muted = !muted;
      applyGain();
      refreshBtn();
    });
  }

  // ── Public API ──────────────────────────────────────────────────────────────────────
  return {
    // Called for every audio Channel message. Parses the RAU1 header and feeds the decoder.
    handle(msg) {
      const raw = toBytes(msg);
      if (raw.byteLength < AUDIO_HEADER_LEN) return;
      const dv = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
      if (dv.getUint32(0, true) !== AUDIO_MAGIC) return; // not an RAU1 packet — drop
      const sampleRate = dv.getUint32(4, true);
      const channels = raw[8];
      const seq = dv.getBigUint64(9, true);
      const opus = raw.subarray(AUDIO_HEADER_LEN);
      if (!sampleRate || !channels || opus.byteLength === 0) return;
      onPacket(opus, sampleRate, channels, seq);
    },
    // Tear everything down on session end (Inv 8: no audio state lingering after a session).
    reset() {
      try { decoder && decoder.close(); } catch (_) {}
      decoder = null;
      if (ctx) {
        try { ctx.close(); } catch (_) {}
      }
      ctx = null;
      gain = null;
      cfg = null;
      firstSeq = null;
      nextStartTime = 0;
      started = false;
      needsGesture = false;
      opusSupported = null; // re-probe support on the next session
      audioErrors = 0;
      noticedUnsupported = false;
      // Keep the user's mute preference across reconnects within the app run; but hide the control until
      // audio flows again.
      refreshBtn();
    },
  };
})();

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

// System-audio-sharing opt-in (Share role, default OFF). Like the clipboard toggle this is a host-side
// authorization choice (Inv 1): the viewer only hears this machine when the owner ticks it. The Rust
// side gates the audio grant on it. Independent of a live viewer — the host opts in ahead of time, and
// the `audio-active` / `audio-inactive` events (below) drive the always-visible "AUDIO SHARED"
// disclosure (Inv 7) only once audio is actually flowing.
const shareAudioCb = document.getElementById("share-audio-cb");
if (shareAudioCb) {
  shareAudioCb.addEventListener("change", () => {
    invoke("set_audio_allowed", { allowed: shareAudioCb.checked }).catch(() => {});
  });
}
// The "🔊 AUDIO SHARED" indicator is honest and unsuppressable: it is visible on the Share view exactly
// while host audio is being transmitted (share-viewer live AND audio opted-in, as decided host-side and
// signalled by audio-active). audio-inactive hides it. It never hides while audio flows (Inv 7).
const shareAudioIndicator = document.getElementById("share-audio-indicator");
listen("audio-active", () => {
  if (shareAudioIndicator) shareAudioIndicator.hidden = false;
});
listen("audio-inactive", () => {
  if (shareAudioIndicator) shareAudioIndicator.hidden = true;
});

let active = false; // a viewer session is live

function resetState() {
  try { decoder && decoder.close(); } catch (_) {}
  decoder = null;
  sawKeyframe = false;
  decoded = 0;
  received = 0;
  lastId = null;
  gaps = 0;
  decErrors = 0;
  t0 = performance.now();
  clearFatalError(); // a fresh session clears any stale "engine can't decode" message
  annotations.clear();
  audioPlayer.reset(); // close the AudioDecoder + AudioContext; no audio state lingers (Inv 8)
}

function setLive(isLive) {
  active = isLive;
  banner.hidden = !isLive;
  if (!isLive) {
    reconnectBanner.hidden = true; // clear the reconnecting banner when the session ends
    connStats.hidden = true; // and the connection-stats readout
    audioPlayer.reset(); // stop + hide audio on any session end (incl. a host-initiated end)
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

// Start a viewer session either from a pasted `ticket` or a saved `contactId` (ticketless, ADR-093).
// Both drive the identical decode path + the same consent-gated two-phase connect host-side.
async function startSession(source) {
  if (!("VideoDecoder" in window)) {
    hud.textContent = "WebCodecs VideoDecoder unavailable in this webview.";
    return;
  }
  resetState();
  const channel = new Channel();
  channel.onmessage = onMessage;
  const onAudio = new Channel();
  onAudio.onmessage = (msg) => audioPlayer.handle(msg);
  hud.textContent = source.contactId ? "reaching your contact…" : "connecting…";
  try {
    if (source.contactId) {
      await invoke("connect_to_contact", { id: source.contactId, onFrame: channel, onAudio });
    } else {
      await invoke("connect_to_host", { ticket: source.ticket, onFrame: channel, onAudio });
    }
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
  startSession({ ticket });
});

stopBtn.addEventListener("click", stopSession);

ticketInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !active) connectBtn.click();
});

// ── Contacts (address book + ticketless connect, ADR-092/093) ────────────────────────────────────
const contactsList = document.getElementById("contacts-list");
const contactsEmpty = document.getElementById("contacts-empty");
const contactAddForm = document.getElementById("contact-add");
const contactInput = document.getElementById("contact-input");
const contactLabelInput = document.getElementById("contact-label");
const contactAddError = document.getElementById("contact-add-error");
const myCodeEl = document.getElementById("my-code");
const copyMyCodeBtn = document.getElementById("copy-my-code");
let myInvite = null;

function contactError(msg) {
  contactAddError.textContent = msg || "";
  contactAddError.hidden = !msg;
}

// Load this machine's own shareable identity (an id-only invite ticket + a human verification code).
async function loadMyIdentity() {
  try {
    const me = await invoke("my_identity");
    myCodeEl.textContent = me.code.slice(0, 19) + "…"; // grouped Crockford prefix; full value on hover
    myCodeEl.title = me.code;
    myInvite = me.ticket;
  } catch (_) {
    myCodeEl.textContent = "unavailable";
    myInvite = null;
  }
}

copyMyCodeBtn.addEventListener("click", async () => {
  if (!myInvite) return;
  try {
    await navigator.clipboard.writeText(myInvite);
    copyMyCodeBtn.textContent = "Copied ✓";
    setTimeout(() => (copyMyCodeBtn.textContent = "Copy my invite"), 1500);
  } catch (_) {
    contactError("Copy was blocked by the system — hover the code to read your full identity.");
  }
});

async function loadContacts() {
  try {
    renderContacts(await invoke("list_contacts"));
  } catch (e) {
    renderContacts([]);
    contactError("Contacts unavailable: " + e);
  }
}

function renderContacts(list) {
  contactsList.querySelectorAll(".contact-row").forEach((r) => r.remove());
  contactsEmpty.hidden = list.length > 0;
  for (const c of list) {
    const li = document.createElement("li");
    li.className = "contact-row" + (c.blocked ? " blocked" : "");

    const info = document.createElement("div");
    info.className = "contact-info";
    const name = document.createElement("div");
    name.className = "contact-name";
    name.textContent = c.label + (c.blocked ? " (blocked)" : "");
    const code = document.createElement("div");
    code.className = "contact-code";
    code.textContent = c.code.slice(0, 14) + "…"; // verification-code prefix; full value on hover
    code.title = c.code;
    info.appendChild(name);
    info.appendChild(code);

    const actions = document.createElement("div");
    actions.className = "contact-actions";

    const connectContactBtn = document.createElement("button");
    connectContactBtn.className = "primary";
    connectContactBtn.textContent = "Connect";
    connectContactBtn.disabled = c.blocked;
    connectContactBtn.title = c.blocked
      ? "Unblock to connect"
      : "Connect by identity — no ticket (works when they're online)";
    connectContactBtn.addEventListener("click", () => {
      showView("connect");
      startSession({ contactId: c.id });
    });

    const blockBtn = document.createElement("button");
    blockBtn.textContent = c.blocked ? "Unblock" : "Block";
    blockBtn.addEventListener("click", async () => {
      try {
        await invoke("set_contact_blocked", { id: c.id, blocked: !c.blocked });
        loadContacts();
      } catch (e) {
        contactError(String(e));
      }
    });

    const removeBtn = document.createElement("button");
    removeBtn.className = "danger";
    removeBtn.textContent = "Remove";
    removeBtn.addEventListener("click", async () => {
      try {
        await invoke("remove_contact", { id: c.id });
        loadContacts();
      } catch (e) {
        contactError(String(e));
      }
    });

    actions.appendChild(connectContactBtn);
    actions.appendChild(blockBtn);
    actions.appendChild(removeBtn);
    li.appendChild(info);
    li.appendChild(actions);
    contactsList.appendChild(li);
  }
}

contactAddForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  contactError("");
  const input = contactInput.value.trim();
  if (!input) {
    contactError("Paste a ticket or key first.");
    return;
  }
  try {
    await invoke("add_contact", { input, label: contactLabelInput.value.trim() });
    contactInput.value = "";
    contactLabelInput.value = "";
    loadContacts();
  } catch (err) {
    contactError(String(err));
  }
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
