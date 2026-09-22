// Casual Annotate — transport tests against a fake conference.
// Covers the §5 guarantees: unicast-only, the idempotent stroke repair, cursor throttling, and that
// attribution comes from the relay rather than the payload.

import test from 'node:test';
import assert from 'node:assert/strict';

import { ConferenceTransport, PostMessageTransport, StrokeSender, CursorSender } from '../transport/jitsi.js';
import { SharerSession, ADMIT } from '../core/session.js';
import { StrokeStore } from '../core/store.js';
import { TOOL } from '../core/ops.js';

const EVENTS = { ENDPOINT_MESSAGE_RECEIVED: 'endpoint_message_received' };

/** Minimal JitsiConference stand-in: records sends, lets tests inject received messages. */
function fakeConference(participants = []) {
    const handlers = new Map();
    return {
        sent: [],
        on(evt, fn) { handlers.set(evt, fn); },
        off(evt) { handlers.delete(evt); },
        sendMessage(payload, to, viaBridge) {
            // Unwrap the External API envelope so assertions read the op, not the transport frame.
            const inner = payload?.name === 'endpoint-text-message' ? JSON.parse(payload.text) : payload;
            this.sent.push({ payload: inner, envelope: payload, to, viaBridge });
        },
        getParticipants() { return participants; },
        /** Simulate JVB delivering a message from `id`. */
        deliver(id, payload) {
            handlers.get(EVENTS.ENDPOINT_MESSAGE_RECEIVED)?.({ getId: () => id }, payload);
        },
        /** Deliver the way the iframe External API would — wrapped. */
        deliverWrapped(id, payload) {
            handlers.get(EVENTS.ENDPOINT_MESSAGE_RECEIVED)?.({ getId: () => id },
                { name: 'endpoint-text-message', text: JSON.stringify(payload) });
        },
    };
}

test('ops are unicast through the videobridge, never broadcast', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    t.send('sharer', { op: 'undo' });

    assert.equal(conf.sent.length, 1);
    assert.equal(conf.sent[0].to, 'sharer');
    assert.equal(conf.sent[0].viaBridge, true);
    assert.equal(conf.sent[0].payload.name, 'casual-annotate');
});

test('sending without a target is a programming error, not a silent broadcast', () => {
    const t = new ConferenceTransport(fakeConference(), EVENTS);
    assert.throws(() => t.send('', { op: 'undo' }), /never broadcast/);
});

test('broadcast is a separate, explicit method (for the roster only)', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    t.broadcast({ op: 'roster', colors: {}, names: {} });
    assert.equal(conf.sent[0].to, '');
});

test('sequence numbers advance and the session id is carried', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS, { sid: 'sess-1' });
    t.send('sharer', { op: 'undo' });
    t.send('sharer', { op: 'undo' });
    assert.equal(conf.sent[0].payload.seq, 0);
    assert.equal(conf.sent[1].payload.seq, 1);
    assert.equal(conf.sent[1].payload.sid, 'sess-1');
});

test('the sender id comes from the relay, not the payload', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const seen = [];
    t.onOp((sender, msg) => seen.push({ sender, op: msg.op }));

    // A hostile payload claiming to be someone else.
    conf.deliver('bob', { name: 'casual-annotate', v: 1, sid: 's', op: 'undo', from: 'alice' });

    assert.deepEqual(seen, [ { sender: 'bob', op: 'undo' } ], 'the claimed `from` must be ignored');
});

test('foreign traffic on the same channel is filtered out', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    let count = 0;
    t.onOp(() => count++);
    conf.deliver('bob', { name: 'remote-control', type: 'mousemove' });
    conf.deliver('bob', null);
    assert.equal(count, 0);
});

test('the roster reads names and moderator status from the conference', () => {
    const conf = fakeConference([
        { getId: () => 'alice', getDisplayName: () => 'Alice', isModerator: () => true },
        { getId: () => 'bob', getDisplayName: () => 'Bob', isModerator: () => false },
    ]);
    const t = new ConferenceTransport(conf, EVENTS);
    assert.deepEqual(t.participants(), [
        { id: 'alice', name: 'Alice', moderator: true },
        { id: 'bob', name: 'Bob', moderator: false },
    ]);
});

test('StrokeSender batches points into small appends', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const s = new StrokeSender(t, 'sharer', { batchPoints: 4 });

    s.begin('a:1', TOOL.PEN);
    s.points('a:1', Array.from({ length: 10 }, (_, i) => [ i, i ]));
    s.end('a:1');

    const ops = conf.sent.map(x => x.payload.op);
    assert.equal(ops[0], 'begin');
    assert.equal(ops.at(-1), 'end');
    const appends = conf.sent.filter(x => x.payload.op === 'append');
    assert.equal(appends.length, 3, '10 points at batch 4 → two full batches plus the tail');
    assert.equal(appends.reduce((n, a) => n + a.payload.pts.length, 0), 10, 'no point is lost');
});

