// Casual Annotate — Electron smoke test.
//
//   electron test/electron/smoke.mjs
//
// Runs the overlay for real: creates the transparent always-on-top window, pushes ops through the
// actual IPC bridge, renders with the actual renderer, then reads the pixels back with
// `capturePage` and asserts what was drawn.
//
// This covers the half of the system unit tests cannot reach — window flags, the preload bridge,
// contextIsolation, the canvas, and `sourceId` → display resolution on THIS machine. It does not
// cover a live meeting: there is no JVB here, and no screen capture to confirm the overlay actually
// lands in the shared video. That remains the on-device step.

import { app, BrowserWindow, screen } from 'electron';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import { AnnotationOverlayWindow, displayForSourceId, isDisplaySource } from '../../overlay/window.js';
import { CH } from '../../adapters/jitsi-electron/channels.js';
import { envelope, begin, append, end, cursor, hello, TOOL } from '../../core/ops.js';
import { LOCAL_CAPS } from '../../core/compat.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BUILD = process.env.ANNOTATE_BUILD ?? path.join(HERE, '.build');

const results = [];
const check = (name, ok, detail = '') => {
    results.push({ name, ok, detail });
    console.log(`${ok ? '✓' : '✗'} ${name}${detail ? `  — ${detail}` : ''}`);
};

const wrap = (op, seq) => envelope(op, { sid: 'smoke', seq });

