// Casual Annotate — core unit tests. `node --test sdk/annotate/test/`
//
// These cover the three properties the design leans on hardest:
//   1. decode is FAIL-CLOSED (unknown tags rejected, never clamped),
//   2. every remove is AUTHOR-SCOPED (the multi-party correctness invariant),
//   3. the coordinate mapping is LETTERBOX-AWARE (where Jitsi's own code is wrong).

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    TOOL, OP, LIMITS, COORD_MAX, CLEAR_SCOPE,
    decode, envelope, begin, append, end, undo, erase, clear, cursor, ack, roster, strokeId,
} from '../core/ops.js';
import { StrokeStore } from '../core/store.js';
import { PaletteAssigner, PALETTE, toHex } from '../core/palette.js';
import { contentRect, normalize, denormalize, strokeHit, FIT } from '../core/geometry.js';

const ctx = { sid: 's1', seq: 1 };
const wrap = op => envelope(op, ctx);

// ── 1. fail-closed decode ───────────────────────────────────────────────────────────────────────

test('decode accepts well-formed ops', () => {
    for (const op of [
        begin('a:1', TOOL.PEN), append('a:1', 0, [[ 0, 0 ], [ 10, 10 ]]), end('a:1'),
        undo(), erase([ 'a:1' ]), clear(CLEAR_SCOPE.MINE), cursor(5, 5),
    ]) {
        assert.equal(decode(wrap(op)).ok, true, `${op.op} should decode`);
    }
});

test('decode REJECTS an unknown tool tag rather than defaulting to pen', () => {
    const r = decode(wrap({ op: OP.BEGIN, id: 'a:1', tool: 99 }));
    assert.equal(r.ok, false);
    assert.match(r.reason, /unknown-tool/);
});

test('decode rejects unknown ops, versions and scopes', () => {
    assert.equal(decode(wrap({ op: 'nope' })).ok, false);
    assert.equal(decode({ ...wrap(undo()), v: 999 }).ok, false);
    assert.equal(decode(wrap({ op: OP.CLEAR, scope: 'everything' })).ok, false);
    assert.equal(decode({ name: 'something-else', op: OP.UNDO }).ok, false);
});

test('decode rejects malformed coordinates — floats, negatives, NaN, out of range', () => {
    for (const bad of [[ 1.5, 0 ], [ -1, 0 ], [ NaN, 0 ], [ COORD_MAX + 1, 0 ], [ 0 ], [ 0, 0, 0 ]]) {
        assert.equal(decode(wrap(append('a:1', 0, [ bad ]))).ok, false, `${JSON.stringify(bad)} should fail`);
    }
});

test('decode enforces collection bounds', () => {
    const pts = Array.from({ length: LIMITS.MAX_APPEND_POINTS + 1 }, () => [ 1, 1 ]);
    assert.equal(decode(wrap(append('a:1', 0, pts))).ok, false);
    assert.equal(decode(wrap(append('a:1', 0, []))).ok, false);

    const ids = Array.from({ length: LIMITS.MAX_ERASE_IDS + 1 }, (_, i) => `a:${i}`);
    assert.equal(decode(wrap(erase(ids))).ok, false);
});

test('decode copies points — it never retains the caller\'s arrays', () => {
    const pts = [[ 1, 2 ]];
    const r = decode(wrap(append('a:1', 0, pts)));
    assert.equal(r.ok, true);
    pts[0][0] = 999;
    assert.equal(r.op.pts[0][0], 1, 'decoded points must be independent of the input');
});

test('sharer-only ops are rejected from an annotator, and vice versa', () => {
    assert.equal(decode(wrap(ack('a:1', Date.now()))).ok, false, 'ack from an annotator');
    assert.equal(decode(wrap(roster({}, {}))).ok, false, 'roster from an annotator');
    assert.equal(decode(wrap(ack('a:1', Date.now())), { fromSharer: true }).ok, true);
    assert.equal(decode(wrap(undo()), { fromSharer: true }).ok, false, 'undo from the sharer');
});

test('the envelope carries no `from` — authorship is the relay\'s to report', () => {
    assert.equal('from' in wrap(undo()), false);
});

// ── 2. author-scoped removes ────────────────────────────────────────────────────────────────────

/** Put one completed stroke from `author` into `st`. */
function stroke(st, author, id, tool = TOOL.PEN, pts = [[ 0, 0 ], [ 10, 10 ]]) {
    st.apply(author, { op: OP.BEGIN, id, tool });
    st.apply(author, { op: OP.APPEND, id, at: 0, pts });
    st.apply(author, { op: OP.END, id });
}

