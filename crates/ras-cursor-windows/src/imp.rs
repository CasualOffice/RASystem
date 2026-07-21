//! Windows `CursorObserver` implementation. Windows-only (`cfg`-gated in `lib.rs`).
//!
//! The Win32 cursor-shape read is a fixed pipeline:
//! 1. [`GetCursorInfo`] → the global cursor state. `CURSOR_SHOWING` clear ⇒ [`CursorFrame::Hidden`].
//! 2. The visible `HCURSOR` → [`GetIconInfo`], which yields the hot-spot (`xHotspot`/`yHotspot`) plus
//!    the color (`hbmColor`) and mask (`hbmMask`) bitmaps.
//! 3. [`GetObjectW`] on `hbmColor` (or the mask, for a monochrome cursor) gives the pixel dimensions.
//! 4. A tightly-packed **top-down 32-bit BGRA** [`CreateDIBSection`] backs a memory DC; [`DrawIconEx`]
//!    composites the cursor into it (`DI_NORMAL` applies the AND-mask + XOR-color, so monochrome
//!    cursors are handled as best-effort by the OS's own renderer).
//! 5. The DIB bytes are copied out and **BGRA→RGBA** (swap B/R) into an owned `Vec` — no GDI handle
//!    or raw pointer escapes this module.
//!
//! Every GDI object is released on every path (including early returns) so a repeated poll never leaks.

use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetObjectW, ReleaseDC,
    SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, GetCursorInfo, GetCursorPos, GetIconInfo, GetSystemMetrics,
    CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HCURSOR, ICONINFO, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use ras_core::{CursorFrame, CursorObserver, CursorShape};

/// Upper bound on cursor edge length, mirroring the wire cap the core enforces
/// ([`ras_protocol::MAX_CURSOR_DIM`]). A cursor larger than this in either dimension is skipped
/// (reported as [`CursorFrame::Hidden`]) rather than truncated — a normal OS cursor is well under this.
const MAX_CURSOR_DIM: u32 = 256;

/// How long to sleep between cursor polls. The OS cursor changes only on user action; a short poll is
/// cheap (one `GetCursorInfo`, then a small draw only when the shape actually changed) and keeps the
/// observed shape fresh without installing a hook (which would demand a message loop / hook DLL).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A Windows host cursor observer: polls the global cursor (`GetCursorInfo` for the live shape,
/// `GetCursorPos` for the live position) and reports each **change** as a [`CursorFrame`] over the
/// [`CursorObserver`] seam. Shape changes are deduped by a content hash of the RGBA bytes, so an
/// unchanged cursor yields the same `id` (the core then sends a cache reference); position changes are
/// deduped against the last normalized position, so a still cursor stays quiet. A shape change wins over
/// a position change in a single poll (mirrors `ras-cursor-linux`).
pub struct WinCursorObserver {
    /// The last `id` we emitted (content hash of the last shape's RGBA), so we only report shape changes.
    /// `None` before the first frame.
    last_id: Option<u32>,
    /// The last normalized position we emitted, so we only report movement (`Moved`) on a real change.
    last_pos: Option<(u16, u16)>,
    /// Virtual-desktop origin + size in pixels (`SM_{X,Y,CX,CY}VIRTUALSCREEN`), read once at
    /// construction, for normalizing the cursor's global position to `0..=65535`. The origin is
    /// **negative-capable** on a multi-monitor layout (a display left/above the primary).
    virt: (i32, i32, i32, i32),
}

impl WinCursorObserver {
    /// Create a cursor observer. Captures nothing until [`CursorObserver::next`] is first awaited.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: `GetSystemMetrics` is a pure query with no arguments beyond a metric index.
        let virt = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        Self {
            last_id: None,
            last_pos: None,
            virt,
        }
    }

    /// Read the live global cursor position (`GetCursorPos`) and normalize it to `0..=65535` over the
    /// virtual desktop (matching the wire pointer units). Returns `None` if the position is unreadable
    /// (so the caller leaves `last_pos` untouched and simply polls again) or the virtual desktop has a
    /// zero dimension.
    fn read_normalized_pos(&self) -> Option<(u16, u16)> {
        let mut pt = POINT::default();
        // SAFETY: `pt` is a valid, sized `POINT`; `GetCursorPos` only writes into it.
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            return None;
        }
        let (vx, vy, vw, vh) = self.virt;
        Some((normalize_pos(pt.x, vx, vw), normalize_pos(pt.y, vy, vh)))
    }
}