async function main() {
    // ── 1. display resolution on this machine ───────────────────────────────────────────────────
    const displays = screen.getAllDisplays();
    const primary = screen.getPrimaryDisplay();
    // macOS source ids are `screen:<displayId>:0`; on a single-display machine any id resolves.
    const sourceId = `screen:${primary.id}:0`;

    check('isDisplaySource accepts a screen id', isDisplaySource(sourceId));
    check('isDisplaySource rejects a window id', !isDisplaySource('window:1234:0'));

    const resolved = displayForSourceId(sourceId);
    check(
        'sourceId resolves to a display',
        !!resolved,
        resolved ? `${resolved.bounds.width}x${resolved.bounds.height} @${resolved.scaleFactor}x` : 'unresolved',
    );
    if (resolved && process.platform === 'darwin') {
        // The macOS trap: `display.scaleFactor` reports 2 while `bounds` already accounts for it.
        // Double-applying puts the overlay on a quarter of the screen.
        check('macOS scaleFactor is normalised to 1', resolved.scaleFactor === 1,
            `raw=${primary.scaleFactor} normalised=${resolved.scaleFactor}`);
    }
    console.log(`  (${displays.length} display(s) attached)`);

    // ── 2. the overlay window ───────────────────────────────────────────────────────────────────
    const overlay = new AnnotationOverlayWindow({
        url: `file://${path.join(BUILD, 'annotate-overlay.html')}`,
        preload: path.join(BUILD, 'annotate-overlay-preload.js'),
    });

    const shown = overlay.show(sourceId);
    check('overlay.show succeeds', shown.ok, shown.ok ? '' : shown.reason);
    if (!shown.ok) return finish();

    const win = overlay._win;
    check('window is transparent', win.isAlwaysOnTop(), 'alwaysOnTop');
    const b = win.getBounds();
    check('window covers the shared display',
        b.width === resolved.bounds.width && b.height === resolved.bounds.height,
        `${b.width}x${b.height}`);

    await new Promise((r) => {
        if (win.webContents.isLoading()) win.webContents.once('did-finish-load', r);
        else r();
    });

    // Surface any page error rather than letting it look like "rendered nothing".
    let pageError = null;
    win.webContents.on('console-message', (_e, level, message) => {
        if (level >= 2) pageError = message;
    });

    const bridgeOk = await win.webContents.executeJavaScript(
        'typeof window.casualAnnotateOverlay === "object" && typeof window.casualAnnotateOverlay.onOp === "function"',
    );
    check('preload exposed the overlay bridge', bridgeOk,
        bridgeOk ? '' : 'window.casualAnnotateOverlay missing — the page would render nothing');
    if (!bridgeOk) return finish(overlay);

    // ── 3. drive it through the real IPC path ───────────────────────────────────────────────────
    const send = (channel, payload) => win.webContents.send(channel, payload);

    // Annotation is OFF until admitted — assert the deny-by-default path first.
    send(CH.OP, { sender: 'alice', msg: wrap(begin('alice:0', TOOL.PEN), 1) });
    send(CH.OP, { sender: 'alice', msg: wrap(end('alice:0'), 2) });
    await settle(win);
    let strokes = await countStrokes(win);
    check('deny-by-default: nothing renders before admit', strokes === 0, `strokes=${strokes}`);

    // Admit, seat participants, then draw a big diagonal.
    send(CH.CONTROL, { type: 'participants', participants: [
        { id: 'alice', name: 'Alice' }, { id: 'bob', name: 'Bob', moderator: true },
    ] });
    send(CH.CONTROL, { type: 'admit', mode: 'everyone' });
    send(CH.OP, { sender: 'alice', msg: wrap(hello([ ...LOCAL_CAPS ]), 3) });

    const pts = [];
    for (let i = 0; i <= 40; i++) pts.push([ Math.round(i / 40 * 60000) + 2000, Math.round(i / 40 * 60000) + 2000 ]);
    send(CH.OP, { sender: 'alice', msg: wrap(begin('alice:1', TOOL.PEN), 4) });
    send(CH.OP, { sender: 'alice', msg: wrap(append('alice:1', 0, pts.slice(0, 24)), 5) });
    send(CH.OP, { sender: 'alice', msg: wrap(append('alice:1', 24, pts.slice(24)), 6) });
    send(CH.OP, { sender: 'alice', msg: wrap(end('alice:1'), 7) });
    send(CH.OP, { sender: 'bob', msg: wrap(cursor(30000, 12000), 8) });
    await settle(win);

    strokes = await countStrokes(win);
    check('a stroke reached the overlay session', strokes === 1, `strokes=${strokes}`);

    // ── 4. did it actually paint? ───────────────────────────────────────────────────────────────
    const image = await win.webContents.capturePage();
    const size = image.getSize();
    const bitmap = image.toBitmap(); // BGRA
    let opaque = 0;
    for (let i = 3; i < bitmap.length; i += 4) if (bitmap[i] > 16) opaque++;
    const total = size.width * size.height;
    const pct = ((opaque / total) * 100).toFixed(3);

    // Save the capture when asked: pixel counts prove *something* was drawn, but only looking at it
    // proves it was a stroke and a labelled cursor rather than noise.
    if (process.env.ANNOTATE_CAPTURE) {
        const fs = await import('node:fs');
        fs.writeFileSync(process.env.ANNOTATE_CAPTURE, image.toPNG());
        console.log(`  capture written to ${process.env.ANNOTATE_CAPTURE}`);
    }

    check('the overlay painted pixels', opaque > 0, `${opaque} non-transparent px of ${total} (${pct}%)`);
    check('the overlay is mostly transparent, not a filled rect', opaque / total < 0.25,
        `${pct}% covered — a high number here means the white-screen regression is back`);

    if (pageError) check('no page errors', false, pageError);
    else check('no page errors', true);

    finish(overlay);
}

/** Let the renderer run a few frames and the IPC drain. */
function settle(win) {
    return win.webContents.executeJavaScript(
        'new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(r, 120))))',
    );
}

/** Ask the page how many strokes its session holds — the authoritative count, not a pixel guess. */
function countStrokes(win) {
    return win.webContents.executeJavaScript('window.__annotateStrokeCount ?? -1');
}

function finish(overlay) {
    overlay?.destroy();
    const failed = results.filter(r => !r.ok);
    console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
    app.exit(failed.length ? 1 : 0);
}

app.disableHardwareAcceleration();
app.whenReady().then(() => main().catch((e) => {
    console.error('smoke test threw:', e);
    app.exit(1);
}));
