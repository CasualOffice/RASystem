//! macOS host-side **cursor observer** (ADR-073) behind [`ras_core::CursorObserver`], reporting
//! **both** the cursor shape *and* its position. macOS-only (`cfg`-gated in `lib.rs`).
//!
//! This observer tracks both the cursor shape *and* the pointer's **position** so the controller can
//! render a zero-latency remote cursor that both looks right (shape) and lands right (position):
//!
//! - **Shape** comes from `NSCursor::currentCursor()` → `NSImage` → `CGImageForProposedRect` →
//!   drawn into a freshly-allocated `CGBitmapContext` (premultiplied RGBA), read out as tightly-packed
//!   top-down RGBA bytes; the hot-spot is `NSCursor::hotSpot()`. Deduped by a stable content id
//!   (hash of the RGBA + dims + hot-spot), so an unchanged cursor reuses its id (the core then sends a
//!   `CursorCached` reference) and the observer only reports the shape when it actually changes.
//! - **Position** comes from `NSEvent::mouseLocation()` — global points in AppKit's **bottom-left**
//!   origin — flipped to a **top-left** origin (via the primary display height) to match the
//!   desktop-union / capture-geometry coordinate space the input backend uses, then normalized to
//!   `0..=65535` over the **captured display bounds** supplied at construction (or updated live via
//!   [`MacCursorObserver::set_display_bounds`]).
//!
//! Both are polled on a short interval; each [`CursorObserver::next`] call returns the *first* change
//! it sees (a `Shape` when the shape changed, else a `Moved` when only the position changed), and the
//! core's cursor pump throttles the high-frequency `Moved` stream so it never floods the control
//! channel. An empty / oversized / unreadable cursor image is reported as [`CursorFrame::Hidden`].
//!
//! FFI is confined to this module (the crate already relaxes `unsafe_code = allow`, CONTRIBUTING §5);
//! no raw pointer/handle escapes the safe [`CursorObserver`] surface. Cursor pixels never touch a log
//! (`CursorShape::Debug` elides the RGBA in `ras-core`, and this module logs nothing).

use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use objc2::rc::Retained;
use objc2_app_kit::{NSCursor, NSEvent, NSImage};
use objc2_core_foundation::{CFRetained, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGMainDisplayID};
use objc2_foundation::NSPoint;

use ras_core::{CursorFrame, CursorObserver, CursorShape};

/// Upper bound on cursor edge length, mirroring the wire cap the core enforces
/// ([`ras_protocol::MAX_CURSOR_DIM`], re-declared here to avoid a proto dep). A cursor larger than
/// this in either dimension is skipped (reported as [`CursorFrame::Hidden`]) rather than truncated —
/// a normal OS cursor is well under this.
const MAX_CURSOR_DIM: u32 = 256;

/// The full normalized position range (`0..=NORM_MAX`) the wire uses for a pointer coordinate.
const NORM_MAX: f64 = 65535.0;

/// How long to sleep between polls. The cursor's shape changes only on user action and its position
/// only while the mouse moves; a short poll is cheap (a few objc msg-sends + one small draw only when
/// the shape actually changed) and keeps both fresh without installing a run-loop event tap (which
/// would demand the main thread). ~50 ms ≈ 20 Hz; the core throttles the position stream to ~60 Hz,
/// so this is the binding rate.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The captured display's global bounds in **top-left-origin points** (the same space as
/// `SCDisplay.frame` / the `CaptureGeometry` lifecycle event / the input backend's registered
/// display bounds). Position is normalized over this rectangle so the controller's rendered cursor
/// lands on the shared display — correct on a secondary monitor, not just the primary.
#[derive(Clone, Copy, Debug)]
pub struct DisplayBounds {
    /// Left edge (points, top-left origin; may be negative for a monitor left of the primary).
    pub x: f64,
    /// Top edge (points, top-left origin; may be negative for a monitor above the primary).
    pub y: f64,
    /// Width in points (`> 0`).
    pub width: f64,
    /// Height in points (`> 0`).
    pub height: f64,
}

