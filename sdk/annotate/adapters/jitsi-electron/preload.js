// Casual Annotate — Electron preload bridge.
//
// Two shapes, because two different pages load this: the MEETING renderer (relays ops, issues
// controls) and the OVERLAY page (receives ops, emits acks and state). Both get the narrowest
// surface that works — only cloneable data and callbacks cross, never objects or handles.

import { contextBridge, ipcRenderer } from 'electron';

import { CH } from './main.js';

const on = (channel, fn) => {
    const cb = (_e, payload) => fn(payload);
    ipcRenderer.on(channel, cb);
    return () => ipcRenderer.removeListener(channel, cb);
};

/** Install in the preload of the window hosting the Jitsi iframe. */
export function installAnnotateBridge() {
    contextBridge.exposeInMainWorld('casualAnnotate', {
        /** Show the overlay. Resolves `{ok}` or `{ok:false, reason}` — the reason must reach the UI. */
        start: sourceId => ipcRenderer.invoke(CH.START, sourceId),
        stop: () => ipcRenderer.invoke(CH.STOP),
        /** Forward one received endpoint message to the overlay, which owns the session. */
        forwardOp: (sender, msg) => ipcRenderer.send(CH.OP, { sender, msg }),
        /** admit / mute / unmute / clear / roster — see `overlay-page.js`. */
        control: payload => ipcRenderer.send(CH.CONTROL, payload),
        /** The overlay wants something on the wire (an ack, or the roster broadcast). */
        onEmit: fn => on(CH.EMIT, fn),
        /** Render-state summary for the meeting UI: counts, participants, legacy-peer warning. */
        onState: fn => on(CH.STATE, fn),
    });
}

/** Install in the preload of the OVERLAY window. */
export function installOverlayBridge() {
    contextBridge.exposeInMainWorld('casualAnnotateOverlay', {
        ready: () => ipcRenderer.send(CH.READY),
        onOp: fn => on(CH.OP, fn),
        onControl: fn => on(CH.CONTROL, fn),
        emit: payload => ipcRenderer.send(CH.EMIT, payload),
        state: payload => ipcRenderer.send(CH.STATE, payload),
    });
}
