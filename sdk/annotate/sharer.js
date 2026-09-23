// Casual Annotate — the sharer controller (ADR-107 §3, §9).
//
// The sharer-side logic, with no opinion about where it runs. It owns a `SharerSession` and talks
// to the outside world through one `emit({ to, op })` callback, so the same code serves:
//
//   - the Electron overlay window, where `emit` crosses IPC to the meeting renderer, which puts it
//     on the wire (the session must live beside the renderer — see `adapters/jitsi-electron/main.js`);
//   - a single-process embedder, where `emit` is the transport directly;
//   - a test, where `emit` is an array.
//
// Everything that decides WHAT goes out lives here. Everything that decides HOW it travels does not.

import { SharerSession, ADMIT } from './core/session.js';
import { ack as ackOp, roster as rosterOp, grant as grantOp, deny as denyOp, revoke as revokeOp } from './core/ops.js';

export class SharerController {
    /**
     * @param {object} opts
     * @param {(msg: {to: string, op: object}) => void} opts.emit - `to: ''` means broadcast.
     * @param {(state: object) => void} [opts.onState]
     * @param {(req: {id: string, name: string}) => void} [opts.onRequest] - someone is asking to
     *   draw on this screen. The host app MUST put this in front of the person whose screen it is;
     *   the SDK never answers on their behalf (Inv 1).
     * @param {string} [opts.admit] - starts at `ADMIT.NONE`: annotation is off until enabled (Inv 1).
     */
    constructor({ emit, onState, onRequest, admit = ADMIT.NONE } = {}) {
        this._emit = emit;
        this._onState = onState ?? (() => {});
        this._onRequest = onRequest ?? (() => {});
        this._moderators = new Set();
        this.session = new SharerSession({
            admit,
            isModerator: id => this._moderators.has(id),
        });
    }

    /** Enable or disable annotation. */
    setAdmit(mode) {
        this.session.setAdmit(mode);
        this.pushState();
    }

    /**
     * Handle one message off the relay.
     * @param {string} sender - the RELAY-REPORTED sender. Anything else voids every ownership check.
     */
    handle(sender, msg) {
        const r = this.session.handle(sender, msg);

        // Unicast back to the author: an ack is what frees their local echo from guessing (§11.2),
        // and nobody else has any use for it.
        if (r.ack) this._emit({ to: sender, op: ackOp(r.ack.id, r.ack.t) });

        // A `hello` says what that client can do; the roster says what we can do, so it can gate its
        // UI instead of offering a button whose ops we would drop (compat R4).
        if (r.accepted && r.reason === 'hello') this.broadcastRoster();

        // Someone is asking for permission. Hand it up — a human decides, and until they do the
        // requester can still do nothing.
        if (r.requestFrom) {
            this._onRequest({ id: r.requestFrom, name: this.session.nameOf(r.requestFrom) });
            this.pushState();
        }

        // Already permitted and asking again (a reconnect, say): answer immediately.
        if (r.accepted && r.reason === 'already-allowed') this._emit({ to: sender, op: grantOp() });

        if (r.changed) this.pushState();
        return r;
    }

    /**
     * Reconcile with the conference roster.
     *
     * Colours are assigned here, so a missed join means somebody draws in a colour nobody has been
     * told about — which is why this is driven by conference events rather than by first sight of
     * an op.
     * @param {Array<{id: string, name: string, moderator?: boolean}>} participants
     */
    syncParticipants(participants) {
        const present = new Set();
        this._moderators.clear();
        for (const p of participants ?? []) {
            present.add(p.id);
            if (p.moderator) this._moderators.add(p.id);
            this.session.participantJoined(p.id, p.name);
        }
        for (const id of Object.keys(this.session.rosterOp().colors)) {
            if (!present.has(id)) this.session.participantLeft(id);
        }
        this.broadcastRoster();
        this.pushState();
    }

    /** Tell everyone their colour, their name, and what this sharer supports. */
    broadcastRoster() {
        const r = this.session.rosterOp();
        this._emit({ to: '', op: rosterOp(r.colors, r.names, r.caps, r.v) });
    }

    /**
     * The person whose screen this is said yes.
     *
     * Refuses unless that participant actually has a request outstanding, so a grant cannot happen
     * without a prompt having been raised. Returns false when there was nothing to approve — worth
     * surfacing rather than ignoring, because it means the UI and the session disagree.
     *
     * @returns {boolean}
     */
    approve(id) {
        if (!this.session.allowParticipant(id)) return false;
        this._emit({ to: id, op: grantOp() });
        this.pushState();
        return true;
    }

    /**
     * Admit someone who never asked — a saved trust decision, or a host that manages consent its
     * own way. Named to be hard to reach by accident; `approve` is the path for answering a prompt.
     */
    preAuthorize(id) {
        this.session.preAuthorize(id);
        this._emit({ to: id, op: grantOp() });
        this.pushState();
    }

    /** They said no. Answer explicitly — silence would leave the requester's UI spinning. */
    reject(id) {
        this.session.denyParticipant(id);
        this._emit({ to: id, op: denyOp() });
        this.pushState();
    }

    /** Withdraw a permission already granted, mid-session. */
    withdraw(id) {
        this.session.denyParticipant(id);
        this.session.store.dropAuthor(id);
        this._emit({ to: id, op: revokeOp() });
        this.pushState();
    }

    mute(id) {
        this.session.mute(id);
        this.pushState();
    }

    unmute(id) {
        this.session.unmute(id);
        this.pushState();
    }

    /** Wipe every mark but leave annotation enabled. */
    clearAll() {
        this.session.store.clearAll();
        this.pushState();
    }

    /** Instant stop: clear everything and refuse further ops (Inv 1). */
    revoke() {
        this.session.revoke();
        this.pushState();
    }

    /** Drop cursors nobody has moved lately. Call on a timer; returns true if anything changed. */
    tick(now = Date.now()) {
        const changed = this.session.expireCursors(now);
        if (changed) this.pushState();
        return changed;
    }

    /** A summary for the meeting UI. Never the stroke data — that would be the expensive mistake. */
    state() {
        const roster = this.session.rosterOp();
        return {
            admit: this.session.admit,
            strokes: this.session.store.size,
            cursors: this.session.cursors.size,
            pending: [ ...this.session.pending ].map(id => ({ id, name: this.session.nameOf(id) })),
            hasLegacyPeers: this.session.hasLegacyPeers(),
            participants: Object.entries(roster.colors).map(([ id, color ]) => ({
                id,
                color,
                name: this.session.nameOf(id),
                // Two genuinely different things a UI needs to tell apart — collapsing them into one
                // `muted` flag (as an earlier version of this did) made "Unmute" meaningless for
                // someone who was simply never granted access in the first place:
                granted: this.session.isGranted(id),   // holds a grant right now (mode-dependent)
                muted: this.session.isMuted(id),        // was granted, then individually silenced
            })),
        };
    }

    pushState() {
        this._onState(this.state());
    }
}

export { ADMIT };
