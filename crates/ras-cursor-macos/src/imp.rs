//! macOS `CursorObserver` implementation. macOS-only (`cfg`-gated in `lib.rs`).

use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSCursor, NSDeviceRGBColorSpace,
    NSGraphicsContext, NSImage,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use ras_core::{CursorFrame, CursorObserver, CursorShape};

/// Upper bound on cursor edge length, mirroring the wire cap the core enforces. A cursor larger than
/// this in either dimension is skipped (reported as [`CursorFrame::Hidden`]) rather than truncated —
/// a normal OS cursor is well under this.
const MAX_CURSOR_DIM: u32 = 256;

/// How long to sleep between cursor polls. The OS cursor changes only on user action; a short poll is
/// cheap (a few objc msg-sends + one small draw only when the shape actually changed) and keeps the
/// observed shape fresh without a runloop/observer install (which would demand the main thread).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A macOS host cursor observer: polls `NSCursor` for the live application cursor and reports each
/// **change** as a [`CursorFrame`] over the [`CursorObserver`] seam. Deduped by a content hash of the
/// RGBA bytes, so an unchanged cursor yields the same `id` (the core then sends a cache reference) and
/// this observer stays quiet until the shape actually changes.
pub struct MacCursorObserver {
    /// The last `id` we emitted (content hash of the last shape's RGBA), so we only report changes.
    /// `None` before the first frame.
    last_id: Option<u32>,
}

impl MacCursorObserver {
    /// Create a cursor observer. Captures nothing until [`CursorObserver::next`] is first awaited.
    #[must_use]
    pub fn new() -> Self {
        Self { last_id: None }
    }
}

impl Default for MacCursorObserver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CursorObserver for MacCursorObserver {
    async fn next(&mut self) -> Option<CursorFrame> {
        // Poll until the cursor differs from what we last reported. The observer models a *stream of
        // changes*, so we suppress repeats here (in addition to the core's send-side dedup) and never
        // busy-spin.
        loop {
            let frame = capture_cursor_frame();
            let this_id = match &frame {
                Some(CursorFrame::Shape(s)) => Some(s.id),
                // A hidden/too-large cursor collapses to a single sentinel so Hidden→Hidden is a repeat.
                Some(CursorFrame::Hidden) => Some(0),
                None => None,
            };

            match (frame, this_id) {
                (Some(frame), Some(id)) if self.last_id != Some(id) => {
                    self.last_id = Some(id);
                    return Some(frame);
                }
                // Same as last time (or transiently unreadable): wait and poll again.
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
}

/// Read the current OS cursor once and convert it to a [`CursorFrame`]. Returns:
/// - `Some(Shape)` for a normal, in-bounds cursor,
/// - `Some(Hidden)` if the cursor image is empty / oversized / unreadable (draw nothing),
/// - `None` only if there is no cursor at all (observer end).
///
/// All `unsafe` FFI is confined to this function's helpers; no raw pointer/handle escapes.
fn capture_cursor_frame() -> Option<CursorFrame> {
    // `currentCursor` is the non-deprecated accessor and returns the app's current cursor (arrow,
    // I-beam, resize, …). It is safe to call off the main thread for a read.
    let cursor: Retained<NSCursor> = NSCursor::currentCursor();
    let image: Retained<NSImage> = cursor.image();
    let hot: NSPoint = cursor.hotSpot();

    match cursor_image_to_rgba(&image) {
        Some((width, height, rgba)) => {
            // `id` = content hash of the RGBA. Identical bitmaps hash equally, so an unchanged cursor
            // reuses its id and the core sends a `CursorCached` reference instead of the pixels.
            let id = hash_rgba(&rgba);

            // Hot-spot is in the image's coordinate space (top-left origin for a cursor). Clamp inside
            // the bitmap so `hotspot_x < width` / `hotspot_y < height` always holds (wire contract).
            let hx = clamp_hotspot(hot.x, width);
            let hy = clamp_hotspot(hot.y, height);

            Some(CursorFrame::Shape(CursorShape {
                id,
                hotspot_x: hx,
                hotspot_y: hy,
                width: width as u16,
                height: height as u16,
                rgba: Bytes::from(rgba),
            }))
        }
        // Empty / oversized / unreadable → nothing to draw.
        None => Some(CursorFrame::Hidden),
    }
}

/// Clamp a hot-spot coordinate into `0..dim` and return it as a `u16` (`dim <= MAX_CURSOR_DIM`, so it
/// always fits). A NaN/negative hot-spot pins to 0.
fn clamp_hotspot(v: f64, dim: u32) -> u16 {
    if !v.is_finite() || v < 0.0 {
        return 0;
    }
    let max = dim.saturating_sub(1);
    (v as u32).min(max) as u16
}

/// FNV-1a 32-bit hash of the RGBA bytes — a stable content id for the shape (dedup key). Not a
/// security hash; collisions only ever cause a redundant `CursorCached` reuse, never a wrong shape.
fn hash_rgba(rgba: &[u8]) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut h);
    // Fold the 64-bit hash into 32 bits (the wire `id` is a u32).
    let full = h.finish();
    ((full >> 32) as u32) ^ (full as u32)
}

/// Draw an `NSImage` cursor into a freshly-allocated, tightly-packed **top-down RGBA8** bitmap and
/// return `(width, height, rgba)` with `rgba.len() == width*height*4`.
///
/// Returns `None` if the image has no size, exceeds [`MAX_CURSOR_DIM`] in either dimension, or the
/// bitmap/context could not be created.
///
/// We render into an `NSBitmapImageRep` we own (rather than reading the image's native rep) so the
/// output format is deterministic regardless of the cursor's source representation: 8 bits/sample,
/// 4 samples/pixel, **non-premultiplied** alpha, sRGB-ish device RGB, no row padding.
fn cursor_image_to_rgba(image: &NSImage) -> Option<(u32, u32, Vec<u8>)> {
    let size: NSSize = image.size();
    let w = size.width;
    let h = size.height;
    if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 {
        return None;
    }
    // `size` is in points; NSImage cursor bitmaps are 1x, so pixels == points, rounded to whole px.
    let width = w.round() as u32;
    let height = h.round() as u32;
    if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
        return None;
    }

