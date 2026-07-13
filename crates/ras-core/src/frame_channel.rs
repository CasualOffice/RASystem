//! The controller frame-Channel wire contract (design §6.1).
//!
//! Each frame crosses the Tauri `Channel(Raw)` boundary as **one binary blob**: a fixed 24-byte
//! little-endian header followed by the Annex-B access unit. Identifiers travel **only** in the
//! binary header (never a JSON sidecar) because a JS `number` corrupts `u64`s past 2^53 — the TS
//! side reads `frame_id`/`captured_at_us` with `DataView.getBigUint64`. This module is the Rust end
//! of that contract; `FRAME_HEADER_LEN` and `FRAME_MAGIC` are shared verbatim with the TS
//! `decoder.worker.ts` `parseHeader`.
//!
//! It is kept dependency-light and Tauri-free so it can be unit-tested here; the actual
//! `Channel`-pump `FrameSink` (which owns a `tauri::ipc::Channel`) lives in the controller app and
//! calls [`encode_frame_blob`].

use ras_media::EncodedFrame;

/// Header magic (`"RAS1"` as a big-endian ASCII tag, stored little-endian on the wire). Framing
/// validation only — a mismatch means desync, not an attacker (the Channel is in-process).
pub const FRAME_MAGIC: u32 = u32::from_be_bytes(*b"RAS1");

/// Fixed header length in bytes. Shared constant with the TS decoder worker.
pub const FRAME_HEADER_LEN: usize = 24;

/// bit0 of the flags byte: this access unit is a keyframe (IDR).
pub const FLAG_KEYFRAME: u8 = 0b0000_0001;

/// The parsed 24-byte frame header. Field order/offsets are the shared contract:
/// `magic:u32 | flags:u8 | pad:u8 | pad:u16 | frame_id:u64 | captured_at_us:u64` (all little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Framing magic; must equal [`FRAME_MAGIC`].
    pub magic: u32,
    /// bit0 = keyframe.
    pub flags: u8,
    /// Monotonic frame id (read as `BigInt` in JS).
    pub frame_id: u64,
    /// Host monotonic capture time, microseconds (read as `BigInt` in JS).
    pub captured_at_us: u64,
}

impl FrameHeader {
    /// Whether bit0 (keyframe) is set.
    #[must_use]
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    /// Write this header into the first [`FRAME_HEADER_LEN`] bytes of `out` (little-endian).
    fn write_into(&self, out: &mut [u8; FRAME_HEADER_LEN]) {
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4] = self.flags;
        out[5] = 0; // reserved
        out[6..8].copy_from_slice(&0u16.to_le_bytes()); // reserved
        out[8..16].copy_from_slice(&self.frame_id.to_le_bytes());
        out[16..24].copy_from_slice(&self.captured_at_us.to_le_bytes());
    }
}

/// Serialize an [`EncodedFrame`] into a single Channel blob: 24-byte header + Annex-B access unit.
///
/// One contiguous owned `Vec<u8>` (what `InvokeResponseBody::Raw` wants). The frame path is
/// allocation-light overall because [`EncodedFrame::data`] is `Bytes`; here we pay one copy at the
/// IPC boundary, which the controller pump can amortize with a free-list if it ever shows up in a
/// profile.
#[must_use]
pub fn encode_frame_blob(frame: &EncodedFrame) -> Vec<u8> {
    let mut blob = Vec::with_capacity(FRAME_HEADER_LEN + frame.data.len());
    let mut header = [0u8; FRAME_HEADER_LEN];
    FrameHeader {
        magic: FRAME_MAGIC,
        flags: if frame.is_keyframe { FLAG_KEYFRAME } else { 0 },
        frame_id: frame.frame_id,
        captured_at_us: frame.captured_at_us,
    }
    .write_into(&mut header);
    blob.extend_from_slice(&header);
    blob.extend_from_slice(&frame.data);
    blob
}

/// Parse a frame header from the front of `blob`. Returns `None` if the blob is too short or the
/// magic does not match (desync/garbage). The Annex-B payload is `blob[FRAME_HEADER_LEN..]`.
#[must_use]
pub fn parse_header(blob: &[u8]) -> Option<FrameHeader> {
    if blob.len() < FRAME_HEADER_LEN {
        return None;
    }
    let magic = u32::from_le_bytes(blob[0..4].try_into().ok()?);
    if magic != FRAME_MAGIC {
        return None;
    }
    Some(FrameHeader {
        magic,
        flags: blob[4],
        frame_id: u64::from_le_bytes(blob[8..16].try_into().ok()?),
        captured_at_us: u64::from_le_bytes(blob[16..24].try_into().ok()?),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use bytes::Bytes;
    use ras_media::{ColorSpace, StreamConfig, VideoCodec, VideoTransportKind};

    fn frame(frame_id: u64, captured_at_us: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            frame_id,
            captured_at_us,
            is_keyframe: keyframe,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA, 0xBB]),
            config: StreamConfig {
                codec: VideoCodec::H264AnnexB,
                width: 1280,
                height: 720,
                fps: 30,
                target_bitrate_bps: 4_000_000,
                color: ColorSpace::Bt709Limited,
                video_transport: VideoTransportKind::PerFrameStream,
            },
        }
    }

    #[test]
    fn blob_layout_is_header_plus_annexb() {
        let f = frame(42, 1_000_000, true);
        let blob = encode_frame_blob(&f);
        assert_eq!(blob.len(), FRAME_HEADER_LEN + f.data.len());
        assert_eq!(&blob[FRAME_HEADER_LEN..], &f.data[..]);
    }

    #[test]
    fn header_round_trips_with_keyframe_bit() {
        let f = frame(42, 1_000_000, true);
        let h = parse_header(&encode_frame_blob(&f)).unwrap();
        assert_eq!(h.magic, FRAME_MAGIC);
        assert_eq!(h.frame_id, 42);
        assert_eq!(h.captured_at_us, 1_000_000);
        assert!(h.is_keyframe());

        let ng = parse_header(&encode_frame_blob(&frame(43, 2, false))).unwrap();
        assert!(!ng.is_keyframe());
    }

    #[test]
    fn u64_ids_beyond_2_pow_53_survive() {
        // The whole point of the binary header: a JS number would corrupt this, a BigInt won't.
        let big = (1u64 << 53) + 12_345;
        let h = parse_header(&encode_frame_blob(&frame(big, big + 1, false))).unwrap();
        assert_eq!(h.frame_id, big);
        assert_eq!(h.captured_at_us, big + 1);
    }

    #[test]
    fn short_or_wrong_magic_is_rejected() {
        assert!(parse_header(&[0u8; 10]).is_none());
        let mut blob = encode_frame_blob(&frame(1, 1, false));
        blob[0] ^= 0xFF; // corrupt the magic
        assert!(parse_header(&blob).is_none());
    }
}
