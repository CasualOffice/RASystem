// Casual Annotate — live end-to-end proof, Electron as the SHARER (ADR-107 Decision 10).
//
// `e2e.spec.mjs` proves the browser-to-browser shape. This proves the shape the whole native-dialog
// fix was actually about: a REAL Electron process, running the REAL `adapters/jitsi-electron/main.js`
// (not a mock of it), with a browser participant as the annotator against the same live
// docker-jitsi-meet. It asserts the thing that had never been observed working — a request reaching
// the app, the native-dialog seam actually firing, and the resulting stroke landing in the overlay
// window's own rendered pixels — not a jitsi-meet-electron checkout, which `test/electron/e2e-harness/`
// exists specifically to avoid needing.
//
//     node test/electron/e2e-harness/build.mjs
//     npx playwright test --config test/live/playwright.config.mjs test/live/electron.spec.mjs

import { test, expect, _electron } from '@playwright/test';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');
const HARNESS = path.join(ROOT, 'test', 'electron', 'e2e-harness');
// A SEPARATE deployment from `e2e.spec.mjs`'s, deliberately: `JitsiMeetExternalAPI` hardcodes
// `https://${domain}` with no opt-out, so this Electron path always needs a self-signed HTTPS
// docker-jitsi-meet, never the plain-HTTP one the browser-to-browser suite uses. See
// `test/live/README.md` for how to stand this one up (a second, differently-ported compose project).
const HTTPS_PORT = process.env.CASUAL_ANNOTATE_HTTPS_PORT ?? '8444';
const SERVER = `https://localhost:${HTTPS_PORT}`;

function browserBundle() {
    const out = '/tmp/casual-annotate-build/casual-annotate.js';
    execFileSync('npx', [
        'esbuild', 'standalone/inject.js', '--bundle', '--format=iife', '--platform=browser',
        '--target=chrome120', `--outfile=${out}`,
    ], { cwd: ROOT, stdio: 'pipe' });
    return readFileSync(out, 'utf8');
}

async function waitForRoom(frameOrPage) {
    await frameOrPage.waitForFunction(() => Boolean(window.APP?.conference?._room?.myUserId?.()),
        { timeout: 60_000 });
}