/// A macOS host cursor observer reporting **both** shape and position over the [`CursorObserver`]
/// seam. Construct it with the captured display's bounds so position normalizes correctly; the app
/// can refresh the bounds on a `CaptureGeometry` change via [`MacCursorObserver::set_display_bounds`].
pub struct MacCursorObserver {
    /// The captured display's bounds (top-left points) that position is normalized over. `None` until
    /// the first `set_display_bounds`; while `None` a move is reported over the **primary** display as
    /// a fail-safe (so the observer is usable before capture geometry is known).
    bounds: Option<DisplayBounds>,
    /// The last shape id we emitted (content hash), so shape changes are reported once. `None` before
    /// the first shape.
    last_shape_id: Option<u32>,
    /// The last normalized position we emitted, so a move is reported only when it actually changes.
    /// `None` before the first move.
    last_pos: Option<(u16, u16)>,
}

impl MacCursorObserver {
    /// Create a cursor observer over the given captured display bounds. Captures nothing until
    /// [`CursorObserver::next`] is first awaited.
    #[must_use]
    pub fn new(bounds: DisplayBounds) -> Self {
        Self {
            bounds: Some(bounds),
            last_shape_id: None,
            last_pos: None,
        }
    }

    /// Create a cursor observer with **no** display bounds yet; position normalizes over the primary
    /// display until [`MacCursorObserver::set_display_bounds`] supplies the captured display's bounds
    /// (e.g. from the host's `CaptureGeometry`).
    #[must_use]
    pub fn without_bounds() -> Self {
        Self {
            bounds: None,
            last_shape_id: None,
            last_pos: None,
        }
    }

    /// Update the captured display bounds position is normalized over (top-left points), e.g. when the
    /// host emits a new `CaptureGeometry` (a different shared monitor). Takes effect on the next poll.
    pub fn set_display_bounds(&mut self, bounds: DisplayBounds) {
        self.bounds = Some(bounds);
    }
}

impl Default for MacCursorObserver {
    fn default() -> Self {
        Self::without_bounds()
    }
}

