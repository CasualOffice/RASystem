//! X11 `CursorObserver` implementation. Linux-only (`cfg`-gated in `lib.rs`).

use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use x11rb::connection::Connection as _; // brings `setup()` into scope (screen dimensions)
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use ras_core::{CursorFrame, CursorObserver, CursorShape};

/// Upper bound on cursor edge length, mirroring the wire cap the core enforces
/// ([`ras_protocol::MAX_CURSOR_DIM`], re-declared here to avoid a proto dep). A cursor larger than this
/// in either dimension is skipped (reported as [`CursorFrame::Hidden`]) rather than truncated — a normal
/// OS cursor is well under this.
const MAX_CURSOR_DIM: u32 = 256;

/// How long to sleep between cursor polls. The OS cursor changes only on user action; a short poll is
/// cheap (one XFixes round-trip only, plus a small ARGB→RGBA unpack when the shape actually changed) and
/// keeps the observed shape fresh without installing an XFixes `CursorNotify` event loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An X11 host cursor observer: polls the XFixes cursor image for the live system cursor and reports each
/// **change** as a [`CursorFrame`] over the [`CursorObserver`] seam. Deduped by a content hash of the RGBA
/// bytes, so an unchanged cursor yields the same `id` (the core then sends a cache reference) and this
/// observer stays quiet until the shape actually changes.
///
/// Fail-closed: with no reachable X server (or no XFixes extension) the connection is `None` and
/// [`CursorObserver::next`] returns `None` immediately — the host then has no host cursor to forward
/// rather than a wrong one (mirrors `ras-input-linux`'s unprivileged, fail-closed posture).
pub struct X11CursorObserver {
    /// The X11 connection to `$DISPLAY`, or `None` if unreachable / XFixes absent (fail-closed).
    conn: Option<RustConnection>,
    /// Root-window pixel dimensions, for normalizing the cursor's root position to `0..=65535`. On a
    /// single-monitor desktop the root equals the shared display; on multi-monitor the position is
    /// normalized over the whole virtual desktop (a known follow-up, matching the macOS observer).
    screen_w: u16,
    screen_h: u16,
    /// The last `id` we emitted (content hash of the last shape's RGBA), so we only report shape changes.
    last_id: Option<u32>,
    /// The last normalized position we emitted, so we only report movement (`Moved`) on a real change.
    last_pos: Option<(u16, u16)>,
}

impl X11CursorObserver {
    /// Connect to `$DISPLAY` and negotiate XFixes. Never panics: an unreachable X server — or a server
    /// without XFixes — yields an observer whose [`CursorObserver::next`] ends immediately (`None`).
    #[must_use]
    pub fn new() -> Self {
        let (conn, screen_w, screen_h) = match RustConnection::connect(None) {
            Ok((conn, screen_num)) => {
                // XFixes must be version-negotiated before `get_cursor_image` is usable. A single
                // round-trip; if it fails the extension is unavailable, so drop the connection and
                // fail closed (no cursor rather than a broken request every poll).
                let ok = conn
                    .xfixes_query_version(5, 0)
                    .is_ok_and(|c| c.reply().is_ok());
                if ok {
                    let screen = &conn.setup().roots[screen_num];
                    let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
                    (Some(conn), w, h)
                } else {
                    (None, 0, 0)
                }
            }
            Err(_) => (None, 0, 0),
        };
        Self {
            conn,
            screen_w,
            screen_h,
            last_id: None,
            last_pos: None,
        }
    }
}