test('the repair resend is idempotent end-to-end — a replayed stroke changes nothing', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const s = new StrokeSender(t, 'sharer', { batchPoints: 4 });

    const pts = Array.from({ length: 10 }, (_, i) => [ i, i ]);
    s.begin('a:1', TOOL.PEN);
    s.points('a:1', pts);
    s.end('a:1');

    // Feed everything the sender produced into a real session, twice.
    const session = new SharerSession({ admit: ADMIT.EVERYONE });
    session.participantJoined('alice', 'Alice');
    const replay = () => conf.sent.forEach(x => session.handle('alice', x.payload));

    replay();
    const afterFirst = StrokeStore.densePoints(session.store.strokes()[0]).length;
    replay();
    const afterSecond = StrokeStore.densePoints(session.store.strokes()[0]).length;

    assert.equal(session.store.size, 1, 'a replay must not create a second stroke');
    assert.equal(afterSecond, afterFirst, 'a replay must not duplicate points');
    assert.equal(afterFirst, 10);
});

test('a dropped mid-stroke append is healed by the repair', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const s = new StrokeSender(t, 'sharer', { batchPoints: 4 });
    const pts = Array.from({ length: 10 }, (_, i) => [ i, i ]);
    s.begin('a:1', TOOL.PEN);
    s.points('a:1', pts);
    s.end('a:1');

    const session = new SharerSession({ admit: ADMIT.EVERYONE });
    session.participantJoined('alice', 'Alice');

    // The relay loses the second append.
    const lossy = conf.sent.filter((x, i) => !(x.payload.op === 'append' && i === 2));
    lossy.forEach(x => session.handle('alice', x.payload));
    assert.ok(StrokeStore.densePoints(session.store.strokes()[0]).length < 10, 'the loss should be visible first');

    // The repair replays the whole stroke; the store fills the gap.
    conf.sent.length = 0;
    s.repair({ id: 'a:1', tool: TOOL.PEN, pts });
    conf.sent.forEach(x => session.handle('alice', x.payload));
    assert.equal(StrokeStore.densePoints(session.store.strokes()[0]).length, 10, 'the repair must restore every point');
});

test('CursorSender throttles and coalesces', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const c = new CursorSender(t, 'sharer', { hz: 20 }); // 50ms gap

    c.move([ 1, 1 ], 0);
    c.move([ 2, 2 ], 10);
    c.move([ 3, 3 ], 20);
    assert.equal(conf.sent.length, 1, 'only the first send goes out immediately');
    assert.equal(conf.sent[0].payload.x, 1);

    c.move([ 4, 4 ], 100);
    assert.equal(conf.sent.length, 2, 'past the gap, the latest position is sent');
    assert.equal(conf.sent[1].payload.x, 4, 'coalesced to the newest position, not a backlog');
    c.dispose();
});

// ── interoperability with the iframe External API ───────────────────────────────────────────────

test('outgoing messages use the External API envelope, or an iframe peer never sees them', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    t.send('sharer', { op: 'undo' });

    // The regression this guards: `sendEndpointTextMessage` wraps as
    // { name: 'endpoint-text-message', text } and the External API surfaces ONLY that name
    // (API.js:602). A bare { name: 'casual-annotate' } on the bridge arrives and is silently
    // dropped — a browser annotator and the Electron app could not talk at all.
    assert.equal(conf.sent[0].envelope.name, 'endpoint-text-message');
    assert.equal(typeof conf.sent[0].envelope.text, 'string');
    assert.equal(JSON.parse(conf.sent[0].envelope.text).name, 'casual-annotate');
});

test('incoming messages are accepted in BOTH the wrapped and bare shapes', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    const seen = [];
    t.onOp((sender, msg) => seen.push({ sender, op: msg.op }));

    const payload = { name: 'casual-annotate', v: 1, sid: 's', op: 'undo' };
    conf.deliverWrapped('alice', payload);   // from an iframe-API peer
    conf.deliver('bob', payload);            // from another ConferenceTransport

    assert.deepEqual(seen, [ { sender: 'alice', op: 'undo' }, { sender: 'bob', op: 'undo' } ]);
});

test('a foreign endpoint-text-message is ignored rather than throwing', () => {
    const conf = fakeConference();
    const t = new ConferenceTransport(conf, EVENTS);
    let count = 0;
    t.onOp(() => count++);

    conf.deliver('bob', { name: 'endpoint-text-message', text: 'not json at all' });
    conf.deliver('bob', { name: 'endpoint-text-message', text: JSON.stringify({ hello: 'chat' }) });
    assert.equal(count, 0);
});

// ── PostMessageTransport (ADR-107 Decision 10) ──────────────────────────────────────────────────
// Covers the host side of the injected-relay bridge: no fake DOM library, just the same
// `addEventListener`/`postMessage` shape a real `window`/iframe `contentWindow` exposes.