test('undo removes the author\'s OWN last stroke, not whatever arrived last', () => {
    const st = new StrokeStore();
    stroke(st, 'alice', 'alice:1');
    stroke(st, 'bob', 'bob:1');   // bob drew most recently

    assert.equal(st.apply('alice', { op: OP.UNDO }).changed, true);

    const left = st.strokes().map(s => s.id);
    assert.deepEqual(left, [ 'bob:1' ], "alice's undo must not touch bob's stroke");
});

test('erase silently ignores ids belonging to another author', () => {
    const st = new StrokeStore();
    stroke(st, 'alice', 'alice:1');
    stroke(st, 'bob', 'bob:1');

    // alice asks to erase bob's stroke by id — the store re-filters by author.
    const r = st.apply('alice', { op: OP.ERASE, ids: [ 'bob:1' ] });
    assert.equal(r.changed, false);
    assert.equal(st.size, 2, "bob's stroke must survive alice's erase");
});

test('clear "mine" is scoped; clear "all" is not', () => {
    const st = new StrokeStore();
    stroke(st, 'alice', 'alice:1');
    stroke(st, 'alice', 'alice:2');
    stroke(st, 'bob', 'bob:1');

    st.apply('alice', { op: OP.CLEAR, scope: CLEAR_SCOPE.MINE });
    assert.deepEqual(st.strokes().map(s => s.id), [ 'bob:1' ]);

    st.apply('alice', { op: OP.CLEAR, scope: CLEAR_SCOPE.ALL });
    assert.equal(st.size, 0);
});

test('an author cannot append to or end another author\'s stroke', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN });

    assert.equal(st.apply('bob', { op: OP.APPEND, id: 'alice:1', at: 0, pts: [[ 1, 1 ]] }).reason, 'not-your-stroke');
    assert.equal(st.apply('bob', { op: OP.END, id: 'alice:1' }).reason, 'not-your-stroke');
    assert.equal(StrokeStore.densePoints(st.strokes()[0]).length, 0);
});

test('a duplicate begin does not wipe points already accumulated (the §5 stroke repair)', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN });
    st.apply('alice', { op: OP.APPEND, id: 'alice:1', at: 0, pts: [[ 1, 1 ], [ 2, 2 ]] });
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN }); // replayed

    assert.equal(StrokeStore.densePoints(st.strokes()[0]).length, 2, 'a replayed begin must not reset the stroke');
});

test('end yields the ack that frees the annotator\'s local echo', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN });
    assert.equal(st.apply('alice', { op: OP.END, id: 'alice:1' }).acked, 'alice:1');
});

test('stroke points are bounded, and the stroke survives the cap', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN });
    for (let i = 0; i < 40; i++) {
        st.apply('alice', { op: OP.APPEND, id: 'alice:1', at: i * 64, pts: Array.from({ length: 64 }, () => [ 1, 1 ]) });
    }
    assert.equal(StrokeStore.densePoints(st.strokes()[0]).length, LIMITS.MAX_STROKE_POINTS);
});

test('retained strokes are bounded, oldest evicted first', () => {
    const st = new StrokeStore();
    for (let i = 0; i < LIMITS.MAX_STROKES + 10; i++) stroke(st, 'alice', `alice:${i}`);
    assert.equal(st.size, LIMITS.MAX_STROKES);
    assert.equal(st.strokes()[0].id, 'alice:10', 'the oldest should have been evicted');
});

test('open strokes per author are bounded', () => {
    const st = new StrokeStore();
    for (let i = 0; i < LIMITS.MAX_OPEN_STROKES_PER_AUTHOR; i++) {
        assert.equal(st.apply('alice', { op: OP.BEGIN, id: `alice:${i}`, tool: TOOL.PEN }).changed, true);
    }
    const over = st.apply('alice', { op: OP.BEGIN, id: 'alice:over', tool: TOOL.PEN });
    assert.equal(over.reason, 'too-many-open-strokes');
    // ...and ending one frees a slot again.
    st.apply('alice', { op: OP.END, id: 'alice:0' });
    assert.equal(st.apply('alice', { op: OP.BEGIN, id: 'alice:new', tool: TOOL.PEN }).changed, true);
});

// ── 3. palette ──────────────────────────────────────────────────────────────────────────────────