impl Default for X11CursorObserver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CursorObserver for X11CursorObserver {
    async fn next(&mut self) -> Option<CursorFrame> {
        // No X server / no XFixes → the observer ends immediately (fail-closed). The host forwards no
        // cursor rather than a wrong one.
        self.conn.as_ref()?;

        // Poll until the cursor SHAPE or POSITION differs from what we last reported. The observer models
        // a *stream of changes*, so we suppress repeats here (in addition to the core's send-side dedup)
        // and never busy-spin. Shape changes win over position changes in a single poll; the core's
        // send-side throttle rate-limits the frequent `Moved` stream.
        loop {
            let Some((frame, rx, ry)) = capture_cursor(self.conn.as_ref()?) else {
                return None; // connection error → observer ends (fail-closed)
            };
            let this_id = match &frame {
                CursorFrame::Shape(s) => s.id,
                // A hidden/too-large cursor collapses to a single sentinel so Hidden→Hidden is a repeat.
                _ => 0,
            };
            // Normalize the root-window cursor position to 0..=65535 for the wire (matches the pointer).
            let pos = (
                normalize_pos(rx, self.screen_w),
                normalize_pos(ry, self.screen_h),
            );

            // Shape change (incl. Shape↔Hidden) takes priority — emit it and re-seed the position so the
            // next poll only reports genuine movement.
            if self.last_id != Some(this_id) {
                self.last_id = Some(this_id);
                self.last_pos = Some(pos);
                return Some(frame);
            }
            // Otherwise, a position-only change → emit Moved.
            if self.last_pos != Some(pos) {
                self.last_pos = Some(pos);
                return Some(CursorFrame::Moved { x: pos.0, y: pos.1 });
            }
            // Nothing changed (or transiently unreadable): wait and poll again.
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// Normalize a root-window pixel coordinate to `0..=65535` over `dim` pixels (matching the wire pointer
/// units). Off-screen / out-of-range coordinates clamp to the edges; a zero `dim` (no screen) maps to 0.
fn normalize_pos(v: i16, dim: u16) -> u16 {
    if dim == 0 {
        return 0;
    }
    let clamped = v.clamp(0, dim as i16) as u32;
    ((clamped * 65535) / u32::from(dim)) as u16
}

/// Read the current X11 cursor once via XFixes: its shape frame plus its **root-window position**
/// (`reply.x`, `reply.y`, in pixels). Returns:
/// - `Some((Shape, x, y))` for a normal, in-bounds cursor,
/// - `Some((Hidden, x, y))` if the cursor image is empty / oversized / malformed (draw nothing),
/// - `None` only on a connection error (treated as the observer ending).
fn capture_cursor(conn: &RustConnection) -> Option<(CursorFrame, i16, i16)> {
    // One XFixes round-trip. A connection-level error ends the observer (`None`); anything malformed
    // in the *reply* collapses to `Hidden` (draw nothing) so a weird cursor never crashes the pump.
    let reply = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
    let (px, py) = (reply.x, reply.y);

    let width = u32::from(reply.width);
    let height = u32::from(reply.height);
    if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
        return Some((CursorFrame::Hidden, px, py));
    }

    // XFixes gives one pixel per `u32`, so the vec length must be exactly width*height. A short/long
    // buffer is malformed — draw nothing rather than read out of bounds or ship a wrong-sized bitmap.
    let expected = (width as usize).checked_mul(height as usize)?;
    if reply.cursor_image.len() != expected {
        return Some((CursorFrame::Hidden, px, py));
    }

    let rgba = argb_u32_to_rgba8(&reply.cursor_image);
    // `id` = content hash of the RGBA. Identical bitmaps hash equally, so an unchanged cursor reuses its
    // id and the core sends a `CursorCached` reference instead of the pixels.
    let id = hash_rgba(&rgba);

    // Hot-spot is in the image's coordinate space. Clamp inside the bitmap so `hotspot_x < width` /
    // `hotspot_y < height` always holds (wire contract).
    let hx = clamp_hotspot(reply.xhot, width);
    let hy = clamp_hotspot(reply.yhot, height);

    Some((
        CursorFrame::Shape(CursorShape {
            id,
            hotspot_x: hx,
            hotspot_y: hy,
            width: width as u16,
            height: height as u16,
            rgba: Bytes::from(rgba),
        }),
        px,
        py,
    ))
}

/// Convert XFixes ARGB cursor pixels (one **premultiplied** ARGB pixel per `u32`, low 24 bits = RGB, high
/// 8 = alpha) into tightly-packed, straight-alpha **RGBA8** bytes (`R,G,B,A` per pixel).
///
/// XFixes stores each pixel as a native-endian `u32` with alpha in bits 24..32 and R/G/B in bits
/// 16..24 / 8..16 / 0..8, with the colour channels **premultiplied** by alpha. We un-premultiply so the
/// output matches `ras-cursor-macos`'s non-premultiplied (straight) alpha — the same wire contract on
/// both platforms. Output length is exactly `pixels.len() * 4`.
fn argb_u32_to_rgba8(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for &px in pixels {
        let a = ((px >> 24) & 0xFF) as u8;
        let r = ((px >> 16) & 0xFF) as u8;
        let g = ((px >> 8) & 0xFF) as u8;
        let b = (px & 0xFF) as u8;
        let (r, g, b) = unpremultiply(r, g, b, a);
        out.push(r);
        out.push(g);
        out.push(b);
        out.push(a);
    }
    out
}

/// Un-premultiply one channel triple by its alpha, saturating at 255. `a == 0` (fully transparent) leaves
/// the colour at 0 — the pixel is invisible, so its RGB is irrelevant. `a == 255` is a no-op.
fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 0 || a == 255 {
        return (r, g, b);
    }
    let a16 = u16::from(a);
    let up = |c: u8| -> u8 {
        // round( c * 255 / a ), clamped to 255 (guards against slightly-out-of-range premultiplied data).
        let v = (u16::from(c) * 255 + a16 / 2) / a16;
        v.min(255) as u8
    };
    (up(r), up(g), up(b))
}

/// Clamp a hot-spot coordinate into `0..dim` and return it as a `u16` (`dim <= MAX_CURSOR_DIM`, so it
/// always fits). An out-of-range hot-spot pins to the last in-bounds pixel.
fn clamp_hotspot(v: u16, dim: u32) -> u16 {
    let max = dim.saturating_sub(1);
    (u32::from(v)).min(max) as u16
}

/// FNV-style 32-bit hash of the RGBA bytes — a stable content id for the shape (dedup key). Not a
/// security hash; collisions only ever cause a redundant `CursorCached` reuse, never a wrong shape.
/// Identical to `ras-cursor-macos` so the id is a pure function of the pixels.
fn hash_rgba(rgba: &[u8]) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut h);
    // Fold the 64-bit hash into 32 bits (the wire `id` is a u32).
    let full = h.finish();
    ((full >> 32) as u32) ^ (full as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotspot_clamps_into_bounds() {
        assert_eq!(clamp_hotspot(5, 16), 5);
        // Out of range pins to the last in-bounds pixel.
        assert_eq!(clamp_hotspot(100, 16), 15);
        assert_eq!(clamp_hotspot(0, 1), 0);
        assert_eq!(clamp_hotspot(50, 1), 0);
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

    #[test]
    fn argb_unpacks_to_tightly_packed_rgba() {
        // One opaque pixel: A=0xFF, R=0x11, G=0x22, B=0x33 → 0xFF112233.
        let px = 0xFF11_2233u32;
        let out = argb_u32_to_rgba8(&[px]);
        assert_eq!(out, vec![0x11, 0x22, 0x33, 0xFF]);
        // Length is exactly pixels * 4.
        assert_eq!(argb_u32_to_rgba8(&[0, 0, 0]).len(), 12);
    }

    #[test]
    fn fully_transparent_pixel_keeps_zero_rgb() {
        // A=0, premultiplied RGB is 0 → stays 0 (invisible), alpha 0.
        let out = argb_u32_to_rgba8(&[0x0000_0000]);
        assert_eq!(out, vec![0, 0, 0, 0]);
    }

    #[test]
    fn unpremultiply_recovers_full_color_at_half_alpha() {
        // Premultiplied half-alpha grey: a=128, premultiplied c = round(255 * 128/255) = 128.
        // Un-premultiplying 128 by 128 recovers ~255.
        let (r, g, b) = unpremultiply(128, 128, 128, 128);
        assert!(r >= 254 && g >= 254 && b >= 254, "recovered {r},{g},{b}");
        // Opaque is a no-op.
        assert_eq!(unpremultiply(10, 20, 30, 255), (10, 20, 30));
    }
}
