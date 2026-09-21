// Casual Annotate — browser test bundle.
//
// Bundled to an IIFE and injected into a live Jitsi page, this exposes the real SDK as a global so
// two browser participants can be driven against a real server. Used by the live proof in
// `test/live/README.md`.
//
// This is test scaffolding, not a shipping entry point: production consumers import the package.

import { ConferenceTransport, StrokeSender, CursorSender } from '../../transport/jitsi.js';
import { SharerController } from '../../sharer.js';
import { AnnotatorController } from '../../annotator.js';
import { StrokeStore } from '../../core/store.js';
import { ADMIT } from '../../core/session.js';
import * as ops from '../../core/ops.js';
import * as compat from '../../core/compat.js';

window.CasualAnnotate = {
    ConferenceTransport, StrokeSender, CursorSender,
    SharerController, AnnotatorController, StrokeStore, ADMIT, ops, compat,
};
