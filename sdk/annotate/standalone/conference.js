// Casual Annotate — shared helper for any script that runs inside a live jitsi-meet page.
//
// Used by both `standalone/inject.js` (the browser/body.html deployment) and
// `adapters/jitsi-electron/injected-relay.js` (injected into the Electron app's iframe) — the two
// places a script needs the real, live `JitsiConference` object.

/**
 * Wait for jitsi-meet to have a live conference.
 *
 * Deliberately waits FOREVER rather than timing out. The script may be present on the page (or
 * injected) well before the user finishes the prejoin screen, or the user may leave and rejoin. A
 * timeout here means giving up before the meeting starts and never arming again, with no error a
 * user could see.
 *
 * @returns {Promise<object>} the live `JitsiConference`.
 */
export async function waitForConference() {
    for (;;) {
        const room = window.APP?.conference?._room;
        if (room?.myUserId?.()) return room;
        await new Promise(r => setTimeout(r, 500));
    }
}
