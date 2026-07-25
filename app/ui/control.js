/* control.js — remote OS-input forwarding for the contacts shell, ported faithfully from the proven
 * main.js control path (Phase 3). A viewer holds a control LEASE, then this forwards pointer/keyboard/
 * wheel to the host as normalized `Input`. Every message is a REQUEST, never authority: the host
 * re-checks the lease/generation/seq/capability on every event (Inv 15). Nothing here can self-grant.
 *
 * Security posture carried over verbatim from main.js:
 *  - Inv 1: taking control asks the host owner (request_control → their Allow); this only reflects it.
 *  - Inv 4/15: held buttons + held keys are flushed on any focus/visibility/pointer-capture loss and on
 *    every control-ended transition, so nothing stays stuck on the host after the lease ends.
 *  - ADR-074: Caps/Num are synced as authoritative STATE (input_set_lock_state), never key edges.
 *  - ADR-087: pointer-lock forwards raw motion deltas for games/CAD that read movement not position.
 *  - Inv 8: nothing here logs stream content; state callbacks carry capability/status enums only.
 *
 * ON-DEVICE VERIFICATION PENDING: a faithful port of code that runs in the app, but a live two-machine
 * OS-injection run (CGEvent/XTEST/SendInput + host consent) is the hardware step.
 */