test('colours are distinct per author and stable across repeat calls', () => {
    const p = new PaletteAssigner();
    const a = p.colorFor('alice');
    const b = p.colorFor('bob');
    assert.notEqual(a, b);
    assert.equal(p.colorFor('alice'), a, 'assignment must be stable');
});

test('a released slot is handed to the next joiner', () => {
    const p = new PaletteAssigner();
    const a = p.colorFor('alice');
    p.colorFor('bob');
    p.release('alice');
    assert.equal(p.colorFor('carol'), a, "carol should inherit alice's freed slot");
});

test('palette exhaustion degrades visually but never breaks assignment', () => {
    const p = new PaletteAssigner();
    for (let i = 0; i < PALETTE.length; i++) p.colorFor(`u${i}`);
    assert.equal(p.exhausted, true);
    // Past the end, everyone still gets a valid colour — removes stay exact because they key on
    // author id, not colour.
    const extra = p.colorFor('overflow');
    assert.ok(PALETTE.includes(extra));
});

test('toHex formats for canvas', () => {
    assert.equal(toHex(0xFF3B30), '#ff3b30');
    assert.equal(toHex(0x000000), '#000000');
});

// ── 4. letterbox-aware geometry ─────────────────────────────────────────────────────────────────

test('contentRect letterboxes a 16:9 element showing a 4:3 source', () => {
    const el = { left: 0, top: 0, width: 1600, height: 900 };
    const r = contentRect(el, 1024, 768, FIT.CONTAIN);
    assert.equal(r.ok, true);
    // scale = min(1600/1024, 900/768) = min(1.5625, 1.171875) = 1.171875 → 1200 x 900
    assert.equal(Math.round(r.rect.width), 1200);
    assert.equal(Math.round(r.rect.height), 900);
    assert.equal(Math.round(r.rect.left), 200, 'pillar-boxed, so centred with 200px bars');
});

test('normalizing against the element instead of the content rect is what Jitsi gets wrong', () => {
    const el = { left: 0, top: 0, width: 1600, height: 900 };
    const r = contentRect(el, 1024, 768, FIT.CONTAIN);

    // A click at the picture's left edge (x = 200, the start of the content rect).
    const correct = normalize(200, 450, r.rect);
    assert.equal(correct[0], 0, 'the picture\'s left edge must map to 0');

    // The same click normalized against the ELEMENT (actions.ts:637) lands 12.5% in — a real,
    // visible offset on someone's desktop.
    const naive = normalize(200, 450, el);
    assert.equal(Math.round((naive[0] / COORD_MAX) * 100), 13);
});

test('object-fit: cover is refused, not approximated', () => {
    const r = contentRect({ left: 0, top: 0, width: 100, height: 100 }, 200, 100, FIT.COVER);
    assert.equal(r.ok, false);
    assert.match(r.reason, /cover/);
});

test('an unsized source or element is refused', () => {
    assert.equal(contentRect({ left: 0, top: 0, width: 100, height: 100 }, 0, 0).ok, false);
    assert.equal(contentRect({ left: 0, top: 0, width: 0, height: 0 }, 100, 100).ok, false);
});

test('normalize and denormalize round-trip', () => {
    const rect = { left: 10, top: 20, width: 800, height: 600 };
    const [ nx, ny ] = normalize(410, 320, rect);
    const [ x, y ] = denormalize(nx, ny, rect);
    assert.ok(Math.abs(x - 410) < 0.1);
    assert.ok(Math.abs(y - 320) < 0.1);
});

test('normalize clamps a drag that leaves the video', () => {
    const rect = { left: 0, top: 0, width: 100, height: 100 };
    assert.deepEqual(normalize(-50, -50, rect), [ 0, 0 ]);
    assert.deepEqual(normalize(150, 150, rect), [ COORD_MAX, COORD_MAX ]);
});

// ── 5. eraser hit-testing ───────────────────────────────────────────────────────────────────────

test('the eraser hits a polyline near it and misses one far away', () => {
    const pts = [[ 0, 0 ], [ 100, 0 ], [ 100, 100 ]];
    assert.equal(strokeHit(pts, TOOL.PEN, 50, 5, 10), true);
    assert.equal(strokeHit(pts, TOOL.PEN, 50, 50, 10), false);
});

