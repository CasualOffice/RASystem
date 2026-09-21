// Casual Annotate — version and capability compatibility (ADR-107 §18).
//
// ── The asymmetry this file exists for ──────────────────────────────────────────────────────────
//
// The web app and the Electron app ship whenever we like. A mobile app ships when a store review
// says so, and then lands on users' devices whenever *they* say so — which for a long tail is
// "never". So the deployed population is permanently mixed, and a protocol that assumes everyone
// upgraded together is a protocol that breaks mobile.
//
// ── What saves us ───────────────────────────────────────────────────────────────────────────────
//
// VIEWING ANNOTATIONS NEEDS NO CLIENT CODE AT ALL.
//
// Marks are rendered on the sharer's real desktop and travel to everyone inside the screen-share
// video (ADR-107 §2). So every participant — an ancient mobile build, a browser we have never
// tested, someone dialled in through a gateway — sees annotations correctly and always will. There
// is no forward-compatibility problem for the 95% case because there is no code in that path.
//
// Only DRAWING requires our code. That narrows compatibility to one direction: a current sharer
// must keep understanding ops from an old annotator.
//
// ── The rules ───────────────────────────────────────────────────────────────────────────────────
//
//   R1. Accept a VERSION RANGE, never a single version.
//   R2. NEVER add a required field to an existing op. New information is an optional field, and its
//       ABSENCE must always mean the old behaviour — never an error.
//   R3. Unknown op tags are IGNORED, not treated as faults: they are probably a newer peer's
//       feature. Malformed KNOWN ops are still rejected — that is the security property, and it is
//       a different thing from version skew.
//   R4. Advertise capabilities and gate UI on them, so a new client does not offer a feature the
//       sharer cannot honour and leave the user poking a dead button.

/** Current wire version. */
export const PROTOCOL_VERSION = 1;

/**
 * Oldest version this build still accepts. Raise this ONLY when the corresponding client builds are
 * genuinely gone from the field — which, for mobile, is measured in years, not sprints.
 */
export const MIN_SUPPORTED_VERSION = 1;

/**
 * Named capabilities. A peer advertises the subset it implements; senders gate on the intersection.
 *
 * Adding a capability here is the supported way to extend the protocol: old peers simply never
 * advertise it, so they are never sent it, and nothing about their behaviour changes.
 */
export const CAPS = Object.freeze({
    /** begin/append/end, undo, clear — the baseline every version has. */
    STROKES: 'strokes',
    /** Live named cursors. */
    CURSORS: 'cursors',
    /** Author-scoped eraser (hit-tested ids). */
    ERASE: 'erase',
    /** Sharer acks completed strokes, so annotators can stop guessing their echo timing. */
    ACK: 'ack',
    /** Sharer broadcasts the colour/name roster. */
    ROSTER: 'roster',
});

/**
 * What a peer that advertises nothing is assumed to support.
 *
 * This is the load-bearing default for old mobile: it predates capability advertisement, so it says
 * nothing, and we must therefore assume the v1 baseline rather than assume failure.
 */
export const BASELINE_CAPS = Object.freeze([ CAPS.STROKES ]);

/** Everything this build implements. */
export const LOCAL_CAPS = Object.freeze([
    CAPS.STROKES, CAPS.CURSORS, CAPS.ERASE, CAPS.ACK, CAPS.ROSTER,
]);

/**
 * Is this wire version one we can talk to?
 *
 * Note the two different failures. A version BELOW our floor is a genuine incompatibility we have
 * chosen to stop supporting. A version ABOVE our ceiling is a newer peer, which is not an error at
 * all — we simply cannot use their traffic, and we must not treat them as hostile for it.
 *
 * @returns {{ ok: true } | { ok: false, kind: 'reject' | 'ignore', reason: string }}
 */
export function versionOk(v) {
    if (!Number.isInteger(v)) return { ok: false, kind: 'reject', reason: 'bad-version' };
    if (v < MIN_SUPPORTED_VERSION) {
        return { ok: false, kind: 'reject', reason: `version-too-old:${v}` };
    }
    if (v > PROTOCOL_VERSION) {
        return { ok: false, kind: 'ignore', reason: `version-newer:${v}` };
    }
    return { ok: true };
}

/**
 * Capabilities we may actually use when talking to this peer.
 *
 * A peer that advertised nothing gets `BASELINE_CAPS` — absence means "old", never "broken" (R2).
 *
 * @param {string[] | undefined | null} remote
 * @param {string[]} [local]
 * @returns {Set<string>}
 */
export function negotiate(remote, local = LOCAL_CAPS) {
    const theirs = Array.isArray(remote) && remote.length ? remote : BASELINE_CAPS;
    const set = new Set();
    for (const c of theirs) if (local.includes(c)) set.add(c);
    // STROKES is the floor: every version has it, so never let negotiation talk us out of it.
    set.add(CAPS.STROKES);
    return set;
}

/**
 * The compatibility view of one peer — what a sender consults before offering a feature (R4).
 */
export class PeerProfile {
    /**
     * @param {object} [info]
     * @param {number} [info.v]       - their wire version; absent ⇒ assume the baseline.
     * @param {string[]} [info.caps]  - their capabilities; absent ⇒ `BASELINE_CAPS`.
     */
    constructor(info = {}) {
        this.version = Number.isInteger(info.v) ? info.v : MIN_SUPPORTED_VERSION;
        this.caps = negotiate(info.caps);
    }

    /** Gate a feature on this. Never assume — an old peer silently ignoring ops is a dead button. */
    supports(cap) {
        return this.caps.has(cap);
    }

    /** True when we are talking to something older than this build. */
    get isLegacy() {
        return this.version < PROTOCOL_VERSION || this.caps.size < LOCAL_CAPS.length;
    }
}