(function (global) {
  "use strict";

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
  const clampI16 = (n) => Math.max(-32768, Math.min(32767, Math.round(n)));

  /** Bind a controller to a canvas. Deps: { canvas, invoke, isLive } — isLive() is true while a viewer
   *  session is up. Callbacks: onState(state, controlling) reflects the take-control FSM in the UI. */
  function createController(opts) {
    const canvas = opts.canvas;
    const invoke = opts.invoke;
    const isLive = opts.isLive || (() => false);
    const onState = opts.onState || function () {};
    const getSwap = opts.getSwap || (() => false); // ⌘↔Ctrl remap toggle (default off)
    const getPointerLock = opts.getPointerLock || (() => false); // pointer-lock enabled toggle

    let controlling = false;
    let controlState = "idle"; // idle | requesting | granted | denied | timeout
    let controlRequestPending = false;
    let controlAutoRevert = null, controlFallbackTimer = null;
    let lastMoveAt = 0, lastCaps = null, lastNum = null;
    let lastNx = 0, lastNy = 0, pointerLocked = false;
    const heldButtons = new Set(), heldKeys = new Set();

    function clearTimers() {
      if (controlAutoRevert) { clearTimeout(controlAutoRevert); controlAutoRevert = null; }
      if (controlFallbackTimer) { clearTimeout(controlFallbackTimer); controlFallbackTimer = null; }
    }

    // Letterboxed content rect of the video inside the canvas (object-fit: contain).
    function videoContentRect() {
      const box = canvas.getBoundingClientRect();
      const vw = canvas.width, vh = canvas.height;
      if (!vw || !vh) return { left: box.left, top: box.top, width: box.width, height: box.height };
      const scale = Math.min(box.width / vw, box.height / vh);
      const w = vw * scale, h = vh * scale;
      return { left: box.left + (box.width - w) / 2, top: box.top + (box.height - h) / 2, width: w, height: h };
    }
    // Normalized 0..=65535 of the content rect, or null if the pointer is outside it.
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
      if (getSwap()) {
        const ctrl = bits & 0x02, cmd = bits & 0x08;
        bits = (bits & ~0x0a) | (ctrl ? 0x08 : 0) | (cmd ? 0x02 : 0);
      }
      return bits;
    }
    function remapHid(hid) {
      if (!getSwap()) return hid;
      switch (hid) { case 0xe0: return 0xe3; case 0xe3: return 0xe0; case 0xe4: return 0xe7; case 0xe7: return 0xe4; default: return hid; }
    }

    // Flush every held button/key so nothing stays stuck on the host across a focus/lease loss (Inv 4).
    function releaseHeldInput() {
      for (const button of heldButtons) invoke("input_pointer_button", { nx: lastNx, ny: lastNy, button, down: false }).catch(() => {});
      heldButtons.clear();
      for (const hidUsage of heldKeys) invoke("input_key", { hidUsage, down: false, modifiers: 0 }).catch(() => {});
      heldKeys.clear();
    }

    // The FSM. `controlling` is true ONLY in the granted state → no input forwards unless the lease is held.
    function setControlState(state) {
      controlState = state;
      controlling = state === "granted" && isLive();
      if (!controlling && pointerLocked && document.pointerLockElement === canvas) document.exitPointerLock();
      switch (state) {
        case "granted":
          clearTimers(); controlRequestPending = false;
          invoke("send_pointer", { x: 0, y: 0, visible: false }).catch(() => {}); // drop the look-here cursor
          break;
        case "requesting": lastCaps = null; lastNum = null; break;
        case "denied": controlRequestPending = false; releaseHeldInput(); break;
        case "timeout": controlRequestPending = false; releaseHeldInput(); break;
        default: // idle
          clearTimers(); controlRequestPending = false; lastCaps = null; lastNum = null; releaseHeldInput();
          break;
      }
      onState(state, controlling);
    }

    // The host denied/revoked (cross-machine ControlLeaseEnded, or loopback). Content-free.
    function notifyConsentDenied() {
      if (controlState === "granted") { setControlState("idle"); return; }
      if (controlState !== "requesting") return;
      setControlState("denied");
      clearTimers();
      controlAutoRevert = setTimeout(() => { if (controlState === "denied") setControlState("idle"); }, 3000);
    }

    // Take-control button behaviour: toggle off if held; else request (bounded ~95 s poll for the grant).
    async function requestOrToggle() {
      if (!isLive()) return;
      if (controlling) { setControlState("idle"); return; }
      if (controlState === "requesting" || controlRequestPending) return;
      if (controlState === "denied" || controlState === "timeout") return;
      controlRequestPending = true;
      setControlState("requesting");
      try { await invoke("request_control"); } catch (_) { setControlState("idle"); return; }
      const started = Date.now();
      clearTimers();
      controlFallbackTimer = setTimeout(() => {
        if (controlState === "requesting") {
          setControlState("timeout");
          controlAutoRevert = setTimeout(() => { if (controlState === "timeout") setControlState("idle"); }, 3000);
        }
      }, 95000);
      while (controlState === "requesting" && isLive() && Date.now() - started < 95000) {
        await new Promise((r) => setTimeout(r, 250));
        if (controlState !== "requesting") return;
        let held = false;
        try { held = await invoke("is_controlling"); } catch (_) {}
        if (held) { setControlState("granted"); return; }
      }
      if (!isLive() && controlState === "requesting") setControlState("idle");
    }

    // ── input handlers (all guarded on `controlling`) ──
    function onPointerMove(e) {
      if (!controlling) return;
      const now = performance.now();
      if (now - lastMoveAt < 8) return; // ~120 Hz cap
      lastMoveAt = now;
      if (pointerLocked) {
        const dx = clampI16(e.movementX), dy = clampI16(e.movementY);
        if (dx !== 0 || dy !== 0) invoke("input_pointer_move_relative", { dx, dy }).catch(() => {});
        return;
      }
      const p = normInput(e);
      if (p) { lastNx = p.nx; lastNy = p.ny; invoke("input_pointer_move", { nx: p.nx, ny: p.ny }).catch(() => {}); }
    }
    function forwardButton(e, down) {
      if (!controlling) return;
      const p = normInput(e);
      if (!p) return;
      e.preventDefault();
      const button = e.button === 2 ? "right" : e.button === 1 ? "middle" : "left";
      lastNx = p.nx; lastNy = p.ny;
      if (down) { heldButtons.add(button); invoke("input_pointer_move", { nx: p.nx, ny: p.ny }).catch(() => {}); }
      else heldButtons.delete(button);
      invoke("input_pointer_button", { nx: p.nx, ny: p.ny, button, down }).catch(() => {});
    }
    function onWheel(e) {
      if (!controlling) return;
      e.preventDefault();
      const clamp = (v) => Math.max(-32768, Math.min(32767, Math.round(-v / 40)));
      invoke("input_pointer_wheel", { dx: clamp(e.deltaX), dy: clamp(e.deltaY) }).catch(() => {});
    }
    function syncLockState(e) {
      const caps = e.getModifierState("CapsLock"), num = e.getModifierState("NumLock");
      if (caps === lastCaps && num === lastNum) return;
      lastCaps = caps; lastNum = num;
      invoke("input_set_lock_state", { capsLock: caps, numLock: num }).catch(() => {});
    }
    function forwardKey(e, down) {
      if (!controlling) return;
      if (e.code === "CapsLock" || e.code === "NumLock") { e.preventDefault(); syncLockState(e); return; }
      const hid = codeToHid(e.code);
      if (hid === undefined) return;
      e.preventDefault();
      syncLockState(e);
      const hidUsage = remapHid(hid);
      if (down) heldKeys.add(hidUsage); else heldKeys.delete(hidUsage);
      invoke("input_key", { hidUsage, down, modifiers: modifierBits(e) }).catch(() => {});
    }
    function onContextMenu(e) { if (controlling) e.preventDefault(); }
    function onPointerLockChange() { pointerLocked = document.pointerLockElement === canvas; }
    function onCanvasClick() {
      if (controlling && getPointerLock() && document.pointerLockElement !== canvas) canvas.requestPointerLock();
    }
    function onVisibility() { if (document.hidden) releaseHeldInput(); }

    // register (kept as named fns so dispose() can remove them)
    const wm = (e) => onPointerMove(e), wd = (e) => forwardButton(e, true), wu = (e) => forwardButton(e, false);
    const kd = (e) => forwardKey(e, true), ku = (e) => forwardKey(e, false);
    window.addEventListener("pointermove", wm);
    window.addEventListener("pointerdown", wd);
    window.addEventListener("pointerup", wu);
    window.addEventListener("contextmenu", onContextMenu);
    window.addEventListener("wheel", onWheel, { passive: false });
    window.addEventListener("keydown", kd);
    window.addEventListener("keyup", ku);
    window.addEventListener("pointercancel", releaseHeldInput);
    window.addEventListener("lostpointercapture", releaseHeldInput);
    window.addEventListener("blur", releaseHeldInput);
    document.addEventListener("visibilitychange", onVisibility);
    document.addEventListener("pointerlockchange", onPointerLockChange);
    canvas.addEventListener("click", onCanvasClick);

    return {
      requestOrToggle,
      notifyConsentDenied,
      isControlling: () => controlling,
      // Force back to idle (releasing everything) — call on session end / Stop (Inv 4).
      reset() { setControlState("idle"); },
      dispose() {
        setControlState("idle");
        window.removeEventListener("pointermove", wm);
        window.removeEventListener("pointerdown", wd);
        window.removeEventListener("pointerup", wu);
        window.removeEventListener("contextmenu", onContextMenu);
        window.removeEventListener("wheel", onWheel);
        window.removeEventListener("keydown", kd);
        window.removeEventListener("keyup", ku);
        window.removeEventListener("pointercancel", releaseHeldInput);
        window.removeEventListener("lostpointercapture", releaseHeldInput);
        window.removeEventListener("blur", releaseHeldInput);
        document.removeEventListener("visibilitychange", onVisibility);
        document.removeEventListener("pointerlockchange", onPointerLockChange);
        canvas.removeEventListener("click", onCanvasClick);
      },
    };
  }

  global.RASControl = { createController };
})(window);
