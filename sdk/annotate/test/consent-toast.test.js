// Casual Annotate — the one piece of `surface/consent-toast.js` with no DOM in it.
//
// The toast itself (timeout, focus, ARIA) needs a real DOM and is exercised live instead
// (`test/live/e2e.spec.mjs`), matching this repo's existing line between DOM-free `core`/pure-logic
// modules (unit tested here) and `surface`/`overlay` modules (exercised in a real browser/Electron).

import test from 'node:test';
import assert from 'node:assert/strict';

import { initials } from '../surface/consent-toast.js';

test('initials: first letters of up to two words, uppercased', () => {
    assert.equal(initials('Ada Lovelace'), 'AL');
    assert.equal(initials('cher'), 'C');
    assert.equal(initials('Grace Beatrice Hopper'), 'GB'); // caps at two — a badge, not a name tag
});

test('initials: never throws on absent input, and stays bounded on a long one', () => {
    assert.equal(initials(''), '?');
    assert.equal(initials(undefined), '?');
    assert.equal(initials('   '), '?');
    // The name is someone else's roster display name — arbitrary length. `initials` only ever needs
    // to feed `avatar.textContent` (never `innerHTML` — `consent-toast.js` sets it that way), so the
    // safety property that actually matters is bounded output, not escaping; this pins that.
    assert.ok(initials('a very long display name indeed'.repeat(10)).length <= 2);
});
