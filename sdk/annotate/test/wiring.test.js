// Casual Annotate — end-to-end wiring tests.
//
// Drives real controllers against a real transport, with only the DOM stubbed. This is the closest
// thing to A2 that can run without Electron: ops leave an annotator, cross a (fake) bridge, land in
// a sharer's session, and the ack comes back.

import test from 'node:test';
import assert from 'node:assert/strict';

import { SharerController } from '../sharer.js';
import { AnnotatorController, estimateVideoLag } from '../annotator.js';
import { ConferenceTransport } from '../transport/jitsi.js';
import { StrokeStore } from '../core/store.js';
import { ADMIT } from '../core/session.js';
import { TOOL, CAPS, LOCAL_CAPS, hello } from '../core/index.js';

const EVENTS = { ENDPOINT_MESSAGE_RECEIVED: 'endpoint_message_received' };

/**
 * A two-endpoint bridge. Each side gets a conference stand-in; a message sent to the other side's
 * id is delivered there, preserving the sender id the way JVB does.
 */
function bridge(participants) {
    const sides = new Map();
    const make = (id) => {
        const handlers = new Map();
        const conf = {
            id,
            sent: [],
            on: (e, fn) => handlers.set(e, fn),
            off: e => handlers.delete(e),
            getParticipants: () => participants.filter(p => p.getId() !== id),
            sendMessage(payload, to) {
                this.sent.push({ payload, to });
                const deliver = (target) => {
                    const h = sides.get(target)?.handlers.get(EVENTS.ENDPOINT_MESSAGE_RECEIVED);
                    h?.({ getId: () => id }, payload);
                };
                if (to) deliver(to);
                else for (const other of sides.keys()) if (other !== id) deliver(other);
            },
        };
        sides.set(id, { conf, handlers });
        return conf;
    };
    return { make };
}

const participant = (id, name, moderator = false) => ({
    getId: () => id,
    getDisplayName: () => name,
    isModerator: () => moderator,
});

/** A surface stand-in that records what the controller told it. */
function fakeSurface() {
    return {
        colors: [], acks: [], author: null, tool: null,
        setAuthor(a) { this.author = a; },
        setColor(c) { this.colors.push(c); },
        setTool(t) { this.tool = t; },
        onAck(id, lag) { this.acks.push({ id, lag }); },
    };
}

/** Wire one sharer and one annotator onto a shared bridge. */
function pair({ admit = ADMIT.EVERYONE } = {}) {
    const people = [ participant('sharer', 'Sharer', true), participant('alice', 'Alice') ];
    const b = bridge(people);

    const sharerTransport = new ConferenceTransport(b.make('sharer'), EVENTS);
    const aliceTransport = new ConferenceTransport(b.make('alice'), EVENTS);

    const states = [];
    const sharer = new SharerController({
        emit: ({ to, op }) => (to ? sharerTransport.send(to, op) : sharerTransport.broadcast(op)),
        onState: s => states.push(s),
        admit,
    });
    sharerTransport.onOp((sender, msg) => sharer.handle(sender, msg));
    sharer.syncParticipants([
        { id: 'sharer', name: 'Sharer', moderator: true },
        { id: 'alice', name: 'Alice' },
    ]);

    const surface = fakeSurface();
    const annotator = new AnnotatorController({
        transport: aliceTransport,
        sharerId: 'sharer',
        selfId: 'alice',
        surface,
    });

    return { sharer, annotator, surface, states, aliceTransport, sharerTransport };
}

// ── the A2 path ─────────────────────────────────────────────────────────────────────────────────

test('a stroke drawn by an annotator lands in the sharer\'s store', () => {
    const { sharer, annotator } = pair();

    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.points('alice:0', [[ 0, 0 ], [ 100, 100 ], [ 200, 50 ] ]);
    annotator.strokes.end('alice:0');

    assert.equal(sharer.session.store.size, 1);
    const s = sharer.session.store.strokes()[0];
    assert.equal(s.author, 'alice', 'attributed to the relay-reported sender');
    assert.deepEqual(StrokeStore.densePoints(s), [[ 0, 0 ], [ 100, 100 ], [ 200, 50 ] ]);
});

test('the ack returns to the annotator and releases its local echo', () => {
    const { annotator, surface } = pair();

    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.points('alice:0', [[ 1, 1 ] ]);
    annotator.strokeEnded('alice:0');
    annotator.strokes.end('alice:0');

    assert.equal(surface.acks.length, 1, 'the surface must be told the stroke landed');
    assert.equal(surface.acks[0].id, 'alice:0');
    assert.ok(surface.acks[0].lag > 0, 'and given a lag to hold for');
});

