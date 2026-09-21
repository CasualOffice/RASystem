// Casual Annotate — IPC channel names.
//
// Deliberately its own module with NO imports. `main.js` pulls in `ipcMain`, `BrowserWindow` and
// `screen`; the preload runs in a sandboxed renderer that must not bundle any of that. Both sides
// need the same channel names, so the names live here and nothing else does.

/** Namespaced so they cannot collide with the host application's own channels. */
export const CH = Object.freeze({
    START: 'casual-annotate:start',     // renderer → main   (invoke) show the overlay
    STOP: 'casual-annotate:stop',       // renderer → main   (invoke)
    OP: 'casual-annotate:op',           // renderer → main → overlay: one received op
    CONTROL: 'casual-annotate:control', // renderer → main → overlay: admit/mute/clear
    EMIT: 'casual-annotate:emit',       // overlay  → main → renderer: an op to put on the wire
    STATE: 'casual-annotate:state',     // overlay  → main → renderer: counts, roster, legacy peers
    READY: 'casual-annotate:ready',     // overlay  → main
});