    let bytes_per_row = (width as usize) * 4;
    let bits_per_pixel = 32;

    // Allocate an RGBA bitmap rep with AppKit owning the backing store (null planes pointer → it
    // allocates and manages the buffer we later read via `bitmapData`).
    let rep: Retained<NSBitmapImageRep> = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(), // planes: let AppKit allocate the buffer
            width as isize,
            height as isize,
            8,    // bits per sample
            4,    // samples per pixel (RGBA)
            true, // has alpha
            false, // not planar (interleaved RGBA)
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied, // straight (non-premultiplied) alpha, top-down RGBA
            bytes_per_row as isize,
            bits_per_pixel,
        )
    }?;

    // Build a graphics context bound to that rep and make it current, then draw the cursor image into
    // it. `NSImage::drawInRect` composites over the (zero-initialised, transparent) bitmap.
    let ctx: Retained<NSGraphicsContext> =
        NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;

    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));

    let dest = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width as f64, height as f64),
    );
    // Copy (not blend) so the source alpha lands verbatim in the destination — the bitmap starts
    // fully transparent, and we want the cursor's own alpha, not a composite.
    image.drawInRect_fromRect_operation_fraction(
        dest,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)), // zero fromRect = whole image
        NSCompositingOperation::Copy,
        1.0,
    );
    ctx.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();

    // Read the packed bytes out of the rep's backing store into an owned Vec (so no objc buffer
    // escapes this function). `bitmapData` points at `height * bytes_per_row` bytes.
    let data_ptr = rep.bitmapData();
    if data_ptr.is_null() {
        return None;
    }
    let total = (height as usize) * bytes_per_row;
    let mut rgba = vec![0u8; total];
    // SAFETY: the rep owns a buffer of exactly `pixelsHigh * bytesPerRow` bytes (as configured above,
    // no padding); `data_ptr` is non-null and valid for that length for the lifetime of `rep`, which
    // outlives this copy. We only read.
    unsafe {
        std::ptr::copy_nonoverlapping(data_ptr, rgba.as_mut_ptr(), total);
    }

    Some((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotspot_clamps_into_bounds() {
        assert_eq!(clamp_hotspot(-3.0, 16), 0);
        assert_eq!(clamp_hotspot(f64::NAN, 16), 0);
        assert_eq!(clamp_hotspot(5.0, 16), 5);
        // Out of range pins to the last in-bounds pixel.
        assert_eq!(clamp_hotspot(100.0, 16), 15);
        assert_eq!(clamp_hotspot(0.0, 1), 0);
    }

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        let a = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let b = a.clone();
        let mut c = a.clone();
        c[0] ^= 0xFF;
        // Identical bytes → identical id (so a repeat becomes a cache reference).
        assert_eq!(hash_rgba(&a), hash_rgba(&b));
        // Different bytes → (almost certainly) a different id.
        assert_ne!(hash_rgba(&a), hash_rgba(&c));
    }
}
