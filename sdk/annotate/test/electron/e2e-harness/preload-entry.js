// Casual Annotate — e2e harness preload entry.
//
// `adapters/jitsi-electron/preload.js` only EXPORTS `installAnnotateBridge`/`installOverlayBridge` —
// it never calls either itself, by design, since a real host decides which one applies to which
// window. This is that call, for the host window specifically (`overlay-preload.cjs`, built
// separately from `overlay-preload.js` directly, is the other one — the overlay window has no need
// for an entry wrapper since it is not shared with anything else).

import { installAnnotateBridge } from '../../../adapters/jitsi-electron/preload.js';

installAnnotateBridge();
