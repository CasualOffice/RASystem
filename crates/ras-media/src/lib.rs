//! Casual RAS media pipeline (skeleton).
//!
//! Home for the capture / encode / decode / render backend traits and the frame-pacing pipeline.
//! Populated in Phase 1 — see `docs/10`. DXGI capture + HW H.264 (B-frames off, Annex-B) with an
//! OpenH264 software fallback (never x264).
