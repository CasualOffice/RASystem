// Casual Annotate — latency decomposition (ADR-107 §11, §17.1).
//
// The observed "~1 s draw-to-see" is three different numbers, and each points at a different fix.
// Guessing which one dominates is how teams end up building a media server to solve a config
// problem, so this module measures them separately:
//
//   t1  draw → the op reaches the sharer                  — from the ack. Data RTT.
//   t2  sharer renders → the mark is in an outgoing frame — pixel beacon on the LOCAL preview.
//   t3  outgoing frame → a viewer displays it             — pixel beacon at the viewer, minus t2.
//
// Decision rule (§17.4):
//   t2 dominates → capture/encode. Raise `desktopSharingFrameRate.max` above 5 first: that single
//                  comparison also flips `contentHint` to 'motion' and re-enables the simulcast
//                  layers (`ScreenObtainer.ts:422`, `TraceablePeerConnection.ts:2281`).
//   t3 dominates → JVB relay + jitter buffer + decode. Try `receiver.playoutDelayHint = 0` — it is
//                  never set anywhere in lib-jitsi-meet. Only then consider a media path change.
//   t1 dominates → the relay is the problem, not the video. That is a transport question.

/**
 * Rolling estimate of the data round trip, from `end` → `ack`.
 *
 * This is t1, and it is also the input the drawing surface needs: the echo hold-off starts from a
 * real ack rather than a guessed timer (§11.2).
 */
export class AckTimer {
    constructor({ window = 20 } = {}) {
        this._sent = new Map();
        this._samples = [];
        this._window = window;
    }

    /** Call when a stroke's `end` goes out. */
    sent(id, now = performance.now()) {
        this._sent.set(id, now);
    }

    /** Call when the sharer's `ack` arrives. @returns {number|null} this sample's RTT in ms. */
    acked(id, now = performance.now()) {
        const t = this._sent.get(id);
        if (t === undefined) return null;
        this._sent.delete(id);
        const rtt = now - t;
        this._samples.push(rtt);
        if (this._samples.length > this._window) this._samples.shift();
        return rtt;
    }

    /** Median, not mean — one stalled sample should not move the estimate. */
    get median() {
        if (!this._samples.length) return null;
        const s = [ ...this._samples ].sort((a, b) => a - b);
        return s[Math.floor(s.length / 2)];
    }

    get samples() {
        return this._samples.length;
    }
}

/**
 * Encode a frame counter as a row of black/white pixel blocks.
 *
 * Drawn by the sharer's overlay in a corner, then read back from the video by anyone receiving it.
 * Deliberately black-and-white and several pixels per bit, so it survives chroma subsampling,
 * scaling and a lossy encoder — the things that would destroy a subtler marker.
 */
export const BEACON = Object.freeze({
    BITS: 12,          // 4096 ticks before wrap — minutes at any sane frame rate
    BLOCK: 8,          // px per bit, pre-DPR
    HEIGHT: 8,
    MARGIN: 0,         // top-left corner: never cropped by a letterbox
});

/** Paint `value` as BEACON.BITS blocks. Call once per rendered overlay frame. */
export function drawBeacon(g, value, dpr = 1) {
    const b = BEACON.BLOCK * dpr;
    const h = BEACON.HEIGHT * dpr;
    const v = value & ((1 << BEACON.BITS) - 1);
    // A leading white block anchors the read and proves the beacon is present at all.
    g.fillStyle = '#fff';
    g.fillRect(BEACON.MARGIN, BEACON.MARGIN, b, h);
    for (let i = 0; i < BEACON.BITS; i++) {
        g.fillStyle = (v >> i) & 1 ? '#fff' : '#000';
        g.fillRect(BEACON.MARGIN + (i + 1) * b, BEACON.MARGIN, b, h);
    }
}

/**
 * Read the beacon out of a video element.
 *
 * @param {HTMLVideoElement} video
 * @param {{ left:number, top:number, width:number, height:number }} contentRect - where the picture
 *   actually is (letterbox bars would otherwise be sampled instead of the beacon).
 * @returns {number|null} the counter, or null if no beacon is legible.
 */
