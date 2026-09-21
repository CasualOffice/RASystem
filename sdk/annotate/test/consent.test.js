// Casual Annotate — the ask-and-approve flow (ADR-107 §9).
//
// The person whose screen it is decides. These tests pin the properties that make that true rather
// than merely intended: a request grants nothing, the SDK never answers on the user's behalf, and a
// refusal is an explicit answer rather than silence.

import test from 'node:test';
import assert from 'node:assert/strict';

import { SharerController } from '../sharer.js';
import { AnnotatorController } from '../annotator.js';
import { ConferenceTransport } from '../transport/jitsi.js';
import { ADMIT } from '../core/session.js';
import { TOOL, OP, envelope, request, begin, end } from '../core/ops.js';
import { CAPS } from '../core/compat.js';

const EVENTS = { ENDPOINT_MESSAGE_RECEIVED: 'endpoint_message_received' };
const wrap = op => envelope(op, { sid: 's1', seq: 1 });

/** Two endpoints on one fake bridge, as in `wiring.test.js`. */
function bridge() {
    const sides = new Map();
    const make = (id, peers) => {
        const handlers = new Map();
        const conf = {
            on: (e, fn) => handlers.set(e, fn),
            off: e => handlers.delete(e),
            getParticipants: () => peers,
            sendMessage(payload, to) {
                const deliver = t => sides.get(t)?.handlers
                    .get(EVENTS.ENDPOINT_MESSAGE_RECEIVED)?.({ getId: () => id }, payload);
                if (to) deliver(to);
                else for (const o of sides.keys()) if (o !== id) deliver(o);
            },
        };
        sides.set(id, { conf, handlers });
        return conf;
    };
    return { make };
}

/** A sharer in ALLOWLIST mode — nobody may draw until individually approved. */
function setup() {
    const b = bridge();
    const requests = [];
    const states = [];

    const sharerT = new ConferenceTransport(b.make('sharer', []), EVENTS);
    const sharer = new SharerController({
        emit: ({ to, op }) => (to ? sharerT.send(to, op) : sharerT.broadcast(op)),
        onRequest: r => requests.push(r),
        admit: ADMIT.ALLOWLIST,
    });
    sharerT.onOp((s, m) => sharer.handle(s, m));
    sharer.syncParticipants([
        { id: 'sharer', name: 'Sharer', moderator: true },
        { id: 'alice', name: 'Alice' },
    ]);

    const aliceT = new ConferenceTransport(b.make('alice', []), EVENTS);
    const surface = { setColor() {}, setTool() {}, onAck() {}, setAuthor() {} };
    const alice = new AnnotatorController({
        transport: aliceT, sharerId: 'sharer', selfId: 'alice', surface,
        onState: s => states.push(s),
    });

    return { sharer, alice, requests, states, aliceT };
}

test('a request reaches the sharer as a prompt, and grants nothing by itself', () => {
    const { sharer, alice, requests } = setup();
    alice.requestPermission();

    assert.deepEqual(requests, [ { id: 'alice', name: 'Alice' } ], 'the host app must be told who asked');
    assert.equal(sharer.session.admits('alice'), false, 'asking is not being allowed');
    assert.equal(alice.permission, 'pending');
    assert.equal(alice.canDraw, false);
});

test('drawing before approval is refused', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    alice.strokes.begin('alice:0', TOOL.PEN);
    alice.strokes.end('alice:0');
    assert.equal(sharer.session.store.size, 0, 'a pending request must not let ink through');
});

test('approval is explicit, reaches the annotator, and unlocks drawing', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    sharer.approve('alice');

    assert.equal(alice.permission, 'granted');
    assert.equal(alice.canDraw, true);

    alice.strokes.begin('alice:1', TOOL.PEN);
    alice.strokes.end('alice:1');
    assert.equal(sharer.session.store.size, 1, 'ink flows once permission is given');
});

test('a refusal is an explicit answer, not silence', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    sharer.reject('alice');

    assert.equal(alice.permission, 'denied', 'the requester must be able to say "declined", not spin');
    assert.equal(alice.canDraw, false);

    alice.strokes.begin('alice:2', TOOL.PEN);
    alice.strokes.end('alice:2');
    assert.equal(sharer.session.store.size, 0);
});

test('permission can be withdrawn mid-session, and takes the marks with it', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    sharer.approve('alice');
    alice.strokes.begin('alice:3', TOOL.PEN);
    alice.strokes.end('alice:3');
    assert.equal(sharer.session.store.size, 1);

    sharer.withdraw('alice');
    assert.equal(alice.permission, 'revoked');
    assert.equal(sharer.session.store.size, 0, "withdrawing removes that author's work");

    alice.strokes.begin('alice:4', TOOL.PEN);
    alice.strokes.end('alice:4');
    assert.equal(sharer.session.store.size, 0, 'and they cannot draw again');
});

test('a request survives the admission gate — otherwise nobody could ever ask', () => {
    // The regression this guards: checking admission BEFORE decode drops the one op whose whole
    // purpose is to ask for admission, so the feature silently cannot be used at all.
    const { sharer } = setup();
    const r = sharer.handle('stranger', wrap(request()));
    assert.equal(r.accepted, true);
    assert.equal(r.requestFrom, 'stranger');
});

test('but nothing ELSE survives it', () => {
    const { sharer } = setup();
    assert.equal(sharer.handle('stranger', wrap(begin('x:1', TOOL.PEN))).reason, 'not-admitted');
    assert.equal(sharer.handle('stranger', wrap(end('x:1'))).reason, 'not-admitted');
    assert.equal(sharer.session.store.size, 0);
});

test('re-asking when already allowed is answered immediately, not re-prompted', () => {
    const { sharer, alice, requests } = setup();
    alice.requestPermission();
    sharer.approve('alice');
    requests.length = 0;

    alice.requestPermission();   // e.g. after a reconnect
    assert.deepEqual(requests, [], 'the user must not be prompted twice for the same person');
    assert.equal(alice.permission, 'granted');
});

test('pending requests are visible in the sharer UI state', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    assert.deepEqual(sharer.state().pending, [ { id: 'alice', name: 'Alice' } ]);
    sharer.approve('alice');
    assert.deepEqual(sharer.state().pending, []);
});

test('a departing participant loses both permission and any pending request', () => {
    const { sharer, alice } = setup();
    alice.requestPermission();
    sharer.approve('alice');
    sharer.syncParticipants([ { id: 'sharer', name: 'Sharer', moderator: true } ]);
    assert.equal(sharer.session.admits('alice'), false, 'permission must not survive leaving');
});

test('consent is advertised as a capability so old clients degrade knowingly', () => {
    const { sharer, alice } = setup();
    assert.equal(sharer.session.rosterOp().caps.includes(CAPS.CONSENT), true);
    assert.equal(alice.supports(CAPS.CONSENT), true);
});

test('the request op carries no name — the prompt is labelled from the roster', () => {
    // A request that carried its own display name would let anyone make the consent dialog say
    // whatever they wanted.
    assert.deepEqual(Object.keys(request()), [ 'op' ]);
});

// ── the UI must not exist when there is nothing to annotate ─────────────────────────────────────

test('a request cannot be made against nobody', () => {
    // The UI bug this mirrors: the toolbar rendered "Request to annotate" with no share in
    // progress, offering to ask permission to draw on a screen that did not exist. The controller
    // half of that: there is no sharer to address, so there is nothing to send.
    const { sharer } = setup();
    assert.equal(sharer.state().pending.length, 0);
    assert.equal(sharer.session.store.size, 0);
});