/** A minimal `window`-shaped event bus, so `PostMessageTransport` can be driven without a real DOM. */
function fakeHost() {
    const handlers = new Set();
    return {
        addEventListener: (evt, fn) => evt === 'message' && handlers.add(fn),
        removeEventListener: (evt, fn) => evt === 'message' && handlers.delete(fn),
        /** Simulate the relay's `window.parent.postMessage(...)` arriving here. */
        // `source` mirrors what a real `MessageEvent` carries — the sender's window. Defaults to
        // `target` (the legitimate relay) so existing tests read naturally; the source-rejection
        // test below passes something else on purpose.
        deliver: (data, source) => { for (const fn of handlers) fn({ data, source }); },
        get listenerCount() { return handlers.size; },
    };
}

function fakeIframeWindow() {
    return { sent: [], postMessage(data) { this.sent.push(data); } };
}

test('PostMessageTransport sends the op verbatim — no second envelope', () => {
    const target = fakeIframeWindow();
    const t = new PostMessageTransport(target, { host: fakeHost() });

    const op = { name: 'casual-annotate', v: 1, sid: 's1', seq: 3, op: 'undo' };
    t.send('alice', op);
    t.broadcast(op);

    assert.deepEqual(target.sent, [
        { __casualAnnotateEmit: { to: 'alice', op } },
        { __casualAnnotateEmit: { to: '', op } },
    ]);
});

test('PostMessageTransport requires a target endpoint for send — never broadcasts an op by accident', () => {
    const t = new PostMessageTransport(fakeIframeWindow(), { host: fakeHost() });
    assert.throws(() => t.send('', { op: 'undo' }), /never broadcast/);
});

test('PostMessageTransport delivers ops relayed from the injected in-page script', () => {
    const host = fakeHost();
    const target = fakeIframeWindow();
    const t = new PostMessageTransport(target, { host });
    const seen = [];
    t.onOp((sender, msg) => seen.push({ sender, msg }));

    host.deliver({ __casualAnnotateWire: { type: 'op', sender: 'bob', msg: { op: 'undo' } } }, target);
    // Unrelated postMessage traffic on the same window must not be mistaken for ours.
    host.deliver({ someOtherLibrary: true }, target);

    assert.deepEqual(seen, [ { sender: 'bob', msg: { op: 'undo' } } ]);
});

test('PostMessageTransport rejects a message whose source is not the relay it was built for', () => {
    // The attribution guarantee (§7.2) this class exists to carry depends entirely on this: a
    // `window` can receive `message` events from ANY sender that can reach it, not only the one
    // relay frame this instance was constructed for. Without checking `e.source`, anything else
    // capable of posting into the host window could forge a sender id or a `moderator: true` roster
    // entry — this is the regression test for that fix.
    const host = fakeHost();
    const legitimateRelay = fakeIframeWindow();
    const somethingElse = fakeIframeWindow();
    const t = new PostMessageTransport(legitimateRelay, { host });
    const seen = [];
    t.onOp((sender, msg) => seen.push({ sender, msg }));

    host.deliver(
        { __casualAnnotateWire: { type: 'op', sender: 'sharer', msg: { op: 'grant' } } }, somethingElse);
    assert.deepEqual(seen, [], 'a forged op from an unexpected source must never reach a listener');

    host.deliver({
        __casualAnnotateWire: {
            type: 'roster', participants: [ { id: 'attacker', name: 'x', moderator: true } ],
        },
    }, somethingElse);
    assert.deepEqual(t.participants(), [], 'a forged roster from an unexpected source must be ignored');

    // The real relay still works.
    host.deliver(
        { __casualAnnotateWire: { type: 'op', sender: 'bob', msg: { op: 'undo' } } }, legitimateRelay);
    assert.deepEqual(seen, [ { sender: 'bob', msg: { op: 'undo' } } ]);
});

test('PostMessageTransport.participants() reflects the relay\'s last pushed roster snapshot', () => {
    const host = fakeHost();
    const target = fakeIframeWindow();
    const t = new PostMessageTransport(target, { host });

    assert.deepEqual(t.participants(), []);
    host.deliver({
        __casualAnnotateWire: { type: 'roster', participants: [ { id: 'a', name: 'Alice', moderator: false } ] },
    }, target);
    assert.deepEqual(t.participants(), [ { id: 'a', name: 'Alice', moderator: false } ]);

    // A later snapshot replaces, rather than merges — stale entries (a departed participant) must
    // not linger.
    host.deliver({ __casualAnnotateWire: { type: 'roster', participants: [] } }, target);
    assert.deepEqual(t.participants(), []);
});

test('PostMessageTransport.dispose() removes its listener', () => {
    const host = fakeHost();
    const t = new PostMessageTransport(fakeIframeWindow(), { host });
    assert.equal(host.listenerCount, 1);
    t.dispose();
    assert.equal(host.listenerCount, 0);
});