export function readBeacon(video, contentRect) {
    const w = video.videoWidth;
    const h = video.videoHeight;
    if (!w || !h) return null;

    // Sample in SOURCE pixels: the beacon was drawn in the sharer's display space, and the video
    // carries that space regardless of how the element is scaled on this screen.
    const scale = w / contentRect.width;
    const blocks = BEACON.BITS + 1;
    const px = Math.ceil(BEACON.BLOCK * scale);
    const sw = Math.ceil(blocks * px);
    const sh = Math.ceil(BEACON.HEIGHT * scale);

    const c = document.createElement('canvas');
    c.width = sw;
    c.height = sh;
    const g = c.getContext('2d', { willReadFrequently: true });
    try {
        g.drawImage(video, 0, 0, sw, sh, 0, 0, sw, sh);
    } catch {
        return null; // tainted canvas — cross-origin video
    }
    const data = g.getImageData(0, 0, sw, sh).data;

    /** Mean luminance at the centre of block `i`. */
    const lumaAt = (i) => {
        const x = Math.floor((i + 0.5) * px);
        const y = Math.floor(sh / 2);
        const o = (y * sw + x) * 4;
        return (data[o] + data[o + 1] + data[o + 2]) / 3;
    };

    // The anchor must be bright, or we are not looking at a beacon at all.
    if (lumaAt(0) < 140) return null;

    let v = 0;
    for (let i = 0; i < BEACON.BITS; i++) if (lumaAt(i + 1) > 128) v |= (1 << i);
    return v;
}

/**
 * Ties the pieces together: the sharer stamps each overlay frame with a counter and remembers when
 * it drew it; a reader reports the counter it currently sees; the difference is that leg's latency.
 *
 * Run the same harness on the sharer's own LOCAL PREVIEW track to get t2, and on a viewer to get
 * t2 + t3 — subtract to isolate t3.
 */
export class LatencyProbe {
    constructor() {
        this._tick = 0;
        this._drawnAt = new Map();  // counter → performance.now()
        this._samples = [];
    }

    /** Sharer: advance the counter each rendered frame and draw it. */
    stamp(g, dpr = 1, now = performance.now()) {
        this._tick = (this._tick + 1) & ((1 << BEACON.BITS) - 1);
        this._drawnAt.set(this._tick, now);
        // Keep the map bounded to one wrap.
        if (this._drawnAt.size > (1 << BEACON.BITS)) {
            this._drawnAt.delete(this._drawnAt.keys().next().value);
        }
        drawBeacon(g, this._tick, dpr);
        return this._tick;
    }

    /**
     * Reader: report the counter currently visible. Only meaningful on the SHARER's machine, where
     * `_drawnAt` exists — a remote viewer must instead exchange the draw timestamps out of band,
     * or compare against the sharer-reported t2.
     * @returns {number|null} latency in ms for this leg.
     */
    observe(counter, now = performance.now()) {
        const drawn = this._drawnAt.get(counter);
        if (drawn === undefined) return null;
        const ms = now - drawn;
        this._samples.push(ms);
        if (this._samples.length > 60) this._samples.shift();
        return ms;
    }

    get median() {
        if (!this._samples.length) return null;
        const s = [ ...this._samples ].sort((a, b) => a - b);
        return s[Math.floor(s.length / 2)];
    }

    /**
     * The A2.5 report. Feed it the three medians and it names the dominant term and the next move,
     * so the decision is made by the measurement rather than by whoever argues hardest.
     */
    static verdict({ t1, t2, t3 }) {
        const terms = [ [ 't1', t1 ], [ 't2', t2 ], [ 't3', t3 ] ].filter(([ , v ]) => Number.isFinite(v));
        if (!terms.length) return { dominant: null, advice: 'no samples yet' };
        terms.sort((a, b) => b[1] - a[1]);
        const [ name, value ] = terms[0];
        const total = terms.reduce((n, [ , v ]) => n + v, 0);
        const advice = {
            t1: 'The relay dominates, not the video. This is a transport question — a media server would not help.',
            t2: 'Capture/encode dominates. Raise desktopSharingFrameRate.max above 5 (it also flips contentHint to motion) and re-measure.',
            t3: 'Relay + jitter buffer + decode dominate. Try receiver.playoutDelayHint = 0 before considering any media path change.',
        }[name];
        return { dominant: name, ms: value, shareOfTotal: value / total, total, advice };
    }
}
