// Casual Annotate — latency harness tests (the pure parts).
// The beacon's pixel round-trip needs a DOM and is on-device work; the estimator and the verdict
// logic are pure, and the verdict is what actually drives the §17.4 decision.

import test from 'node:test';
import assert from 'node:assert/strict';

import { AckTimer, LatencyProbe } from '../latency/beacon.js';

test('AckTimer measures a round trip and ignores an unknown ack', () => {
    const t = new AckTimer();
    t.sent('a:1', 1000);
    assert.equal(t.acked('a:1', 1250), 250);
    assert.equal(t.acked('never-sent', 1300), null);
});

test('AckTimer uses the median, so one stall does not move the estimate', () => {
    const t = new AckTimer();
    const rtts = [ 100, 110, 105, 5000, 95 ];
    rtts.forEach((rtt, i) => {
        t.sent(`s${i}`, 0);
        t.acked(`s${i}`, rtt);
    });
    assert.equal(t.median, 105, 'the 5000ms outlier must not dominate');
});

test('AckTimer bounds its sample window', () => {
    const t = new AckTimer({ window: 3 });
    for (let i = 0; i < 10; i++) {
        t.sent(`s${i}`, 0);
        t.acked(`s${i}`, 100);
    }
    assert.equal(t.samples, 3);
});

test('the probe recovers a leg latency from the beacon counter', () => {
    const p = new LatencyProbe();
    const g = { fillRect() {}, set fillStyle(_) {} };
    const tick = p.stamp(g, 1, 1000);
    assert.equal(p.observe(tick, 1420), 420);
});

test('an unseen counter yields no sample rather than a wrong one', () => {
    const p = new LatencyProbe();
    assert.equal(p.observe(999, 1000), null);
});

test('the verdict names capture/encode and points at the frame rate', () => {
    const v = LatencyProbe.verdict({ t1: 40, t2: 600, t3: 150 });
    assert.equal(v.dominant, 't2');
    assert.match(v.advice, /desktopSharingFrameRate/);
});

test('the verdict names the receive path and points at playoutDelayHint', () => {
    const v = LatencyProbe.verdict({ t1: 40, t2: 120, t3: 700 });
    assert.equal(v.dominant, 't3');
    assert.match(v.advice, /playoutDelayHint/);
});

test('the verdict names the relay and explicitly rules out a media server', () => {
    const v = LatencyProbe.verdict({ t1: 800, t2: 90, t3: 100 });
    assert.equal(v.dominant, 't1');
    assert.match(v.advice, /would not help/);
});

test('the verdict reports a share of the total, so "dominant" is quantified', () => {
    const v = LatencyProbe.verdict({ t1: 100, t2: 100, t3: 800 });
    assert.equal(v.total, 1000);
    assert.equal(Math.round(v.shareOfTotal * 100), 80);
});

test('the verdict copes with missing terms instead of inventing them', () => {
    assert.equal(LatencyProbe.verdict({}).dominant, null);
    assert.equal(LatencyProbe.verdict({ t2: 300 }).dominant, 't2');
});