#[async_trait]
impl CursorObserver for MacCursorObserver {
    async fn next(&mut self) -> Option<CursorFrame> {
        // Poll until *something* changes. Shape changes take priority over moves in a single poll (a
        // shape change is rarer and more visually significant); a pure move is reported when the shape
        // is unchanged. The observer models a stream of *changes*, so we suppress repeats here (in
        // addition to the core's send-side dedup) and never busy-spin.
        loop {
            let shape_frame = capture_shape_frame();
            let pos = self.capture_position();

            // 1) Shape changed (or first observation): report it.
            let this_shape_id = match &shape_frame {
                CursorFrame::Shape(s) => s.id,
                // A hidden/unreadable cursor collapses to a single sentinel so Hidden→Hidden is a repeat.
                _ => 0,
            };
            if self.last_shape_id != Some(this_shape_id) {
                self.last_shape_id = Some(this_shape_id);
                return Some(shape_frame);
            }

            // 2) Shape unchanged — report a position change if there is one.
            if let Some((nx, ny)) = pos {
                if self.last_pos != Some((nx, ny)) {
                    self.last_pos = Some((nx, ny));
                    return Some(CursorFrame::Moved { x: nx, y: ny });
                }
            }

            // 3) Nothing changed: wait and poll again.
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl MacCursorObserver {
    /// Read the live pointer position and normalize it to `0..=65535` over the observer's display
    /// bounds. `None` if the bounds are degenerate (zero/negative extent) — the caller then reports no
    /// move rather than a divide-by-zero or an off-screen coordinate.
    fn capture_position(&self) -> Option<(u16, u16)> {
        // `mouseLocation` is global points in AppKit's **bottom-left** origin. It is safe to call off
        // the main thread for a read.
        let loc: NSPoint = NSEvent::mouseLocation();

        // Flip Y to a **top-left** origin to match `SCDisplay.frame` / `CaptureGeometry` / the input
        // backend's desktop-union coords. AppKit's global origin is the bottom-left of the *primary*
        // display, so the flip reference is the primary display's height (top-left CG space).
        let primary_h = primary_display_height();
        let top_left = CGPoint {
            x: loc.x,
            y: primary_h - loc.y,
        };

        // Normalize over the captured display bounds (top-left points). Without bounds yet, fall back
        // to the primary display so the observer is usable before capture geometry is known.
        let b = self.bounds.unwrap_or_else(primary_display_bounds);
        // Reject degenerate/non-finite extents (a `<= 0.0` test is false for NaN, so spell it out as
        // "is a positive, finite extent" to also exclude NaN — never divide by a bad width/height).
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !positive(b.width) || !positive(b.height) {
            return None;
        }

        let fx = ((top_left.x - b.x) / b.width).clamp(0.0, 1.0);
        let fy = ((top_left.y - b.y) / b.height).clamp(0.0, 1.0);
        // A non-finite fraction (NaN survives `clamp`) pins to the origin — never off-screen.
        let nx = norm(fx);
        let ny = norm(fy);
        Some((nx, ny))
    }
}

/// Map a `0.0..=1.0` fraction to the `0..=65535` normalized wire range, saturating. A non-finite
/// input pins to 0 (the display origin).
fn norm(frac: f64) -> u16 {
    if !frac.is_finite() {
        return 0;
    }
    (frac * NORM_MAX).round().clamp(0.0, NORM_MAX) as u16
}

/// The primary display's height in points (top-left CG space) — the Y-flip reference for AppKit's
/// bottom-left global coordinates. Falls back to a sane default if the display is unreadable.
fn primary_display_height() -> CGFloat {
    let b = primary_display_bounds();
    if b.height > 0.0 {
        b.height
    } else {
        // Extremely defensive: a zero height would break the flip. A non-zero default keeps the flip
        // finite; a wrong-but-finite value only mis-normalizes until real bounds arrive.
        1.0
    }
}

/// The primary display's bounds in top-left-origin points, via CoreGraphics (off-main-thread safe,
/// unlike `NSScreen` which needs a `MainThreadMarker`).
fn primary_display_bounds() -> DisplayBounds {
    let rect: CGRect = objc2_core_graphics::CGDisplayBounds(CGMainDisplayID());
    DisplayBounds {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

/// Read the current OS cursor shape once and convert it to a [`CursorFrame`]:
/// - `Shape` for a normal, in-bounds cursor,
/// - `Hidden` if the cursor image is empty / oversized / unreadable (draw nothing).
///
/// All FFI is confined here; no raw pointer/handle escapes.
fn capture_shape_frame() -> CursorFrame {
    // `currentCursor` is the non-deprecated accessor and returns the app's current cursor (arrow,
    // I-beam, resize, …). Safe to call off the main thread for a read.
    let cursor: Retained<NSCursor> = NSCursor::currentCursor();
    let image: Retained<NSImage> = cursor.image();
    let hot: NSPoint = cursor.hotSpot();

    match cursor_image_to_rgba(&image) {
        Some((width, height, rgba)) => {
            // `id` = content hash of the pixels + dims + hot-spot. Identical shapes hash equally, so an
            // unchanged cursor reuses its id and the core sends a `CursorCached` reference.
            let hx = clamp_hotspot(hot.x, width);
            let hy = clamp_hotspot(hot.y, height);
            let id = shape_id(&rgba, width, height, hx, hy);
            CursorFrame::Shape(CursorShape {
                id,
                hotspot_x: hx,
                hotspot_y: hy,
                width: width as u16,
                height: height as u16,
                rgba: Bytes::from(rgba),
            })
        }
        // Empty / oversized / unreadable → nothing to draw.
        None => CursorFrame::Hidden,
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

/// A stable 32-bit content id for a shape: a hash of its RGBA bytes **and** dims **and** hot-spot, so
/// two visually distinct cursors that happen to share a byte pattern (different size/hot-spot) get
/// different ids. Not a security hash; a collision only ever causes a redundant `CursorCached` reuse,
/// never a wrong shape.
fn shape_id(rgba: &[u8], width: u32, height: u32, hotspot_x: u16, hotspot_y: u16) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut h);
    width.hash(&mut h);
    height.hash(&mut h);
    hotspot_x.hash(&mut h);
    hotspot_y.hash(&mut h);
    let full = h.finish();
    // Fold the 64-bit hash into 32 bits (the wire `id` is a u32).
    ((full >> 32) as u32) ^ (full as u32)
}

/// Rasterize an `NSImage` cursor into a freshly-allocated, tightly-packed **top-down RGBA8** bitmap
/// and return `(width, height, rgba)` with `rgba.len() == width*height*4`.
///
/// Returns `None` if the image has no drawable `CGImage`, is empty, exceeds [`MAX_CURSOR_DIM`] in
/// either dimension, or the color-space/context could not be created.
///
/// We ask the image for its backing `CGImage` at its native size, then draw it into our own
/// `CGBitmapContext` (device-RGB, 8 bits/component, premultiplied-last alpha, no row padding) so the
/// output format is deterministic regardless of the cursor's source representation.
fn cursor_image_to_rgba(image: &NSImage) -> Option<(u32, u32, Vec<u8>)> {
    // Ask the image for a `CGImage` sized to its own bounds. `proposed_dest_rect = null` lets AppKit
    // use the image's natural size; `context`/`hints = None` = default rendering.
    // SAFETY: null `proposed_dest_rect` is explicitly permitted by the method contract; `None`
    // context/hints are valid; we only borrow the returned image.
    let cg: Retained<CGImage> =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;

    let width = CGImage::width(Some(&cg)) as u32;
    let height = CGImage::height(Some(&cg)) as u32;
    if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
        return None;
    }

    let bytes_per_row = (width as usize).checked_mul(4)?;
    let total = bytes_per_row.checked_mul(height as usize)?;

    // Owned, zero-initialised (fully transparent) backing buffer. CoreGraphics draws into it; we keep
    // ownership so no CG-allocated buffer escapes.
    let mut buf = vec![0u8; total];

    let color_space: CFRetained<CGColorSpace> = CGColorSpace::new_device_rgb()?;

    // Classic `CGBitmapContextCreate` — a stable public CoreGraphics symbol not bound by
    // objc2-core-graphics 0.3.2 (which only exposes the block-based `CreateAdaptive`). We declare it
    // over the crate's already-bound opaque CF types. Premultiplied-last (RGBA) alpha; big-endian
    // component order is the default (no CGBitmapInfo byte-order flags), giving R,G,B,A byte layout.
    extern "C-unwind" {
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: Option<&CGColorSpace>,
            bitmap_info: u32,
        ) -> *mut CGContext;
    }

    // SAFETY: `buf` is a valid, writable, zero-initialised region of exactly
    // `bytes_per_row * height` bytes and outlives the context (dropped after we copy out). All scalar
    // arguments match the drawn geometry; `space` is a live device-RGB color space. The returned
    // context is retained (Create rule) and wrapped in `CFRetained` for release-on-drop.
    let ctx_ptr = unsafe {
        CGBitmapContextCreate(
            buf.as_mut_ptr().cast::<c_void>(),
            width as usize,
            height as usize,
            8,
            bytes_per_row,
            Some(&color_space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    };
    let ctx_ptr = NonNull::new(ctx_ptr)?;
    // SAFETY: `CGBitmapContextCreate` follows the Create rule (returns a +1 retained context), so we
    // own this reference; `CFRetained::from_raw` takes that ownership and releases it on drop.
    let ctx: CFRetained<CGContext> = unsafe { CFRetained::from_raw(ctx_ptr) };

    // Draw the cursor image over the whole (transparent) bitmap. CoreGraphics' context origin is
    // bottom-left, but `CGBitmapContextGetData` returns the buffer top-down when drawn this way for a
    // premultiplied-last device-RGB context; the whole-rect draw at (0,0,w,h) fills it exactly.
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: width as CGFloat,
            height: height as CGFloat,
        },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(&cg));

    // `buf` now holds the drawn RGBA. It is exactly `total` bytes, no padding (bytes_per_row = w*4).
    Some((width, height, buf))
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
    fn shape_id_is_stable_and_content_sensitive() {
        let a = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        // Identical inputs → identical id (so a repeat becomes a cache reference).
        assert_eq!(shape_id(&a, 2, 1, 0, 0), shape_id(&a, 2, 1, 0, 0));
        // Different pixels → different id.
        let mut c = a.clone();
        c[0] ^= 0xFF;
        assert_ne!(shape_id(&a, 2, 1, 0, 0), shape_id(&c, 2, 1, 0, 0));
        // Same pixels, different dims / hot-spot → different id.
        assert_ne!(shape_id(&a, 2, 1, 0, 0), shape_id(&a, 1, 2, 0, 0));
        assert_ne!(shape_id(&a, 2, 1, 0, 0), shape_id(&a, 2, 1, 1, 0));
    }

    #[test]
    fn norm_maps_fraction_to_wire_range() {
        assert_eq!(norm(0.0), 0);
        assert_eq!(norm(1.0), 65535);
        assert_eq!(norm(0.5), 32768); // round(0.5 * 65535) = 32768
                                      // Out of range / non-finite pins into range.
        assert_eq!(norm(2.0), 65535);
        assert_eq!(norm(-1.0), 0);
        assert_eq!(norm(f64::NAN), 0);
    }
}