test('a rect hit-tests on its outline, not its interior', () => {
    const pts = [[ 0, 0 ], [ 100, 100 ]];
    assert.equal(strokeHit(pts, TOOL.RECT, 50, 2, 5), true, 'on the top edge');
    assert.equal(strokeHit(pts, TOOL.RECT, 50, 50, 5), false, 'the empty middle must not be a hit');
});

test('an arrow hit-tests on the segment, not on its stored point list', () => {
    const pts = [[ 0, 0 ], [ 100, 100 ]];
    assert.equal(strokeHit(pts, TOOL.ARROW, 50, 50, 5), true);
    assert.equal(strokeHit(pts, TOOL.ARROW, 0, 100, 5), false);
});

test('a single-point stroke is hit-testable', () => {
    assert.equal(strokeHit([[ 50, 50 ]], TOOL.PEN, 52, 52, 5), true);
    assert.equal(strokeHit([], TOOL.PEN, 0, 0, 5), false);
});

// ── 6. indexed appends: idempotent and order-independent ────────────────────────────────────────

test('an append never overwrites a point that is already there (idempotent replay)', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'alice:1', tool: TOOL.PEN });
    st.apply('alice', { op: OP.APPEND, id: 'alice:1', at: 0, pts: [[ 1, 1 ], [ 2, 2 ]] });

    // The same message again, and a hostile attempt to rewrite slot 0 with different data.
    st.apply('alice', { op: OP.APPEND, id: 'alice:1', at: 0, pts: [[ 1, 1 ], [ 2, 2 ]] });
    st.apply('alice', { op: OP.APPEND, id: 'alice:1', at: 0, pts: [[ 9, 9 ]] });

    assert.deepEqual(StrokeStore.densePoints(st.strokes()[0]), [[ 1, 1 ], [ 2, 2 ]]);
});

test('appends may arrive in any order — the relay promises none', () => {
    const forward = new StrokeStore();
    const shuffled = new StrokeStore();
    const batches = [
        { at: 0, pts: [[ 0, 0 ], [ 1, 1 ]] },
        { at: 2, pts: [[ 2, 2 ], [ 3, 3 ]] },
        { at: 4, pts: [[ 4, 4 ], [ 5, 5 ]] },
    ];
    for (const st of [ forward, shuffled ]) st.apply('alice', { op: OP.BEGIN, id: 'a:1', tool: TOOL.PEN });
    for (const b of batches) forward.apply('alice', { op: OP.APPEND, id: 'a:1', ...b });
    for (const b of [ batches[2], batches[0], batches[1] ]) shuffled.apply('alice', { op: OP.APPEND, id: 'a:1', ...b });

    assert.deepEqual(
        StrokeStore.densePoints(shuffled.strokes()[0]),
        StrokeStore.densePoints(forward.strokes()[0]),
        'order of arrival must not change the result',
    );
});

test('a hole from a dropped append is skipped, not rendered as a gap', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'a:1', tool: TOOL.PEN });
    st.apply('alice', { op: OP.APPEND, id: 'a:1', at: 0, pts: [[ 0, 0 ]] });
    st.apply('alice', { op: OP.APPEND, id: 'a:1', at: 4, pts: [[ 4, 4 ]] }); // slots 1..3 lost

    assert.deepEqual(StrokeStore.densePoints(st.strokes()[0]), [[ 0, 0 ], [ 4, 4 ]]);
});

test('a repair can fill a hole after `end`, but cannot rewrite what landed', () => {
    const st = new StrokeStore();
    st.apply('alice', { op: OP.BEGIN, id: 'a:1', tool: TOOL.PEN });
    st.apply('alice', { op: OP.APPEND, id: 'a:1', at: 0, pts: [[ 0, 0 ]] });
    st.apply('alice', { op: OP.END, id: 'a:1' });

    assert.equal(st.apply('alice', { op: OP.APPEND, id: 'a:1', at: 1, pts: [[ 1, 1 ]] }).changed, true);
    assert.equal(st.apply('alice', { op: OP.APPEND, id: 'a:1', at: 0, pts: [[ 7, 7 ]] }).changed, false);
    assert.deepEqual(StrokeStore.densePoints(st.strokes()[0]), [[ 0, 0 ], [ 1, 1 ]]);
});

test('decode rejects a bad append index', () => {
    for (const at of [ -1, 1.5, NaN, LIMITS.MAX_STROKE_POINTS, '0' ]) {
        assert.equal(decode(wrap({ op: OP.APPEND, id: 'a:1', at, pts: [[ 1, 1 ]] })).ok, false, `at=${at}`);
    }
});
