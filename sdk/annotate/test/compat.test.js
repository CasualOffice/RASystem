// Casual Annotate — backward-compatibility tests.
//
// The deployed population is permanently mixed: web and Electron ship on demand, mobile ships when
// a store review and then a user's update habits allow. These tests pin the properties that keep an
// old client working against a current sharer.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    versionOk, negotiate, PeerProfile, CAPS, BASELINE_CAPS, LOCAL_CAPS,
    PROTOCOL_VERSION, MIN_SUPPORTED_VERSION,
} from '../core/compat.js';
import { decode, envelope, begin, append, end, hello, roster, TOOL, OP } from '../core/ops.js';
import { SharerSession, ADMIT } from '../core/session.js';
import { StrokeStore } from '../core/store.js';

const wrap = (op, v) => ({ ...envelope(op, { sid: 's1', seq: 1 }), ...(v === undefined ? {} : { v }) });

// ── version range ───────────────────────────────────────────────────────────────────────────────

test('the supported version range is accepted, not a single version', () => {
    for (let v = MIN_SUPPORTED_VERSION; v <= PROTOCOL_VERSION; v++) {
        assert.equal(versionOk(v).ok, true, `v${v} should be accepted`);
    }
});

test('a NEWER peer is ignored, not rejected — being ahead of us is not a fault', () => {
    const r = versionOk(PROTOCOL_VERSION + 1);
    assert.equal(r.ok, false);
    assert.equal(r.kind, 'ignore', 'a newer peer must never be treated as malformed or hostile');
});

test('a peer below the floor is a genuine rejection', () => {
    const r = versionOk(MIN_SUPPORTED_VERSION - 1);
    assert.equal(r.ok, false);
    assert.equal(r.kind, 'reject');
});

test('decode carries the ignore/reject distinction through to callers', () => {
    // A newer version, and an op tag we do not know: both benign.
    assert.equal(decode(wrap(begin('a:1', TOOL.PEN), PROTOCOL_VERSION + 1)).kind, 'ignore');
    assert.equal(decode(wrap({ op: 'sparkle' })).kind, 'ignore');
    // Malformed known ops stay hard rejections — that is the security property, unchanged.
    assert.equal(decode(wrap({ op: OP.BEGIN, id: 'a:1', tool: 99 })).kind, 'reject');
    assert.equal(decode(wrap(append('a:1', -1, [[ 1, 1 ]]))).kind, 'reject');
});

// ── capability negotiation ──────────────────────────────────────────────────────────────────────

test('a peer that advertises nothing is assumed BASELINE, never broken', () => {
    const p = new PeerProfile();
    assert.equal(p.supports(CAPS.STROKES), true, 'drawing must always be available');
    assert.equal(p.supports(CAPS.ERASE), false, 'but newer features are not assumed');
    assert.deepEqual([ ...negotiate(undefined) ], BASELINE_CAPS.slice());
});

test('negotiation is an intersection, and never drops the baseline', () => {
    const caps = negotiate([ CAPS.CURSORS, CAPS.ERASE, 'time-travel' ]);
    assert.equal(caps.has(CAPS.CURSORS), true);
    assert.equal(caps.has('time-travel'), false, 'we cannot do what we do not implement');
    assert.equal(caps.has(CAPS.STROKES), true, 'the floor survives any negotiation');
});

test('a hello upgrades a peer from assumed-baseline to its real capabilities', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');
    assert.equal(s.profileOf('alice').supports(CAPS.ERASE), false);

    s.handle('alice', wrap(hello([ CAPS.STROKES, CAPS.ERASE ])));
    assert.equal(s.profileOf('alice').supports(CAPS.ERASE), true);
});

test('a malformed hello degrades to baseline rather than failing the peer', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');
    const r = s.handle('alice', wrap({ op: OP.HELLO, caps: 'not-an-array', hv: 'nonsense' }));
    assert.equal(r.accepted, true, 'a bad hello must never lock a client out');
    assert.equal(s.profileOf('alice').supports(CAPS.STROKES), true);
});

test('the sharer advertises its capabilities in the roster so clients can gate their UI', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');
    const r = s.rosterOp();
    assert.deepEqual(r.caps, [ ...LOCAL_CAPS ]);
    assert.equal(r.v, PROTOCOL_VERSION);
});

test('an OLD client decoding a NEW roster keeps the parts it understands', () => {
    // A future sharer sends caps and fields this build has never seen.
    const future = {
        ...envelope(roster({ alice: '#ff3b30' }, { alice: 'Alice' }, [ 'strokes', 'holograms' ], 1), { sid: 's', seq: 1 }),
        somethingNew: { nested: true },
    };
    const d = decode(future, { fromSharer: true });
    assert.equal(d.ok, true, 'unknown fields must not fail the message (compat R2)');
    assert.deepEqual(d.op.colors, { alice: '#ff3b30' });
    assert.equal(d.op.caps.includes('holograms'), true, 'caps pass through verbatim for negotiation');
});

// ── the end-to-end property that actually matters ───────────────────────────────────────────────

test('a legacy annotator that never says hello can still draw against a current sharer', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('old-mobile', 'Someone on a 2-year-old build');

    // No hello, baseline version, no optional fields — exactly what an old client emits.
    s.handle('old-mobile', wrap(begin('old:1', TOOL.PEN)));
    s.handle('old-mobile', wrap(append('old:1', 0, [[ 0, 0 ], [ 10, 10 ]])));
    const r = s.handle('old-mobile', wrap(end('old:1')));

    assert.equal(s.store.size, 1, 'an old client must still be able to draw');
    assert.deepEqual(StrokeStore.densePoints(s.store.strokes()[0]), [[ 0, 0 ], [ 10, 10 ]]);
    assert.ok(r.ack, 'and still gets its ack');
});

test('a NEWER client\'s unknown ops are skipped without disturbing its usable ones', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');

    s.handle('alice', wrap(begin('a:1', TOOL.PEN)));
    const unknown = s.handle('alice', wrap({ op: 'lasso', id: 'a:1' }));
    s.handle('alice', wrap(append('a:1', 0, [[ 1, 1 ]])));

    assert.equal(unknown.kind, 'ignore', 'a future op must degrade, not fault');
    assert.equal(s.store.size, 1);
    assert.deepEqual(StrokeStore.densePoints(s.store.strokes()[0]), [[ 1, 1 ]], 'the rest still works');
});

test('the sharer can tell when legacy clients are present', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');
    s.handle('alice', wrap(hello([ ...LOCAL_CAPS ])));
    assert.equal(s.hasLegacyPeers(), false);

    s.participantJoined('bob', 'Bob');
    s.handle('bob', wrap(hello([ CAPS.STROKES ])));
    assert.equal(s.hasLegacyPeers(), true);
});
