// Casual Annotate — the load-bearing assumption, tested directly.
//
//   electron test/electron/capture.mjs
//
// ADR-107 §2 rests on one claim, and everything else follows from it:
//
//     Because the overlay is a transparent always-on-top window on the SHARER's real desktop,
//     its pixels are inside the screen capture, and therefore reach every participant as video.
//
// That is why there is one renderer, no replicated state, no CRDT, no late-joiner replay, and why
// mobile and browsers see annotations with no client code. If it is false, the design is wrong —
// not buggy, wrong — so it deserves a test rather than an argument.
//
// This needs no meeting, no JVB and no second machine: show the overlay, then capture the screen
// the way a screen-share does (`desktopCapturer`), and look for the overlay's ink in the captured
// pixels. What it does NOT prove is the rest of the path — encode, JVB, decode. That still needs
// two machines.

import { app, desktopCapturer, screen, systemPreferences } from 'electron';
import path from 'node:path';
import fs from 'node:fs';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import { AnnotationOverlayWindow } from '../../overlay/window.js';
import { CH } from '../../adapters/jitsi-electron/channels.js';
import { envelope, begin, append, end, TOOL } from '../../core/ops.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BUILD = process.env.ANNOTATE_BUILD ?? path.join(HERE, '.build');

const results = [];
const check = (name, ok, detail = '') => {
    results.push({ name, ok, detail });
    console.log(`${ok ? '✓' : '✗'} ${name}${detail ? `  — ${detail}` : ''}`);
};
const wrap = (op, seq) => envelope(op, { sid: 'capture', seq });

/** Palette slot 0 — the RAS default red. Distinctive enough to find in a screenshot. */
const INK = { r: 0xff, g: 0x3b, b: 0x30 };

/** Is this pixel recognisably our ink, allowing for encode/scale drift? */
function isInk(r, g, b) {
    return r > 190 && g < 130 && b < 120 && (r - g) > 90 && (r - b) > 90;
}

async function main() {
    // macOS gates screen capture behind a permission that a dev-run Electron often lacks. Without
    // it `desktopCapturer` returns a picture with no windows in it, which would look exactly like
    // "the overlay is not captured" — the false negative this test must never report as a failure.
    if (process.platform === 'darwin') {
        const status = systemPreferences.getMediaAccessStatus('screen');
        if (status !== 'granted') {
            console.log(`\n⚠ Screen Recording permission is "${status}", not "granted".`);
            console.log('  desktopCapturer would return a windowless image, so this test cannot');
            console.log('  distinguish "overlay not captured" from "nothing is captured".');
            console.log('  Grant Electron access in System Settings → Privacy & Security →');
            console.log('  Screen Recording, then re-run. Skipping rather than reporting a false result.');
            app.exit(0);
            return;
        }
    }

    const primary = screen.getPrimaryDisplay();
    const sourceId = `screen:${primary.id}:0`;

    const overlay = new AnnotationOverlayWindow({
        url: `file://${path.join(BUILD, 'annotate-overlay.html')}`,
        preload: path.join(BUILD, 'annotate-overlay-preload.js'),
    });
    const shown = overlay.show(sourceId);
    check('overlay shown on the shared display', shown.ok, shown.ok ? '' : shown.reason);
    if (!shown.ok) return finish(overlay);

    const win = overlay._win;
    await new Promise(r => (win.webContents.isLoading()
        ? win.webContents.once('did-finish-load', r) : r()));

    // Admit, then draw a thick highlighter band. Thick because the capture may be downscaled, and a
    // 3px pen line can legitimately vanish into a thumbnail — that would be a measurement artifact,
    // not a finding.
    win.webContents.send(CH.CONTROL, {
        type: 'participants', participants: [ { id: 'alice', name: 'Alice' } ],
    });
    win.webContents.send(CH.CONTROL, { type: 'admit', mode: 'everyone' });

    const pts = [];
    for (let i = 0; i <= 60; i++) {
        pts.push([ 3000 + Math.round((i / 60) * 59000), 30000 + Math.round(Math.sin(i / 6) * 9000) ]);
    }
    win.webContents.send(CH.OP, { sender: 'alice', msg: wrap(begin('alice:1', TOOL.HIGHLIGHTER), 1) });
    for (let i = 0; i < pts.length; i += 24) {
        win.webContents.send(CH.OP, {
            sender: 'alice', msg: wrap(append('alice:1', i, pts.slice(i, i + 24)), 2 + i),
        });
    }
    win.webContents.send(CH.OP, { sender: 'alice', msg: wrap(end('alice:1'), 99) });

    await win.webContents.executeJavaScript(
        'new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(r, 400))))',
    );
    const strokes = await win.webContents.executeJavaScript('window.__annotateStrokeCount ?? -1');
    check('the overlay is rendering a stroke', strokes === 1, `strokes=${strokes}`);

    // ── capture the screen exactly as a screen-share would ──────────────────────────────────────
    const sources = await desktopCapturer.getSources({
        types: [ 'screen' ],
        thumbnailSize: { width: primary.size.width, height: primary.size.height },
    });
    check('desktopCapturer returned a screen source', sources.length > 0, `${sources.length} source(s)`);
    if (!sources.length) return finish(overlay);

    const shot = sources[0].thumbnail;
    const size = shot.getSize();
    const empty = shot.isEmpty();
    check('the captured image is not empty', !empty, `${size.width}x${size.height}`);
    if (empty) return finish(overlay);

    const bmp = shot.toBitmap(); // BGRA
    let ink = 0;
    for (let i = 0; i < bmp.length; i += 4) {
        if (isInk(bmp[i + 2], bmp[i + 1], bmp[i])) ink++;
    }
    const total = size.width * size.height;
    const pct = ((ink / total) * 100).toFixed(3);

    if (process.env.ANNOTATE_CAPTURE) {
        fs.writeFileSync(process.env.ANNOTATE_CAPTURE, shot.toPNG());
        console.log(`  capture written to ${process.env.ANNOTATE_CAPTURE}`);
    }

    // The headline. A handful of stray reddish pixels could come from anything on the desktop, so
    // require a run of them consistent with a drawn band.
    check(
        'THE OVERLAY IS INSIDE THE SCREEN CAPTURE (ADR-107 §2)',
        ink > 2000,
        `${ink} ink pixels of ${total} (${pct}%)`,
    );

    finish(overlay);
}

function finish(overlay) {
    overlay?.destroy();
    const failed = results.filter(r => !r.ok);
    console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
    if (failed.length) {
        console.log('\nIf the §2 check failed while the others passed, the architecture assumption');
        console.log('is wrong and ADR-107 needs revisiting — start at §13, the video-element fallback.');
    }
    app.exit(failed.length ? 1 : 0);
}

app.whenReady().then(() => main().catch((e) => {
    console.error('capture test threw:', e);
    app.exit(1);
}));