impl Default for WinCursorObserver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CursorObserver for WinCursorObserver {
    async fn next(&mut self) -> Option<CursorFrame> {
        // Poll until the cursor SHAPE or POSITION differs from what we last reported. The observer models
        // a *stream of changes*, so we suppress repeats here (in addition to the core's send-side dedup)
        // and never busy-spin. A shape change (incl. Shape↔Hidden) wins over a position change in a
        // single poll; the core's send-side throttle rate-limits the frequent `Moved` stream.
        loop {
            let frame = capture_cursor_frame();
            let this_id = match &frame {
                Some(CursorFrame::Shape(s)) => Some(s.id),
                // A hidden/too-large cursor collapses to a single sentinel so Hidden→Hidden is a repeat.
                Some(CursorFrame::Hidden) => Some(0),
                // `capture_cursor_frame` only ever produces Shape/Hidden; a `Moved` would be a bug, but
                // forward it directly (never dedup against a shape id) rather than mishandle it.
                Some(CursorFrame::Moved { x, y }) => {
                    return Some(CursorFrame::Moved { x: *x, y: *y })
                }
                None => None,
            };

            match (frame, this_id) {
                // Shape change (incl. Shape↔Hidden) takes priority — emit it and re-seed the position so
                // the next poll only reports genuine movement.
                (Some(frame), Some(id)) if self.last_id != Some(id) => {
                    self.last_id = Some(id);
                    if let Some(pos) = self.read_normalized_pos() {
                        self.last_pos = Some(pos);
                    }
                    return Some(frame);
                }
                // Same shape as last time (or transiently unreadable): check for a position-only change.
                _ => {
                    if let Some(pos) = self.read_normalized_pos() {
                        if self.last_pos != Some(pos) {
                            self.last_pos = Some(pos);
                            return Some(CursorFrame::Moved { x: pos.0, y: pos.1 });
                        }
                    }
                    // Nothing changed: wait and poll again.
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Read the current OS cursor once and convert it to a [`CursorFrame`]. Returns:
/// - `Some(Shape)` for a normal, in-bounds, visible cursor,
/// - `Some(Hidden)` if the cursor is hidden / empty / oversized / unreadable (draw nothing),
/// - `None` never (the global cursor always exists while the session is up — this observer does not
///   self-terminate; the host drops it on teardown).
///
/// All `unsafe` FFI is confined to this function's helpers; no raw pointer/handle escapes.
fn capture_cursor_frame() -> Option<CursorFrame> {
    // 1. Global cursor state. `cbSize` must be set before the call.
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid, sized `CURSORINFO`; `GetCursorInfo` only writes into it.
    let got = unsafe { GetCursorInfo(&mut info) };
    if got.is_err() {
        // Transiently unreadable — treat as hidden (draw nothing) rather than terminating the stream.
        return Some(CursorFrame::Hidden);
    }

    // `flags == 0` (not `CURSOR_SHOWING`) means the cursor is hidden. `hCursor` may also be null.
    if (info.flags.0 & CURSOR_SHOWING.0) == 0 || info.hCursor.is_invalid() {
        return Some(CursorFrame::Hidden);
    }

    match cursor_to_rgba(info.hCursor) {
        Some((width, height, hotspot_x, hotspot_y, rgba)) => {
            // `id` = content hash of the RGBA. Identical bitmaps hash equally, so an unchanged cursor
            // reuses its id and the core sends a `CursorCached` reference instead of the pixels.
            let id = hash_rgba(&rgba);

            // Clamp the hot-spot inside the bitmap so `hotspot_x < width` / `hotspot_y < height` always
            // holds (wire contract enforced by `ras-core`).
            let hx = clamp_hotspot(hotspot_x, width);
            let hy = clamp_hotspot(hotspot_y, height);

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

/// Normalize a global cursor pixel coordinate to `0..=65535` over one virtual-desktop axis, given that
/// axis's `origin` (negative-capable) and `extent` (pixels). The coordinate is first shifted into the
/// desktop's local `0..extent` space, then scaled to the wire pointer range. Off-screen / out-of-range
/// coordinates clamp to the edges; a zero `extent` (no desktop) maps to 0.
fn normalize_pos(v: i32, origin: i32, extent: i32) -> u16 {
    if extent <= 0 {
        return 0;
    }
    // Shift into the desktop-local space and clamp to `0..extent`. `saturating_sub` guards the shift
    // against an out-of-range coordinate (e.g. a stale read during a display reconfigure).
    let local = v.saturating_sub(origin).clamp(0, extent) as u32;
    ((local * 65535) / extent as u32) as u16
}

/// Clamp a hot-spot coordinate into `0..dim` and return it as a `u16` (`dim <= MAX_CURSOR_DIM`, so it
/// always fits). An out-of-range hot-spot pins to the last in-bounds pixel.
fn clamp_hotspot(v: u32, dim: u32) -> u16 {
    let max = dim.saturating_sub(1);
    v.min(max) as u16
}

/// FNV-style fold over a `DefaultHasher` of the RGBA bytes — a stable content id for the shape (dedup
/// key). Not a security hash; a collision only ever causes a redundant `CursorCached` reuse, never a
/// wrong shape.
fn hash_rgba(rgba: &[u8]) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut h);
    // Fold the 64-bit hash into 32 bits (the wire `id` is a u32).
    let full = h.finish();
    ((full >> 32) as u32) ^ (full as u32)
}

/// Render an `HCURSOR` into a tightly-packed **top-down RGBA8** buffer and return
/// `(width, height, hotspot_x, hotspot_y, rgba)` with `rgba.len() == width*height*4`.
///
/// Returns `None` if the cursor has no drawable bitmap, exceeds [`MAX_CURSOR_DIM`] in either
/// dimension, or any GDI object could not be created. Every GDI handle allocated here is released on
/// every return path.
fn cursor_to_rgba(hcursor: HCURSOR) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
    // 2. Icon info: hot-spot + color/mask bitmaps. We OWN `hbmColor`/`hbmMask` and must delete them.
    let mut ii = ICONINFO::default();
    // SAFETY: `hcursor` is a valid, showing cursor handle (checked by the caller); `GetIconInfo`
    // writes the hot-spot and creates the bitmap handles we free below.
    if unsafe { GetIconInfo(hcursor, &mut ii) }.is_err() {
        return None;
    }
    // From here every early return must free the bitmaps GetIconInfo created.
    let result = render_icon_info(&ii);
    // SAFETY: `hbmMask`/`hbmColor` were created by `GetIconInfo`; deleting them once is correct.
    // `hbmColor` is null for monochrome cursors — `DeleteObject(null)` is a harmless no-op.
    unsafe {
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
    }
    result.map(|(w, h, rgba)| (w, h, ii.xHotspot, ii.yHotspot, rgba))
}

/// Determine the cursor dimensions from its bitmaps, then draw it into a fresh top-down BGRA DIB and
/// convert to RGBA. Split out from [`cursor_to_rgba`] so the bitmap cleanup there is unconditional.
fn render_icon_info(ii: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
    // 3. Dimensions. Prefer the color bitmap; a monochrome cursor has no color bitmap and packs the
    //    AND-mask over the XOR-mask into a double-height mask bitmap, so its logical height is half.
    let (width, height) = if !ii.hbmColor.is_invalid() {
        let bm = get_bitmap_dims(HBITMAP(ii.hbmColor.0))?;
        (bm.0, bm.1)
    } else if !ii.hbmMask.is_invalid() {
        let bm = get_bitmap_dims(HBITMAP(ii.hbmMask.0))?;
        // Monochrome: mask is [AND-plane; XOR-plane] stacked, so the drawable cursor is half as tall.
        (bm.0, bm.1 / 2)
    } else {
        return None;
    };

    if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
        return None;
    }

    draw_cursor_bgra(ii, width, height)
}

/// Query an `HBITMAP`'s pixel width/height via `GetObjectW(BITMAP)`.
fn get_bitmap_dims(hbm: HBITMAP) -> Option<(u32, u32)> {
    let mut bm = BITMAP::default();
    // SAFETY: `hbm` is a valid bitmap handle; `GetObjectW` writes `size_of::<BITMAP>()` bytes into
    // `bm` (the return value is the bytes written; 0 == failure).
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(std::ptr::from_mut(&mut bm).cast()),
        )
    };
    if n == 0 {
        return None;
    }
    let w = bm.bmWidth; // BITMAP.bmWidth/bmHeight are already i32 (LONG)
    let h = bm.bmHeight;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// Create a top-down 32-bit BGRA DIB section, draw the cursor into it via `DrawIconEx(DI_NORMAL)`
/// (which applies the AND+XOR masks, so monochrome cursors render best-effort through the OS), then
/// read the bytes out and swap B/R to RGBA. Every GDI handle is released before returning.
fn draw_cursor_bgra(ii: &ICONINFO, width: u32, height: u32) -> Option<(u32, u32, Vec<u8>)> {
    // A negative `biHeight` requests a **top-down** DIB, matching the wire's top-down RGBA contract.
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    // SAFETY: all handles below are created/checked before use and released on every path (including
    // the `?` early-returns, which only fire *before* the corresponding handle is created).
    unsafe {
        let screen_dc: HDC = GetDC(HWND::default());
        if screen_dc.is_invalid() {
            return None;
        }
        let mem_dc: HDC = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            let _ = ReleaseDC(HWND::default(), screen_dc);
            return None;
        }

        // The DIB section; `bits` receives a pointer to the pixel buffer GDI owns (freed with the DIB).
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            mem_dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            None, // no file mapping
            0,
        );
        let dib: HBITMAP = match dib {
            Ok(h) if !h.is_invalid() && !bits.is_null() => h,
            _ => {
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);
                return None;
            }
        };

        // Select the DIB into the memory DC (buffer is zero-initialised → transparent black start).
        let old: HGDIOBJ = SelectObject(mem_dc, HGDIOBJ(dib.0));

        // `DrawIconEx` needs an HICON/HCURSOR handle. Rather than plumb the caller's live `HCURSOR`
        // down here, we rebuild an equivalent throwaway icon from the color+mask bitmaps in `ii`
        // (`CreateIconIndirect`) and destroy it below. This keeps the hot-spot/bitmap read in one
        // place and avoids holding the OS cursor handle across the draw.
        let temp_icon = windows::Win32::UI::WindowsAndMessaging::CreateIconIndirect(ii);
        let hicon = match temp_icon {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                let _ = SelectObject(mem_dc, old);
                let _ = DeleteObject(HGDIOBJ(dib.0));
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);
                return None;
            }
        };

        // Composite the cursor (AND+XOR masks applied by DI_NORMAL) into the top-down BGRA buffer.
        let drawn = DrawIconEx(
            mem_dc,
            0,
            0,
            HCURSOR(hicon.0),
            width as i32,
            height as i32,
            0,
            None,      // no flicker-free brush
            DI_NORMAL, // apply mask + image
        );

        // Copy the pixel bytes out before tearing down GDI. Buffer is `width*height*4` (32bpp, no pad
        // for a 32-bit top-down DIB — the row stride is exactly `width*4`).
        let total = (width as usize) * (height as usize) * 4;
        let mut rgba = Vec::new();
        if drawn.is_ok() {
            rgba.resize(total, 0u8);
            std::ptr::copy_nonoverlapping(bits.cast::<u8>(), rgba.as_mut_ptr(), total);
        }

        // Release everything (order: deselect DIB, destroy icon, delete DIB, delete DC, release DC).
        let _ = SelectObject(mem_dc, old);
        let _ = DestroyIcon(hicon);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(HWND::default(), screen_dc);

        if !drawn.is_ok() {
            return None;
        }

        // 5. BGRA → RGBA in place: swap byte 0 (B) and byte 2 (R) of every pixel.
        bgra_to_rgba_inplace(&mut rgba);
        Some((width, height, rgba))
    }
}

