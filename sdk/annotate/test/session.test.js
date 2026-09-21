// Casual Annotate — sharer session policy tests.
// These cover the security posture: deny-by-default, admission, mute, rate limiting, revoke, and
// the one op whose authority the store cannot judge (`clear: "all"`).

import test from 'node:test';
import assert from 'node:assert/strict';

import { SharerSession, ADMIT } from '../core/session.js';
import { envelope, begin, append, end, cursor, clear, undo, TOOL, CLEAR_SCOPE, LIMITS } from '../core/ops.js';

const wrap = (op, seq = 1) => envelope(op, { sid: 's1', seq });

/** A session with annotation already switched on for everyone. */
function live() {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('alice', 'Alice');
    s.participantJoined('bob', 'Bob');
    return s;
}

/** Draw one complete stroke through the session's public entry point. */
function draw(s, author, id, at = 0) {
    s.handle(author, wrap(begin(id, TOOL.PEN)), at);
    s.handle(author, wrap(append(id, 0, [[ 0, 0 ], [ 10, 10 ]])), at);
    return s.handle(author, wrap(end(id)), at);
}

test('annotation is OFF by default — deny-by-default (Inv 1)', () => {
    const s = new SharerSession();
    s.participantJoined('alice', 'Alice');
    assert.equal(s.admit, ADMIT.NONE);
    const r = s.handle('alice', wrap(begin('alice:1', TOOL.PEN)));
    assert.equal(r.accepted, false);
    assert.equal(r.reason, 'not-admitted');
    assert.equal(s.store.size, 0);
});

test('the sharer can admit everyone, then revoke instantly', () => {
    const s = live();
    draw(s, 'alice', 'alice:1');
    assert.equal(s.store.size, 1);

    s.revoke();
    assert.equal(s.store.size, 0, 'revoke clears every mark');
    assert.equal(s.handle('alice', wrap(begin('alice:2', TOOL.PEN))).accepted, false);
});

test('moderator-only admission excludes non-moderators', () => {
    const s = new SharerSession({ admit: ADMIT.MODERATORS, isModerator: a => a === 'alice' });
    s.participantJoined('alice', 'Alice');
    s.participantJoined('bob', 'Bob');
    assert.equal(s.admits('alice'), true);
    assert.equal(s.admits('bob'), false);
});

test('allowlist admission honours only named participants', () => {
    const s = new SharerSession({ admit: ADMIT.ALLOWLIST });
    s.participantJoined('alice', 'Alice');
    assert.equal(s.admits('alice'), false);
    s.allowParticipant('alice');
    assert.equal(s.admits('alice'), true);
});

test('mute silences one participant without affecting the others', () => {
    const s = live();
    s.mute('bob');
    assert.equal(s.handle('bob', wrap(begin('bob:1', TOOL.PEN))).accepted, false);
    assert.equal(s.handle('alice', wrap(begin('alice:1', TOOL.PEN))).accepted, true);
    s.unmute('bob');
    assert.equal(s.handle('bob', wrap(begin('bob:2', TOOL.PEN))).accepted, true);
});

test('rate limiting bounds a chatty peer, and recovers over time', () => {
    const s = live();
    let refused = 0;
    for (let i = 0; i < LIMITS.MAX_OPS_PER_SEC_PER_AUTHOR + 20; i++) {
        if (!s.handle('alice', wrap(cursor(1, 1)), 0).accepted) refused++;
    }
    assert.ok(refused > 0, 'a burst past the budget must be refused');
    // A second later the bucket has refilled.
    assert.equal(s.handle('alice', wrap(cursor(1, 1)), 1000).accepted, true);
});

test('rate limiting is per-author — one peer cannot starve another', () => {
    const s = live();
    for (let i = 0; i < LIMITS.MAX_OPS_PER_SEC_PER_AUTHOR + 20; i++) s.handle('alice', wrap(cursor(1, 1)), 0);
    assert.equal(s.handle('bob', wrap(cursor(1, 1)), 0).accepted, true);
});

test('clear "all" from a non-moderator is downgraded to clearing their own work', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE, isModerator: a => a === 'alice' });
    s.participantJoined('alice', 'Alice');
    s.participantJoined('bob', 'Bob');
    draw(s, 'alice', 'alice:1');
    draw(s, 'bob', 'bob:1');

    const r = s.handle('bob', wrap(clear(CLEAR_SCOPE.ALL)));
    assert.equal(r.reason, 'downgraded-to-mine');
    assert.deepEqual(s.store.strokes().map(x => x.id), [ 'alice:1' ], "bob may not wipe alice's work");

    // The moderator may.
    s.handle('alice', wrap(clear(CLEAR_SCOPE.ALL)));
    assert.equal(s.store.size, 0);
});

test('an end produces the ack the annotator holds its echo against', () => {
    const s = live();
    const r = draw(s, 'alice', 'alice:1', 12345);
    assert.deepEqual(r.ack, { id: 'alice:1', t: 12345 });
});

test('cursors are transient, labelled from the roster, and expire', () => {
    const s = live();
    s.handle('alice', wrap(cursor(100, 200)), 0);
    assert.deepEqual(
        { ...s.cursors.get('alice') },
        { x: 100, y: 200, t: 0 },
    );
    assert.equal(s.nameOf('alice'), 'Alice');
    assert.equal(s.store.size, 0, 'a cursor is not markup');

    assert.equal(s.expireCursors(5000), true);
    assert.equal(s.cursors.has('alice'), false);
});

test('colour comes from the author, and is stable and distinct', () => {
    const s = live();
    const a = s.colorOf('alice');
    assert.notEqual(a, s.colorOf('bob'));
    assert.equal(s.colorOf('alice'), a);
});

test('a departing participant frees their colour and cursor but keeps their strokes', () => {
    const s = live();
    draw(s, 'alice', 'alice:1');
    s.handle('alice', wrap(cursor(1, 1)), 0);

    s.participantLeft('alice');
    assert.equal(s.cursors.has('alice'), false);
    assert.equal(s.store.size, 1, "a departed participant's marks stay until cleared");
});

test('a name falls back to the opaque id rather than to nothing', () => {
    const s = new SharerSession({ admit: ADMIT.EVERYONE });
    s.participantJoined('ghost');
    assert.equal(s.nameOf('ghost'), 'ghost');
});

test('the session ignores traffic that is not ours', () => {
    const s = live();
    assert.equal(s.handle('alice', { name: 'remote-control', type: 'mousemove' }).reason, 'not-ours');
    assert.equal(s.handle('alice', null).reason, 'not-ours');
});

test('a garbage op from an admitted peer is refused, not partially applied', () => {
    const s = live();
    const r = s.handle('alice', wrap({ op: 'begin', id: 'alice:1', tool: 99 }));
    assert.equal(r.accepted, false);
    assert.match(r.reason, /unknown-tool/);
    assert.equal(s.store.size, 0);
});

test('ending the share clears markup so it never outlives the session', () => {
    const s = live();
    draw(s, 'alice', 'alice:1');
    s.endShare();
    assert.equal(s.store.size, 0);
    assert.equal(s.admit, ADMIT.NONE);
});
