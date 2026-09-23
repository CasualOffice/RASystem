// Casual Annotate — live end-to-end proof, driven through the REAL UI (ADR-107, Decision 10).
//
// Everything else in this package is verified either without a network (`test/*.test.js`) or by
// typing commands into a browser console by hand (the old `README.md` runbook). Neither proves the
// thing a user actually does: click a button, see a prompt, click Allow, draw. This does.
//
// Requires a live docker-jitsi-meet — `bash test/live/setup-docker.sh` — and drives it with two real
// Chromium contexts via Playwright. Not part of `npm test`; run explicitly:
//
//     npx playwright test --config test/live/playwright.config.mjs
//
// Deliberately headless throughout, with no attempt at a REAL `getDisplayMedia()` share: that needs
// a visible, focusable browser window and a real OS capture picker, which — tried once during this
// test's own development — surfaces directly on whatever machine runs this suite (a visible Chrome
// window, a real permission popup), not something to spring on a shared or someone else's machine.
// The overlay-is-inside-the-capture architecture claim this would otherwise exercise is already
// proven headlessly and locally in `test/electron/capture-stream.mjs` — not this suite's job. Instead
// a synthetic `videoType: 'desktop'` local track (plus a synthetic `<video>` element standing in for
// the shared-screen video, so coordinate mapping has something real to map against) lets the
// consent/draw PROTOCOL — the actual subject of this test — be proven through the real UI with no
// window ever needing to be visible.

