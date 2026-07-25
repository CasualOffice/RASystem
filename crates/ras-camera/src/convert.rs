//! Pure pixel-conversion + the `CapturedFrame` adapter — the part of camera capture that is fully
//! unit-testable off-device. Camera frameworks hand back RGB(A); the shared software encoder wants
//! top-down BGRA8888 ([`ras_media::SurfaceKind::CpuBgra`]). These helpers own the converted BGRA
//! buffer and expose it as a borrowed [`ras_media::CapturedFrame`], exactly like the screen `scap`
//! backend — so a camera frame reuses the video encoder verbatim (ADR-103).

use ras_media::{CapturedFrame, CpuBgraFrame, PlatformSurface, SurfaceKind};

/// Convert packed RGB8 (`R,G,B` per pixel, top-down) into tightly-packed BGRA8888 (`B,G,R,255`). The
/// output is `width*height*4` bytes with no row padding (stride = `width*4`). Extra trailing input is
/// ignored; missing input pixels are left black — never a panic on a short/odd buffer (fail-safe).
#[must_use]
pub fn rgb8_to_bgra(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let px = width as usize * height as usize;
    let mut out = vec![0u8; px * 4];
    for i in 0..px {
        let s = i * 3;
        if s + 2 >= rgb.len() {
            break;
        }
        let d = i * 4;
        out[d] = rgb[s + 2]; // B
        out[d + 1] = rgb[s + 1]; // G
        out[d + 2] = rgb[s]; // R
        out[d + 3] = 0xFF; // A (opaque)
    }
    out
}

/// Convert packed RGBA8 (`R,G,B,A`) into BGRA8888, forcing alpha opaque (a call frame has no
/// meaningful transparency and the encoder ignores alpha). Fail-safe on a short buffer.
#[must_use]
pub fn rgba8_to_bgra(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let px = width as usize * height as usize;
    let mut out = vec![0u8; px * 4];
    for i in 0..px {
        let s = i * 4;
        if s + 3 >= rgba.len() {
            break;
        }
        let d = i * 4;
        out[d] = rgba[s + 2]; // B
        out[d + 1] = rgba[s + 1]; // G
        out[d + 2] = rgba[s]; // R
        out[d + 3] = 0xFF;
    }
    out
}

/// An owned CPU BGRA camera frame + its borrowed-surface descriptor. Owns the allocation the
/// `CpuBgraFrame` pointer addresses (read only through that pointer, in the encoder) — hold it alive.
pub struct CameraBuf {
    // The BGRA bytes. Read through `desc.data` in the encoder, so the compiler can't see the use.
    #[allow(dead_code)]
    data: Vec<u8>,
    desc: CpuBgraFrame,
    width: u32,
    height: u32,
    captured_at_us: u64,
}

// SAFETY: `desc.data` is a raw pointer into `data`'s own heap allocation (self-referential). Moving a
// `CameraBuf` moves only the `Vec` handle, not its heap buffer, so `desc.data` stays valid across a
// move/thread transfer. The bytes are read only through that pointer, in the paired encoder. Mirrors
// `ras-media-scap`'s `unsafe impl Send for Buf`.
unsafe impl Send for CameraBuf {}

impl CameraBuf {
    /// Build from a tightly-packed (`stride = width*4`) BGRA buffer. Returns `None` if the buffer is
    /// too small for `width*height*4` — a malformed frame is dropped, never read out of bounds.
    #[must_use]
    pub fn from_bgra(bgra: Vec<u8>, width: u32, height: u32, captured_at_us: u64) -> Option<Self> {
        let stride = width as usize * 4;
        let needed = stride * height as usize;
        if bgra.len() < needed || needed == 0 {
            return None;
        }
        let desc = CpuBgraFrame {
            data: bgra.as_ptr(),
            len: bgra.len(),
            stride,
            width,
            height,
        };
        Some(Self {
            data: bgra,
            desc,
            width,
            height,
            captured_at_us,
        })
    }

    /// A borrowed [`CapturedFrame`] view for the encoder.
    #[must_use]
    pub fn frame(&self) -> CameraFrame<'_> {
        CameraFrame { buf: self }
    }
}

/// A borrowed camera frame; exposes its BGRA buffer as a `CpuBgra` surface (mirrors `scap`'s frame).
pub struct CameraFrame<'a> {
    buf: &'a CameraBuf,
}

impl CapturedFrame for CameraFrame<'_> {
    fn captured_at_us(&self) -> u64 {
        self.buf.captured_at_us
    }
    fn width(&self) -> u32 {
        self.buf.width
    }
    fn height(&self) -> u32 {
        self.buf.height
    }
    fn platform_surface(&self) -> PlatformSurface<'_> {
        PlatformSurface::from_ptr(
            core::ptr::from_ref(&self.buf.desc).cast(),
            SurfaceKind::CpuBgra,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rgb_to_bgra_swaps_channels_and_sets_opaque_alpha() {
        // One red pixel + one green pixel (RGB) → BGRA.
        let rgb = [255, 0, 0, 0, 255, 0];
        let bgra = rgb8_to_bgra(&rgb, 2, 1);
        assert_eq!(bgra, vec![0, 0, 255, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn rgba_to_bgra_forces_opaque() {
        // Blue pixel with alpha 0 → BGRA opaque blue.
        let rgba = [0, 0, 255, 0];
        assert_eq!(rgba8_to_bgra(&rgba, 1, 1), vec![255, 0, 0, 255]);
    }

    #[test]
    fn conversion_is_fail_safe_on_short_input() {
        // Ask for 2 pixels but give 1 pixel of data → no panic; missing pixel stays black.
        let bgra = rgb8_to_bgra(&[10, 20, 30], 2, 1);
        assert_eq!(bgra.len(), 8);
        assert_eq!(&bgra[0..4], &[30, 20, 10, 255]); // first pixel converted
        assert_eq!(&bgra[4..8], &[0, 0, 0, 0]); // second left black
    }

    #[test]
    fn camera_buf_rejects_undersized_buffers_and_adapts_valid_ones() {
        // Too small for 2×2×4 = 16 bytes → None.
        assert!(CameraBuf::from_bgra(vec![0u8; 15], 2, 2, 0).is_none());
        assert!(CameraBuf::from_bgra(vec![], 0, 0, 0).is_none());
        // Valid → a CapturedFrame reporting the right geometry + a CpuBgra surface.
        let buf = CameraBuf::from_bgra(vec![7u8; 16], 2, 2, 123).unwrap();
        let f = buf.frame();
        assert_eq!((f.width(), f.height(), f.captured_at_us()), (2, 2, 123));
        // The surface is a valid CpuBgra pointer (and only matches when asked for CpuBgra).
        assert!(f.platform_surface().as_ptr(SurfaceKind::CpuBgra).is_some());
        assert!(f.platform_surface().as_ptr(SurfaceKind::None).is_none());
    }
}