/// Swap the B and R channels of every 4-byte pixel, converting a BGRA buffer to RGBA in place. The
/// alpha byte is left untouched (`DrawIconEx` with a 32-bit color cursor produces per-pixel alpha; a
/// monochrome/legacy cursor may leave alpha at 0 — best-effort, the OS composited the visible pixels).
fn bgra_to_rgba_inplace(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
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
        assert_eq!(clamp_hotspot(20, 1), 0);
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
    fn normalize_pos_maps_over_virtual_desktop() {
        // Origin 0, extent 1000: endpoints and midpoint map to 0 / ~half / max.
        assert_eq!(normalize_pos(0, 0, 1000), 0);
        assert_eq!(normalize_pos(1000, 0, 1000), 65535);
        assert_eq!(normalize_pos(500, 0, 1000), 32767);
        // Out-of-range coords clamp to the edges (never wrap / overflow).
        assert_eq!(normalize_pos(-50, 0, 1000), 0);
        assert_eq!(normalize_pos(5000, 0, 1000), 65535);
        // Zero / negative extent (no desktop) maps to 0.
        assert_eq!(normalize_pos(42, 0, 0), 0);
    }

    #[test]
    fn normalize_pos_honors_negative_origin() {
        // A display left/above the primary: origin -1920, extent 1920. The left edge is at x == -1920
        // and the right edge at x == 0, so those map to 0 / max after the origin shift.
        assert_eq!(normalize_pos(-1920, -1920, 1920), 0);
        assert_eq!(normalize_pos(0, -1920, 1920), 65535);
        assert_eq!(normalize_pos(-960, -1920, 1920), 32767);
    }

    #[test]
    fn bgra_to_rgba_swaps_b_and_r() {
        // Two pixels: (B,G,R,A). After swap → (R,G,B,A).
        let mut buf = vec![10u8, 20, 30, 40, 50, 60, 70, 80];
        bgra_to_rgba_inplace(&mut buf);
        assert_eq!(buf, vec![30u8, 20, 10, 40, 70, 60, 50, 80]);
    }
}