import { test, expect } from '@playwright/test';
import { readFileSync, mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');
const PORT = process.env.CASUAL_ANNOTATE_HTTP_PORT ?? '8000';
const BASE = `http://localhost:${PORT}`;
const ROOM = `AnnotateE2E-${Date.now().toString(36)}`;
const ROOM_URL = `${BASE}/${ROOM}#config.prejoinConfig.enabled=false&config.disableDeepLinking=true`
    + '&config.startWithVideoMuted=true&config.startWithAudioMuted=true';

// Explicit screenshots at the meaningful moments, saved to disk — proof to look at afterward without
// a live window competing for resources while the (headless) test runs. See playwright.config.mjs's
// own comment for why headless is the default here.
const SHOT_DIR = path.join(ROOT, 'test-results', 'e2e-screenshots');
mkdirSync(SHOT_DIR, { recursive: true });
let shotN = 0;
async function shot(page, label) {
    shotN += 1;
    await page.screenshot({ path: path.join(SHOT_DIR, `${String(shotN).padStart(2, '0')}-${label}.png`) });
}

function bundle() {
    const out = '/tmp/casual-annotate-build/casual-annotate.js';
    execFileSync('npx', [
        'esbuild', 'standalone/inject.js', '--bundle', '--format=iife', '--platform=browser',
        '--target=chrome120', `--outfile=${out}`,
    ], { cwd: ROOT, stdio: 'pipe' });
    return readFileSync(out, 'utf8');
}

/** Real jitsi-meet conference objects are readable off `window.APP` — poll rather than guess a delay. */
async function waitForRoom(page) {
    await page.waitForFunction(() => Boolean(window.APP?.conference?._room?.myUserId?.()), { timeout: 60_000 });
}

/**
 * Simulate "the sharer is sharing a screen," on BOTH sides' respective views — no real capture, no
 * visible window (see the file header for why). Has to patch both, not just the sharer's:
 * `standalone/inject.js`'s `amSharing` (the SHARER's own page, deciding whether to `becomeSharer()`)
 * checks `room.getLocalTracks()`, but `currentSharer()` (the ANNOTATOR's page, deciding who to attach
 * to — and thus whether the toolbar even appears) checks a COMPLETELY DIFFERENT object:
 * `room.getParticipantById(id).getTracks()` on the ANNOTATOR's own room. Patching only the sharer's
 * `getLocalTracks()` — an earlier version of this test did exactly that — left the annotator's
 * toolbar never appearing at all, silently testing nothing.
 *
 * Also injects a real (offscreen, 1x1-styled but non-zero `videoWidth`/`videoHeight`) `<video>`
 * element on the annotator's page for `findShareVideo()` to find. Without one, `AnnotationSurface`
 * has nothing to map pointer coordinates against and refuses to draw at all — which is CORRECT
 * behaviour (§6.2: guessing a mapping is the worst thing this feature can do), but means the actual
 * drawing this test exists to prove can only be reached with something real to point at.
 */
async function simulateSharing(sharerPage, annotatorPage) {
    const sharerId = await sharerPage.evaluate(() => window.APP.conference._room.myUserId());
    await sharerPage.evaluate(() => {
        const room = window.APP.conference._room;
        const real = room.getLocalTracks.bind(room);
        room.getLocalTracks = (...a) => {
            const tracks = real(...a);
            return [ ...tracks, { getType: () => 'video', videoType: 'desktop' } ];
        };
    });
    await annotatorPage.evaluate((id) => {
        const room = window.APP.conference._room;
        const participant = room.getParticipantById(id);
        if (!participant) throw new Error(`annotator's room has no participant for sharer id ${id}`);
        const real = participant.getTracks.bind(participant);
        participant.getTracks = (...a) =>
            [ ...real(...a), { getType: () => 'video', videoType: 'desktop' } ];

        // NOT `id="largeVideo"` — a real one already exists on the page (Jitsi's own, `videoWidth`
        // 0 here since nothing is actually playing into it), and duplicate ids are best avoided
        // rather than relying on which one document-order happens to favour. `findShareVideo()`'s
        // fallback (`standalone/inject.js`) — the biggest `<video videoWidth>0>` on the page — finds
        // this one unambiguously, since it is the ONLY one with a non-zero size in this scenario.
        const v = document.createElement('video');
        Object.defineProperty(v, 'videoWidth', { value: 1280 });
        Object.defineProperty(v, 'videoHeight', { value: 720 });
        v.style.cssText = 'position:fixed;left:0;top:0;width:400px;height:225px;opacity:0.01;z-index:-1;';
        document.body.appendChild(v);
    }, sharerId);
}

test.describe('annotate consent flow — real docker-jitsi-meet, real UI', () => {
    test('request → toast → allow → draw → undo → hostile-erase-refused → revoke', async ({ browser }) => {
        const sdkBundle = bundle();

        const sharerCtx = await browser.newContext();
        const annotatorCtx = await browser.newContext();
        await sharerCtx.addInitScript({ content: sdkBundle });
        await annotatorCtx.addInitScript({ content: sdkBundle });

        const sharer = await sharerCtx.newPage();
        const annotator = await annotatorCtx.newPage();

        await sharer.goto(ROOM_URL);
        await annotator.goto(`${ROOM_URL}&noop=1`); // distinct URL so Playwright never coalesces nav
        await waitForRoom(sharer);
        await waitForRoom(annotator);

        // Each side joining only confirms ITS OWN join — participant discovery is separate, arriving
        // over XMPP presence a moment later. Wait for mutual visibility before anything that reads
        // `getParticipantById`/`getParticipants()` on either side (the whole rest of this test).
        const [ sharerId, annotatorId ] = await Promise.all([
            sharer.evaluate(() => window.APP.conference._room.myUserId()),
            annotator.evaluate(() => window.APP.conference._room.myUserId()),
        ]);
        await Promise.all([
            annotator.waitForFunction(
                id => Boolean(window.APP?.conference?._room?.getParticipantById?.(id)),
                sharerId, { timeout: 20_000 }),
            sharer.waitForFunction(
                id => Boolean(window.APP?.conference?._room?.getParticipantById?.(id)),
                annotatorId, { timeout: 20_000 }),
        ]);

        await simulateSharing(sharer, annotator);

        // ── the real UI, annotator side: the toolbar must appear once someone is sharing AND the
        //    sharer's own SharerController has confirmed itself alive (the sharerAvailable gate) ────
        const requestBtn = annotator.getByRole('button', { name: 'Request to annotate', exact: true });
        await expect(requestBtn).toBeVisible({ timeout: 20_000 });
        await shot(annotator, 'annotator-sees-request-button');
        await requestBtn.click();

        // ── the real UI, sharer side: the toast, not a console call ────────────────────────────────
        const toast = sharer.getByRole('alertdialog');
        await expect(toast).toBeVisible({ timeout: 10_000 });
        await shot(sharer, 'sharer-sees-consent-toast');
        await sharer.getByRole('button', { name: 'Allow', exact: true }).click();

        // ── the real UI, annotator side: tools appear once granted ─────────────────────────────────
        const penBtn = annotator.getByRole('button', { name: 'Pen', exact: true });
        await expect(penBtn).toBeVisible({ timeout: 10_000 });
        await shot(annotator, 'annotator-granted-tools-visible');
        await penBtn.click();

        // A real pointer-drag stroke on the annotation canvas, not a synthesized op.
        const canvas = annotator.locator('.ca-canvas');
        const box = await canvas.boundingBox();
        const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
        await annotator.mouse.move(cx - 80, cy);
        await annotator.mouse.down();
        for (let i = -80; i <= 80; i += 16) await annotator.mouse.move(cx + i, cy + Math.sin(i / 20) * 10);
        await annotator.mouse.up();
        await shot(annotator, 'annotator-drew-stroke-local-echo');

        // ── assert it actually landed, sharer side ──────────────────────────────────────────────────
        await sharer.waitForFunction(() => window.CasualAnnotateSession?.sharer?.session?.store?.size === 1,
            { timeout: 10_000 });
        const strokeInfo = await sharer.evaluate(() => {
            const s = [ ...window.CasualAnnotateSession.sharer.session.store.strokes() ][0];
            return { author: s.author, color: window.CasualAnnotateSession.sharer.session.colorOf(s.author) };
        });
        expect(strokeInfo.author).toBe(annotatorId);
        console.log(`[e2e] stroke landed, author=${strokeInfo.author} color=${strokeInfo.color}`);

        // ── undo, through the real button ───────────────────────────────────────────────────────────
        // The accessible name is the `aria-label` `surface/toolbar.js` sets ("Undo my last stroke"),
        // not the visible "Undo" text alone — `aria-label` wins over text content in name computation.
        await annotator.getByRole('button', { name: 'Undo my last stroke', exact: true }).click();
        await sharer.waitForFunction(() => window.CasualAnnotateSession?.sharer?.session?.store?.size === 0,
            { timeout: 10_000 });

        // ── a second stroke, then a hostile forged erase from an unadmitted identity is refused ─────
        await annotator.mouse.move(cx - 40, cy - 40);
        await annotator.mouse.down();
        await annotator.mouse.move(cx + 40, cy + 40);
        await annotator.mouse.up();
        await sharer.waitForFunction(() => window.CasualAnnotateSession?.sharer?.session?.store?.size === 1,
            { timeout: 10_000 });
        const victimId = await sharer.evaluate(() =>
            [ ...window.CasualAnnotateSession.sharer.session.store.strokes() ][0].id);
        const hostileResult = await sharer.evaluate((vid) => {
            // The relay-reported sender is what matters (§7.2) — 'someone-else' never sent this op,
            // so this simulates a forged claim the way a hostile peer would send one, not a trusted
            // local call. This two-participant room only lets this exercise the FIRST gate (an
            // unadmitted sender is refused outright, `not-admitted`) — the finer-grained property,
            // that a properly-admitted SECOND annotator still cannot erase a stroke that is not
            // theirs (`nothing-erased`, the store's own author-filter), needs a third identity this
            // live room does not have and is already covered without one:
            // `test/wiring.test.js`'s "an annotator cannot erase another participant's stroke end to
            // end". Both gates matter; this proves the one this room can actually exercise.
            return window.CasualAnnotateSession.sharer.handle('someone-else', {
                name: 'casual-annotate', v: 1, sid: 'hostile', op: 'erase', ids: [ vid ],
            });
        }, victimId);
        expect(hostileResult.reason).toBe('not-admitted');
        await sharer.waitForFunction(() => window.CasualAnnotateSession?.sharer?.session?.store?.size === 1);
        console.log('[e2e] forged erase from an unadmitted sender correctly refused; victim stroke survives');

        // ── revoke, through the sharer's own management panel — not the console ─────────────────────
        // The accessible name includes the live count span ("Annotators 1"), not just the title —
        // match by substring rather than pin an exact string that changes with the roster.
        await sharer.getByRole('button', { name: /Annotators/ }).click();
        await shot(sharer, 'sharer-management-panel-open');
        await sharer.getByRole('button', { name: /Remove .*annotation access/i }).click();

        // `SharerController.withdraw()` (`sharer.js`) deliberately drops the withdrawn participant's
        // EXISTING marks too (`store.dropAuthor`), not just future ones — so the surviving stroke
        // from the hostile-erase step above goes with it. Confirm that first, since it is what
        // "revoked" actually means here, before checking the part this step exists to prove: that a
        // draw attempted AFTER revocation never reaches the sharer at all.
        await sharer.waitForFunction(() => window.CasualAnnotateSession?.sharer?.session?.store?.size === 0,
            { timeout: 10_000 });

        const requestBtnAgain = annotator.getByRole('button', { name: 'Request to annotate', exact: true });
        await expect(requestBtnAgain).toBeVisible({ timeout: 10_000 });
        await shot(annotator, 'annotator-back-to-request-after-revoke');
        // Revocation must put the pen down (ADR-107 §9.3) — a further draw must not reach the sharer.
        await annotator.mouse.move(cx, cy - 60);
        await annotator.mouse.down();
        await annotator.mouse.move(cx + 20, cy - 40);
        await annotator.mouse.up();
        await sharer.waitForTimeout(500);
        const strokeCountAfterRevoke = await sharer.evaluate(
            () => window.CasualAnnotateSession.sharer.session.store.size);
        expect(strokeCountAfterRevoke).toBe(0); // the post-revoke draw attempt never reached the wire
        console.log('[e2e] revoke via the panel button dropped existing marks and stopped further drawing');

        await sharerCtx.close();
        await annotatorCtx.close();
    });
});
