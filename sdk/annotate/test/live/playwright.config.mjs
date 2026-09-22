// Casual Annotate — Playwright config for the live e2e suite.
//
// Deliberately minimal and separate from any host app's own Playwright config: this suite needs a
// real docker-jitsi-meet (`setup-docker.sh`) and a real X display for screen-share, so it is never
// meant to run as part of `npm test`.

export default {
    testDir: '.',
    timeout: 90_000,
    expect: { timeout: 15_000 },
    fullyParallel: false,   // both tests share one docker-jitsi-meet room sequence
    // A local docker-jitsi-meet under repeated rapid room churn (this file creates a fresh room per
    // run) is occasionally slow to complete XMPP join/presence — one retry absorbs that without
    // masking a real regression (a genuinely broken feature fails the same way on retry too).
    retries: 1,
    workers: 1,
    reporter: [ [ 'list' ] ],
    use: {
        trace: 'retain-on-failure',
        video: 'retain-on-failure',
        // Deliberately headless, with no real-capture flags. An earlier version of this config used
        // `headless: false` + `--auto-select-desktop-capture-source` to attempt a REAL
        // `getDisplayMedia()` share — that requires a visible, focusable browser window, which
        // surfaced directly on the machine's real display (a visible Chrome window, a real
        // permission popup) rather than staying contained to this test. That is a genuine surprise
        // for whoever is sitting at that machine and is not worth it: the architecture claim a real
        // capture would exercise (the overlay's pixels are inside a live screen-capture stream) is
        // already proven headlessly and locally in `test/electron/capture-stream.mjs`. This suite's
        // job is the consent/draw PROTOCOL through the real UI, which the synthetic-track fallback in
        // `e2e.spec.mjs`/`electron.spec.mjs` proves without ever needing a visible window.
    },
};
