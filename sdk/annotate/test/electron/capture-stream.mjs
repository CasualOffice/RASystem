// Casual Annotate — ADR-107 §2, tested through a REAL capture stream.
//
//   electron test/electron/capture-stream.mjs
//
// A first attempt used `desktopCapturer.getSources({ thumbnailSize })` and found no overlay ink.
// That was a measurement error worth recording: the thumbnail is a PREVIEW, produced by a different
// and more restricted path than the live stream, and it is not what a screen-share sends.
//
// What Jitsi actually does is `getUserMedia` with `chromeMediaSource: 'desktop'`, so that is what
// this test does — grab a frame off the live track and look for the overlay's ink in it.

import { app, BrowserWindow, desktopCapturer, screen, session, systemPreferences } from 'electron';
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

async function main() {
    if (process.platform === 'darwin' && systemPreferences.getMediaAccessStatus('screen') !== 'granted') {
        console.log('\n⚠ Screen Recording permission not granted — skipping rather than reporting');
        console.log('  a false negative. System Settings → Privacy & Security → Screen Recording.');
        app.exit(0);
        return;
    }

    const primary = screen.getPrimaryDisplay();
    const sourceId = `screen:${primary.id}:0`;

    // ── the overlay, with a thick band of ink ───────────────────────────────────────────────────
    const overlay = new AnnotationOverlayWindow({
        url: `file://${path.join(BUILD, 'annotate-overlay.html')}`,
        preload: path.join(BUILD, 'annotate-overlay-preload.js'),
    });
    const shown = overlay.show(sourceId);
    check('overlay shown', shown.ok, shown.ok ? '' : shown.reason);
    if (!shown.ok) return finish(overlay);

    const win = overlay._win;
    await new Promise(r => (win.webContents.isLoading() ? win.webContents.once('did-finish-load', r) : r()));

    win.webContents.send(CH.CONTROL, { type: 'participants', participants: [ { id: 'alice', name: 'Alice' } ] });
    win.webContents.send(CH.CONTROL, { type: 'admit', mode: 'everyone' });

    // A band of PEN strokes across the middle — opaque on purpose.
    //
    // The first version of this used the highlighter, which draws at 0.35 alpha (`overlay.js:89`).
    // Over a dark page that blends #ff3b30 down to roughly rgb(89,21,17), which fails any sane
    // "is this red ink" test — so the band was plainly visible in the saved frame while the pixel
    // check called it a miss. The tool under test is the capture path, not alpha blending.
    let seq = 1;
    // 8 strokes x 3 ops = 24, inside MAX_OPS_PER_SEC_PER_AUTHOR (40). A first run used 14 lines,
    // ~56 ops, and the session's rate limiter correctly dropped the overflow — the limiter working
    // as designed, but it made this test look flaky. Stay inside the budget.
    for (let line = 0; line < 8; line++) {
        const y = 31000 + line * 300;
        const pts = [];
        for (let i = 0; i <= 20; i++) pts.push([ 3000 + Math.round((i / 20) * 59000), y ]);
        const id = `alice:${line}`;
        win.webContents.send(CH.OP, { sender: 'alice', msg: wrap(begin(id, TOOL.PEN), seq++) });
        for (let i = 0; i < pts.length; i += 24) {
            win.webContents.send(CH.OP, { sender: 'alice', msg: wrap(append(id, i, pts.slice(i, i + 24)), seq++) });
        }
        win.webContents.send(CH.OP, { sender: 'alice', msg: wrap(end(id), seq++) });
    }
    await win.webContents.executeJavaScript(
        'new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(r, 500))))',
    );
    const drawn = await win.webContents.executeJavaScript('window.__annotateStrokeCount');
    check('overlay is rendering every stroke (none rate-limited)', drawn === 8, `strokes=${drawn}`);

    // ── a hidden renderer that opens the real capture stream ────────────────────────────────────
    const sources = await desktopCapturer.getSources({ types: [ 'screen' ], thumbnailSize: { width: 1, height: 1 } });
    const screenSource = sources.find(s => s.display_id === String(primary.id)) ?? sources[0];
    check('found a screen source', !!screenSource, screenSource?.id);
    if (!screenSource) return finish(overlay);

    // getUserMedia needs the permission handler to say yes.
    session.defaultSession.setPermissionRequestHandler((_wc, _p, cb) => cb(true));

    const grabber = new BrowserWindow({
        show: false,
        width: 400, height: 300,
        webPreferences: { nodeIntegration: false, contextIsolation: true, offscreen: false },
    });
    // A real file:// page, not a data: URL — a data: URL is an opaque origin, which is not a
    // secure context, so `navigator.mediaDevices` is undefined there.
    const grabberHtml = path.join(BUILD, 'grabber.html');
    fs.writeFileSync(grabberHtml, '<!doctype html><html><body></body></html>');
    await grabber.loadFile(grabberHtml);

    const probe = await grabber.webContents.executeJavaScript(`(async () => {
        try {
            const stream = await navigator.mediaDevices.getUserMedia({
                audio: false,
                video: {
                    mandatory: {
                        chromeMediaSource: 'desktop',
                        chromeMediaSourceId: ${JSON.stringify(screenSource.id)},
                        maxWidth: ${primary.size.width},
                        maxHeight: ${primary.size.height}
                    }
                }
            });
            const v = document.createElement('video');
            v.srcObject = stream;
            v.muted = true;
            await v.play();
            // Let a few real frames flow — the first can be blank.
            await new Promise(r => setTimeout(r, 1200));

            const c = document.createElement('canvas');
            c.width = v.videoWidth; c.height = v.videoHeight;
            c.getContext('2d').drawImage(v, 0, 0);
            const data = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;

            let ink = 0;
            // Count only in the horizontal band where the ink was drawn (rows 45%-55%), so the
            // desktop's own red icons in the dock and menu bar cannot be mistaken for a hit.
            const y0 = Math.floor(c.height * 0.45), y1 = Math.ceil(c.height * 0.55);
            for (let y = y0; y < y1; y++) {
                for (let x = 0; x < c.width; x++) {
                    const o = (y * c.width + x) * 4;
                    const r = data[o], g = data[o + 1], b = data[o + 2];
                    // Opaque #ff3b30. Generous on the exact values (capture may resample) but
                    // strict on red DOMINANCE, so neither a dark page nor a red app icon qualifies.
                    if (r > 180 && g < 110 && b < 110 && (r - g) > 100 && (r - b) > 100) ink++;
                }
            }
            stream.getTracks().forEach(t => t.stop());
            return { ok: true, w: c.width, h: c.height, ink, band: [ y0, y1 ],
                     png: c.toDataURL('image/png') };
        } catch (e) {
            return { ok: false, error: String(e && e.message || e) };
        }
    })()`);

    check('opened a live desktop capture stream', probe.ok, probe.ok ? `${probe.w}x${probe.h}` : probe.error);
    if (!probe.ok) return finish(overlay, grabber);

    if (process.env.ANNOTATE_CAPTURE && probe.png) {
        fs.writeFileSync(process.env.ANNOTATE_CAPTURE,
            Buffer.from(probe.png.split(',')[1], 'base64'));
        console.log(`  frame written to ${process.env.ANNOTATE_CAPTURE}`);
    }

    check(
        'THE OVERLAY IS INSIDE THE CAPTURE STREAM (ADR-107 §2)',
        probe.ink > 2000,
        `${probe.ink} ink pixels in the drawn band (rows ${probe.band[0]}–${probe.band[1]})`,
    );

    finish(overlay, grabber);
}

function finish(overlay, grabber) {
    grabber?.destroy();
    overlay?.destroy();
    const failed = results.filter(r => !r.ok);
    console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
    app.exit(failed.length ? 1 : 0);
}

app.whenReady().then(() => main().catch((e) => {
    console.error('capture-stream threw:', e);
    app.exit(1);
}));