test('Electron sharer: injected relay reaches the native dialog, and the stroke reaches the overlay', async ({ browser }) => {
    execFileSync('node', [ path.join(HARNESS, 'build.mjs') ], { cwd: ROOT, stdio: 'inherit' });
    mkdirSync(path.join(ROOT, 'test-results', 'electron-proof'), { recursive: true });

    const electronApp = await _electron.launch({
        args: [ path.join(HARNESS, 'main.js') ],
        env: { ...process.env, CASUAL_ANNOTATE_SERVER: SERVER },
    });

    const hostPage = await electronApp.firstWindow();
    await hostPage.waitForFunction(() => Boolean(window.__harness?.room), { timeout: 30_000 });
    const room = await hostPage.evaluate(() => window.__harness.room);
    console.log(`[e2e-electron] room = ${room}`);

    const jitsiFrame = hostPage.frameLocator('iframe').first();
    // The iframe's own document is what actually holds `window.APP` — reach it via `page.frames()`.
    // `window.__harness` was already awaited above, so `JitsiMeetExternalAPI` has created its iframe
    // by now; poll briefly for it to be attached and navigated rather than assuming the first tick.
    let frame = null;
    await expect.poll(() => {
        frame = hostPage.frames().find(f => f.url().includes(room)) ?? null;
        return frame !== null;
    }, { timeout: 20_000 }).toBe(true);

    // ── the annotator: a real browser participant in the SAME room ─────────────────────────────────
    const annotatorCtx = await browser.newContext({ ignoreHTTPSErrors: true }); // self-signed, see above
    await annotatorCtx.addInitScript({ content: browserBundle() });
    const annotator = await annotatorCtx.newPage();
    await annotator.goto(`${SERVER}/${room}#config.prejoinConfig.enabled=false&config.disableDeepLinking=true`);
    await waitForRoom(annotator);

    // ── trigger the share from inside the Electron-hosted iframe ───────────────────────────────────
    const shareBtn = jitsiFrame.getByRole('button', { name: /start screen sharing|share.*screen/i });
    let sharing = false;
    if (await shareBtn.count().catch(() => 0)) {
        await shareBtn.first().click().catch(() => {});
        sharing = await hostPage.waitForFunction(() => {
            const room2 = [ ...document.querySelectorAll('iframe') ]
                .map(f => f.contentWindow?.APP?.conference?._room).find(Boolean);
            return (room2?.getLocalTracks?.() ?? []).some(t => t.videoType === 'desktop');
        }, { timeout: 20_000 }).then(() => true).catch(() => false);
    }
    if (!sharing) {
        // NOT the same fallback as `e2e.spec.mjs`. `renderer.js` never polls `getLocalTracks()` — it
        // learns sharing started from the External API's `screenSharingStatusChanged` event plus the
        // source id `main.js` observed (see `renderer.js`'s own comment on why). Patching
        // `getLocalTracks()` here would silently test nothing: no event fires, `startSharing()` is
        // never called, and the rest of this test would then be proving nothing about the fix it
        // exists to prove. Call the same entry point `renderer.js` itself would call instead.
        console.log('[e2e-electron] real getDisplayMedia did not resolve in time; '
            + 'calling startSharing() directly with a synthetic source id');

        // Whatever machine this runs on may genuinely have more than one real display attached —
        // found live: this one does. `overlay/window.js`'s `displayForSourceId` correctly refuses
        // multi-monitor Linux (`linux-multi-monitor-unsupported`, §8.1/§12 — deliberate, not a bug;
        // painting on the wrong monitor is worse than refusing). That refusal is real and correct,
        // but it is not what THIS test is for — it already has no coverage gap, it is the documented,
        // by-design behaviour. Proving the actual subject (does a request reach the native dialog; does
        // a stroke reach the overlay's own pixels) needs a single resolvable display, so `screen`'s own
        // `getAllDisplays` is patched down to one for this run only — the same class of test-only
        // substitution as the `getLocalTracks()` patches in `e2e.spec.mjs`, at the Electron API
        // boundary rather than the conference's.
        const displayCount = await electronApp.evaluate(({ screen }) => screen.getAllDisplays().length);
        if (displayCount > 1) {
            console.log(`[e2e-electron] ${displayCount} real displays detected; `
                + 'restricting to one for this run (see comment above)');
            await electronApp.evaluate(({ screen }) => {
                const all = screen.getAllDisplays();
                screen.getAllDisplays = () => [ all[0] ];
            });
        }

        const r = await hostPage.evaluate(() => window.__harness.annotate.startSharing('screen:0:0'));
        if (!r?.ok) throw new Error(`synthetic startSharing() was refused: ${r?.reason}`);

        // `startSharing()` tells the OVERLAY to show; it never touches `room.getLocalTracks()` (that
        // is `renderer.js`'s whole point — it learns sharing state from the External API event, not
        // by polling tracks the way the browser path does). So the Electron participant's real
        // conference room still reports NO desktop track at all, and the annotator's browser-side
        // `currentSharer()` poll (`standalone/inject.js`, checking OTHER participants' tracks) would
        // never find anyone to attach to. Same class of patch as `e2e.spec.mjs`'s fallback, applied
        // to the ANNOTATOR's view of the Electron participant specifically, plus a synthetic
        // `<video>` so `AnnotationSurface` has something real to map pointer coordinates against.
        const sharerId = await frame.evaluate(() => window.APP.conference._room.myUserId());
        await annotator.waitForFunction(
            id => Boolean(window.APP?.conference?._room?.getParticipantById?.(id)),
            sharerId, { timeout: 20_000 });
        await annotator.evaluate((id) => {
            const room = window.APP.conference._room;
            const participant = room.getParticipantById(id);
            const real = participant.getTracks.bind(participant);
            participant.getTracks = (...a) =>
                [ ...real(...a), { getType: () => 'video', videoType: 'desktop' } ];
            const v = document.createElement('video');
            Object.defineProperty(v, 'videoWidth', { value: 1280 });
            Object.defineProperty(v, 'videoHeight', { value: 720 });
            v.style.cssText = 'position:fixed;left:0;top:0;width:400px;height:225px;opacity:0.01;z-index:-1;';
            document.body.appendChild(v);
        }, sharerId);
    }

    // ── the request, from the real annotator UI ─────────────────────────────────────────────────────
    // Scripted BEFORE the click, not after: `scriptedConsentDialog` (the harness's `main.js`) reads
    // `app.__e2e.nextConsentAnswer` at the moment the native-dialog seam is actually invoked, which
    // can happen as soon as the request lands — a real race against setting it afterward. Caught
    // live: the first version of this test set it after clicking and the dialog was already answered
    // `false` (the seam's own safe default) before the "true" ever arrived, so the request was
    // genuinely denied — proving the seam fires, but not proving the grant path this test is also for.
    await electronApp.evaluate(({ app }) => { app.__e2e.nextConsentAnswer = true; });

    const requestBtn = annotator.getByRole('button', { name: 'Request to annotate', exact: true });
    try {
        await expect(requestBtn).toBeVisible({ timeout: 20_000 });
    } catch (err) {
        // Diagnostic-only: both sides already expose exactly this for "why is nothing happening"
        // (`renderer.js`'s own `window.__casualAnnotate` trace, `AnnotatorController.sharerAvailable`)
        // — read them before failing instead of guessing blind at which hop broke.
        const hostDiag = await hostPage.evaluate(() => window.__casualAnnotate ?? null).catch(() => null);
        const annotatorDiag = await annotator.evaluate(() => ({
            sharerId: window.CasualAnnotateSession?.sharerId ?? null,
            sharerAvailable: window.CasualAnnotateSession?.annotator?.sharerAvailable ?? 'no-annotator-controller',
        })).catch(() => null);
        console.error('[e2e-electron] DIAGNOSTIC host side (window.__casualAnnotate):', JSON.stringify(hostDiag, null, 2));
        console.error('[e2e-electron] DIAGNOSTIC annotator side:', JSON.stringify(annotatorDiag, null, 2));
        throw err;
    }
    await annotator.screenshot({ path: path.join(ROOT, 'test-results', 'electron-proof', '01-request-button-visible.png') });
    await requestBtn.click();

    // ── this is the actual defect this whole fix is for: does the request reach the native-dialog
    //    seam at all? Before Decision 10 it never did. ────────────────────────────────────────────────
    await expect.poll(
        () => electronApp.evaluate(({ app }) => app.__e2e.consentCalls.length),
        { timeout: 20_000, message: 'the injected relay never delivered the request to the native dialog seam' },
    ).toBeGreaterThan(0);
    const consentCalls = await electronApp.evaluate(({ app }) => app.__e2e.consentCalls);
    console.log('[e2e-electron] native dialog seam invoked with:', consentCalls);

    // ── draw, through the real UI ────────────────────────────────────────────────────────────────────
    const penBtn = annotator.getByRole('button', { name: 'Pen', exact: true });
    await expect(penBtn).toBeVisible({ timeout: 15_000 });
    await penBtn.click();
    const canvas = annotator.locator('.ca-canvas');
    const box = await canvas.boundingBox();
    const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
    await annotator.mouse.move(cx - 100, cy);
    await annotator.mouse.down();
    for (let i = -100; i <= 100; i += 10) await annotator.mouse.move(cx + i, cy);
    await annotator.mouse.up();
    await annotator.screenshot({ path: path.join(ROOT, 'test-results', 'electron-proof', '02-pen-selected-annotator-side.png') });

    // ── it must reach the OVERLAY WINDOW's own session — the authoritative one (`overlay-page.js`) ──
    const strokeCount = await electronApp.evaluate(async ({ BrowserWindow }) => {
        const overlayWin = BrowserWindow.getAllWindows().find(w => w !== undefined && w.webContents
            && w.webContents.getURL().includes('annotate-overlay'));
        if (!overlayWin) return -1;
        return overlayWin.webContents.executeJavaScript('window.__annotateStrokeCount ?? -1');
    });
    expect(strokeCount).toBe(1);

    // ── and it must have actually painted (reusing the same technique as `test/electron/smoke.mjs`) ─
    const { opaque, total, png } = await electronApp.evaluate(async ({ BrowserWindow }) => {
        const overlayWin = BrowserWindow.getAllWindows().find(w => w.webContents
            && w.webContents.getURL().includes('annotate-overlay'));
        const image = await overlayWin.webContents.capturePage();
        const size = image.getSize();
        const bitmap = image.toBitmap();
        let n = 0;
        for (let i = 3; i < bitmap.length; i += 4) if (bitmap[i] > 16) n++;
        return { opaque: n, total: size.width * size.height, png: image.toPNG().toString('base64') };
    });
    console.log(`[e2e-electron] overlay painted ${opaque}/${total} non-transparent px`);
    writeFileSync(path.join(ROOT, 'test-results', 'electron-proof', '03-overlay-window-pixels.png'), Buffer.from(png, 'base64'));
    expect(opaque).toBeGreaterThan(0);

    await electronApp.close();
    await annotatorCtx.close();
});