test('the annotator learns its assigned colour from the roster — it never picks one', () => {
    const { surface } = pair();
    assert.ok(surface.colors.length > 0, 'a colour must have been pushed down');
    assert.match(surface.colors.at(-1), /^#[0-9a-f]{6}$/);
});

test('two annotators get different colours', () => {
    const { sharer } = pair();
    const colors = sharer.state().participants.map(p => p.color);
    assert.equal(new Set(colors).size, colors.length, 'colours must be distinct');
});

test('the sharer advertises its capabilities and the annotator gates on them', () => {
    const { annotator } = pair();
    assert.equal(annotator.supports(CAPS.ERASE), true);
    assert.equal(annotator.supports(CAPS.CURSORS), true);
    assert.equal(annotator.availableTools().erase, true);
});

test('an annotator talking to a BASELINE sharer hides the features it cannot use', () => {
    const { annotator, aliceTransport } = pair();
    // A sharer that predates erase/cursors — exactly what an old build advertises.
    aliceTransport._deliver('sharer', {
        name: 'casual-annotate', v: 1, sid: 's', op: 'roster',
        colors: { alice: '#1e90ff' }, names: { alice: 'Alice' }, caps: [ CAPS.STROKES ],
    });
    assert.equal(annotator.supports(CAPS.ERASE), false);
    assert.equal(annotator.availableTools().erase, false, 'no dead buttons (compat R4)');
    assert.equal(annotator.availableTools().tools.length, 4, 'drawing always remains');
});

test('a roster forged by a non-sharer is ignored', () => {
    const { annotator, aliceTransport } = pair();
    const before = annotator.color;
    aliceTransport._deliver('mallory', {
        name: 'casual-annotate', v: 1, sid: 's', op: 'roster',
        colors: { alice: '#000000' }, names: {},
    });
    assert.equal(annotator.color, before, 'only the sharer may assign colours');
});

test('cursors reach the sharer and carry the drawer\'s name', () => {
    const { sharer, annotator } = pair();
    annotator.pointerMoved([ 1234, 5678 ]);
    const c = sharer.session.cursors.get('alice');
    assert.deepEqual({ x: c.x, y: c.y }, { x: 1234, y: 5678 });
    assert.equal(sharer.session.nameOf('alice'), 'Alice');
});

test('an annotator cannot erase another participant\'s stroke end to end', () => {
    const { sharer, annotator, sharerTransport } = pair();

    // The sharer themself draws (applied locally, as the sharer's own client would).
    sharer.handle('sharer', { name: 'casual-annotate', v: 1, sid: 's', op: 'begin', id: 'sharer:1', tool: TOOL.PEN });
    sharer.handle('sharer', { name: 'casual-annotate', v: 1, sid: 's', op: 'end', id: 'sharer:1' });

    // Alice tries to erase it by id.
    annotator.transport.send('sharer', { op: 'erase', ids: [ 'sharer:1' ] });

    assert.equal(sharer.session.store.size, 1, "the sharer's stroke must survive");
});

test('revoke clears everything and refuses further drawing', () => {
    const { sharer, annotator } = pair();
    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.end('alice:0');
    assert.equal(sharer.session.store.size, 1);

    sharer.revoke();
    annotator.strokes.begin('alice:1', TOOL.PEN);
    annotator.strokes.end('alice:1');
    assert.equal(sharer.session.store.size, 0, 'revoked means revoked');
});

test('annotation is refused entirely until the sharer enables it', () => {
    const { sharer, annotator } = pair({ admit: ADMIT.NONE });
    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.end('alice:0');
    assert.equal(sharer.session.store.size, 0, 'deny-by-default (Inv 1)');
});

test('a muted participant is silenced without affecting the session', () => {
    const { sharer, annotator } = pair();
    sharer.mute('alice');
    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.end('alice:0');
    assert.equal(sharer.session.store.size, 0);

    sharer.unmute('alice');
    annotator.strokes.begin('alice:1', TOOL.PEN);
    annotator.strokes.end('alice:1');
    assert.equal(sharer.session.store.size, 1);
});

test('state() tells "never granted" apart from "granted, then muted" — the panel needs both', () => {
    // An earlier version collapsed both into one `muted` flag, which made an Unmute button appear
    // for someone who had never actually been granted access in the first place, and did nothing
    // when clicked.
    const { sharer } = pair({ admit: ADMIT.ALLOWLIST });
    const byId = id => sharer.state().participants.find(p => p.id === id);

    assert.equal(byId('alice').granted, false, 'ALLOWLIST + never approved: not granted');
    assert.equal(byId('alice').muted, false, 'and not "muted" either — there was nothing to mute');

    sharer.session.pending.add('alice');   // a request landed
    assert.ok(sharer.approve('alice'));
    assert.equal(byId('alice').granted, true);
    assert.equal(byId('alice').muted, false);

    sharer.mute('alice');
    assert.equal(byId('alice').granted, true, 'muting does not revoke the underlying grant');
    assert.equal(byId('alice').muted, true);

    sharer.unmute('alice');
    assert.equal(byId('alice').granted, true);
    assert.equal(byId('alice').muted, false);
});

test('the UI state carries counts and participants, never the stroke data', () => {
    const { sharer, annotator, states } = pair();
    annotator.strokes.begin('alice:0', TOOL.PEN);
    annotator.strokes.points('alice:0', [[ 1, 1 ] ]);
    annotator.strokes.end('alice:0');

    const s = states.at(-1);
    assert.equal(s.strokes, 1);
    assert.equal(JSON.stringify(s).includes('pts'), false, 'stroke geometry must not cross to the UI');
    assert.ok(s.participants.every(p => p.color && p.name));
});

test('a departing participant frees their colour for the next joiner', () => {
    const { sharer } = pair();
    const alice = sharer.state().participants.find(p => p.id === 'alice').color;
    sharer.syncParticipants([ { id: 'sharer', name: 'Sharer', moderator: true } ]);
    sharer.syncParticipants([
        { id: 'sharer', name: 'Sharer', moderator: true },
        { id: 'carol', name: 'Carol' },
    ]);
    assert.equal(sharer.state().participants.find(p => p.id === 'carol').color, alice);
});

test('the video-lag estimate is biased long — a gap is worse than a ghost', () => {
    assert.ok(estimateVideoLag(100, 100) > 100, 'must exceed the data RTT it is derived from');
    assert.ok(estimateVideoLag(0, 0) >= 600, 'and never collapse to zero on a fast link');
});

test('hello/roster negotiation survives a client that never says hello', () => {
    const { sharer } = pair();
    sharer.handle('ghost', { name: 'casual-annotate', v: 1, sid: 's', op: 'begin', id: 'g:1', tool: TOOL.PEN });
    // Not admitted (not in the roster) but the session must not throw or corrupt.
    assert.ok(sharer.state().participants.length >= 2);
});
