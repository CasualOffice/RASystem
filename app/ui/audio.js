/* audio.js — shared-audio playback for the contacts shell, ported faithfully from the proven main.js
 * audioPlayer. Each host→controller Opus packet arrives on the audio Channel as an RAU1 blob (17-byte
 * header + raw Opus); decode with a WebCodecs AudioDecoder configured on the first packet, then play
 * each AudioData through one AudioContext, scheduling buffers back-to-back on a running clock. If we
 * fall behind (loss/stall) we re-snap the cursor to now — latency (priority #2) over gapless fidelity.
 *
 * Inv 8: audio CONTENT (Opus/PCM bytes) is never logged. Inv 12: output-only, live-only, never recorded
 * — reset() tears the whole graph down so no audio state lingers past a session. Decoupled from the DOM:
 * the caller drives a mute/enable button via toggle() + onState(), so this module owns no elements.
 *
 * ON-DEVICE VERIFICATION PENDING: faithful port; a live two-machine Opus run is the hardware step.
 */
(function (global) {
  "use strict";

  const AUDIO_MAGIC = 0x52415531; // "RAU1"
  const AUDIO_HEADER_LEN = 17; // 4 magic + 4 sampleRate + 1 channels + 8 seq
  const MAX_AUDIO_RETRIES = 3;

  function toBytes(msg) {
    if (msg instanceof ArrayBuffer) return new Uint8Array(msg);
    if (ArrayBuffer.isView(msg)) return new Uint8Array(msg.buffer, msg.byteOffset, msg.byteLength);
    if (Array.isArray(msg)) return Uint8Array.from(msg);
    throw new Error("unexpected audio payload type");
  }

  /** Deps/callbacks: onState({started, muted, needsGesture}) reflects the button; onUnsupported(msg)
   *  fires once if this engine has no usable Opus decoder (video is unaffected). */
  function createAudioPlayer(opts) {
    opts = opts || {};
    const onState = opts.onState || function () {};
    const onUnsupported = opts.onUnsupported || function () {};

    let ctx = null, decoder = null, cfg = null, gain = null;
    let nextStartTime = 0, muted = false, started = false, needsGesture = false;
    let firstSeq = null, opusSupported = null, audioErrors = 0, noticedUnsupported = false;

    const emit = () => onState({ started, muted, needsGesture });
    function applyGain() { if (gain && ctx) gain.gain.setValueAtTime(muted ? 0 : 1, ctx.currentTime); }

    function ensureContext(sampleRate) {
      if (ctx) return;
      const AC = window.AudioContext || window.webkitAudioContext;
      if (!AC) return;
      try { ctx = new AC({ sampleRate, latencyHint: "interactive" }); } catch (_) { ctx = new AC(); }
      gain = ctx.createGain();
      gain.gain.value = muted ? 0 : 1;
      gain.connect(ctx.destination);
      nextStartTime = 0;
      needsGesture = ctx.state === "suspended";
      if (needsGesture) tryResume();
    }
    function tryResume() {
      if (!ctx) return;
      ctx.resume().then(
        () => { needsGesture = ctx.state === "suspended"; emit(); },
        () => { needsGesture = true; emit(); },
      );
    }

    function buildDecoder() {
      const dec = new AudioDecoder({
        output: (audioData) => { audioErrors = 0; try { playAudioData(audioData); } finally { audioData.close(); } },
        error: () => {
          audioErrors++;
          if (audioErrors > MAX_AUDIO_RETRIES) { opusSupported = false; resetDecoder(); markUnsupported(); return; }
          resetDecoder();
        },
      });
      dec.configure({ codec: "opus", sampleRate: cfg.sampleRate, numberOfChannels: cfg.channels });
      return dec;
    }
    function markUnsupported() {
      started = false; emit();
      if (!noticedUnsupported) {
        noticedUnsupported = true;
        onUnsupported("Shared audio isn't available in this webview engine on this device. The screen share is unaffected.");
      }
    }
    async function probeOpus(sampleRate, channels) {
      const config = { codec: "opus", sampleRate, numberOfChannels: channels };
      try {
        if (typeof AudioDecoder.isConfigSupported === "function") { const { supported } = await AudioDecoder.isConfigSupported(config); opusSupported = !!supported; }
        else opusSupported = true;
      } catch (_) { opusSupported = true; }
      if (opusSupported === false) markUnsupported();
    }
    function resetDecoder() { try { decoder && decoder.close(); } catch (_) {} decoder = null; }

    function playAudioData(audioData) {
      if (!ctx) return;
      const channels = audioData.numberOfChannels, frames = audioData.numberOfFrames;
      const rate = audioData.sampleRate || cfg.sampleRate;
      if (!frames || !channels) return;
      const buffer = ctx.createBuffer(channels, frames, rate);
      for (let ch = 0; ch < channels; ch++) {
        const plane = new Float32Array(frames);
        audioData.copyTo(plane, { planeIndex: ch, format: "f32-planar" });
        buffer.copyToChannel(plane, ch);
      }
      const src = ctx.createBufferSource();
      src.buffer = buffer;
      src.connect(gain);
      const now = ctx.currentTime;
      if (nextStartTime < now + 0.01) nextStartTime = now + 0.02; // re-snap so latency never grows
      src.start(nextStartTime);
      nextStartTime += buffer.duration;
    }

    function onPacket(bytes, sampleRate, channels, seq) {
      if (!("AudioDecoder" in window)) return; // no WebCodecs audio — silent, video unaffected
      if (opusSupported === false) return;
      if (firstSeq === null) firstSeq = seq;
      if (!cfg || cfg.sampleRate !== sampleRate || cfg.channels !== channels) {
        cfg = { sampleRate, channels }; ensureContext(sampleRate); resetDecoder(); opusSupported = null; audioErrors = 0;
      }
      if (opusSupported === null) { opusSupported = false; probeOpus(sampleRate, channels).then(() => {}, () => {}); return; }
      if (!ctx) return;
      if (!decoder || decoder.state !== "configured") { try { decoder = buildDecoder(); } catch (_) { return; } }
      if (!started) { started = true; emit(); }
      const tsUs = Number(seq - firstSeq) * 20000; // 20 ms Opus frames — decode-order hint only
      try { decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: tsUs >= 0 ? tsUs : 0, data: bytes })); }
      catch (_) { resetDecoder(); }
    }

    return {
      // Feed every audio Channel message (parses the RAU1 header).
      handle(msg) {
        let raw; try { raw = toBytes(msg); } catch (_) { return; }
        if (raw.byteLength < AUDIO_HEADER_LEN) return;
        const dv = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
        if (dv.getUint32(0, true) !== AUDIO_MAGIC) return;
        const sampleRate = dv.getUint32(4, true);
        const channels = raw[8];
        const seq = dv.getBigUint64(9, true);
        const opus = raw.subarray(AUDIO_HEADER_LEN);
        if (!sampleRate || !channels || opus.byteLength === 0) return;
        onPacket(opus, sampleRate, channels, seq);
      },
      // Button action: resume a gesture-suspended context, else toggle mute.
      toggle() {
        if (needsGesture) { tryResume(); return; }
        muted = !muted; applyGain(); emit();
      },
      state() { return { started, muted, needsGesture }; },
      // Full teardown on session end (Inv 12: no audio state lingers).
      reset() {
        try { decoder && decoder.close(); } catch (_) {}
        decoder = null;
        if (ctx) { try { ctx.close(); } catch (_) {} }
        ctx = null; gain = null; cfg = null; firstSeq = null; nextStartTime = 0;
        started = false; needsGesture = false; opusSupported = null; audioErrors = 0; noticedUnsupported = false;
        emit(); // keeps the user's mute preference across reconnects; the button hides until re-started
      },
    };
  }

  global.RASAudio = { createAudioPlayer };
})(window);
