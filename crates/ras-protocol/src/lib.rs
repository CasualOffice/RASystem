//! Wire protocol types, framing, versioning, and the stable error taxonomy for Casual RAS.
//!
//! The protobuf message set (`proto/casual_ras.proto`) is the wire source of truth; codegen is
//! wired in a later phase. This crate is the single home for: [`ErrorCode`] + [`RasError`] (the
//! shared error taxonomy every crate aliases), the monotonic id aliases ([`FrameId`],
//! [`CaptureTimestampUs`]), and the control-plane message set ([`ControlMsg`] and friends).
//!
//! Placement note (Phase-1 design, `docs/design/phase-1-design.md` §2): the `u64` id aliases live
//! here — not in `ras-media` as the raw design drafted — because [`ControlMsg`] references
//! [`FrameId`] and `ras-media` already depends on this crate; homing them here breaks the cycle.
//! `ras-media` re-exports them so downstream code can still say `ras_media::FrameId`.

use bytes::Bytes;

pub mod codec;

/// Current bootstrap/session protocol major version. See `docs/04`.
pub const PROTOCOL_VERSION: u32 = 1;

/// DoS guard on hostile control input: the length-prefixed control-frame decoder rejects any frame
/// whose prefix claims more than this many bytes, **before** allocating or waiting for the body.
/// 1 MiB is ample for config/feedback frames. Homed here (the wire crate); `ras-transport-iroh`
/// re-exports it so `ras_transport_iroh::MAX_CONTROL_FRAME` keeps resolving.
pub const MAX_CONTROL_FRAME: usize = 1 << 20;

/// Monotonic per-stream frame id. Never wraps within a session; a gap implies loss.
///
/// Crosses to JS as a BigInt (`DataView.getBigUint64`), never a JS `number` (would corrupt past
/// 2^53 and trigger spurious keyframe requests).
pub type FrameId = u64;

/// Capture time in microseconds on the host **monotonic** clock, sampled at capture.
///
/// Not wall-clock; used only for pacing/ordering/jitter, never for authorization. Because B-frames
/// are off, capture order == decode order == presentation order, so this doubles as the WebCodecs
/// `EncodedVideoChunk.timestamp`.
pub type CaptureTimestampUs = u64;

/// Stable, machine-readable error codes exposed across SDK and wire boundaries.
///
/// Mirrors the error model in `docs/04 §14`. Codes are stable across releases: add new variants,
/// never repurpose existing ones. String forms via [`ErrorCode::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// Malformed or unparseable message.
    InvalidMessage,
    /// Protocol version not supported.
    UnsupportedVersion,
    /// Identity does not match the expected/bound endpoint.
    IdentityMismatch,
    /// Signature verification failed.
    SignatureInvalid,
    /// Request or ticket expired.
    RequestExpired,
    /// Replay of a nonce/ticket/generation detected.
    ReplayDetected,
    /// Local user denied consent.
    ConsentDenied,
    /// Requested capability not permitted by policy.
    CapabilityDenied,
    /// Session grant invalid (binding/expiry/signature).
    GrantInvalid,
    /// Control lease invalid (generation/expiry).
    LeaseInvalid,
    /// Session was revoked (incl. emergency stop).
    SessionRevoked,
    /// Transport-level failure.
    TransportError,
    /// Screen capture failure.
    CaptureFailed,
    /// Encoder failure.
    EncoderFailed,
    /// Input injection failure.
    InputFailed,
    /// Local policy changed mid-session.
    PolicyChanged,
    /// Unexpected internal error.
    Internal,
    /// Intentional, fault-free teardown (a clean `Bye`) — not an error. The canonical reason for a
    /// graceful stop/disconnect; distinct from [`SessionRevoked`](Self::SessionRevoked) (emergency
    /// stop) and from a missing `Bye` (transport loss). WebSocket-1000 / QUIC-app-error-0 analogue.
    NormalClosure,
}

impl ErrorCode {
    /// The stable wire/string form, e.g. `"SIGNATURE_INVALID"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidMessage => "INVALID_MESSAGE",
            ErrorCode::UnsupportedVersion => "UNSUPPORTED_VERSION",
            ErrorCode::IdentityMismatch => "IDENTITY_MISMATCH",
            ErrorCode::SignatureInvalid => "SIGNATURE_INVALID",
            ErrorCode::RequestExpired => "REQUEST_EXPIRED",
            ErrorCode::ReplayDetected => "REPLAY_DETECTED",
            ErrorCode::ConsentDenied => "CONSENT_DENIED",
            ErrorCode::CapabilityDenied => "CAPABILITY_DENIED",
            ErrorCode::GrantInvalid => "GRANT_INVALID",
            ErrorCode::LeaseInvalid => "LEASE_INVALID",
            ErrorCode::SessionRevoked => "SESSION_REVOKED",
            ErrorCode::TransportError => "TRANSPORT_ERROR",
            ErrorCode::CaptureFailed => "CAPTURE_FAILED",
            ErrorCode::EncoderFailed => "ENCODER_FAILED",
            ErrorCode::InputFailed => "INPUT_FAILED",
            ErrorCode::PolicyChanged => "POLICY_CHANGED",
            ErrorCode::Internal => "INTERNAL_ERROR",
            ErrorCode::NormalClosure => "NORMAL_CLOSURE",
        }
    }

    /// A stable numeric id (matching the wire `proto` enum numbering, 1-based). Use for compact,
    /// order-independent persistence/serialization (e.g. the audit journal) — never derived from the
    /// Rust enum's declaration order (`as u32`), which would silently shift if a variant is inserted.
    #[must_use]
    pub const fn to_code(self) -> u16 {
        match self {
            ErrorCode::InvalidMessage => 1,
            ErrorCode::UnsupportedVersion => 2,
            ErrorCode::IdentityMismatch => 3,
            ErrorCode::SignatureInvalid => 4,
            ErrorCode::RequestExpired => 5,
            ErrorCode::ReplayDetected => 6,
            ErrorCode::ConsentDenied => 7,
            ErrorCode::CapabilityDenied => 8,
            ErrorCode::GrantInvalid => 9,
            ErrorCode::LeaseInvalid => 10,
            ErrorCode::SessionRevoked => 11,
            ErrorCode::TransportError => 12,
            ErrorCode::CaptureFailed => 13,
            ErrorCode::EncoderFailed => 14,
            ErrorCode::InputFailed => 15,
            ErrorCode::PolicyChanged => 16,
            ErrorCode::Internal => 17,
            ErrorCode::NormalClosure => 18,
        }
    }

    /// Inverse of [`Self::to_code`]. `None` for an unrecognized code (fail-closed — never defaulted).
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => ErrorCode::InvalidMessage,
            2 => ErrorCode::UnsupportedVersion,
            3 => ErrorCode::IdentityMismatch,
            4 => ErrorCode::SignatureInvalid,
            5 => ErrorCode::RequestExpired,
            6 => ErrorCode::ReplayDetected,
            7 => ErrorCode::ConsentDenied,
            8 => ErrorCode::CapabilityDenied,
            9 => ErrorCode::GrantInvalid,
            10 => ErrorCode::LeaseInvalid,
            11 => ErrorCode::SessionRevoked,
            12 => ErrorCode::TransportError,
            13 => ErrorCode::CaptureFailed,
            14 => ErrorCode::EncoderFailed,
            15 => ErrorCode::InputFailed,
            16 => ErrorCode::PolicyChanged,
            17 => ErrorCode::Internal,
            18 => ErrorCode::NormalClosure,
            _ => return None,
        })
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The one canonical error struct; every crate aliases it (`MediaError`, `TransportError`,
/// `CoreError`, `SessionError`) so `?` needs no `From` impls.
///
/// `recoverable` is load-bearing: it drives the capture-rebuild loop (SCK restart / DXGI
/// `ACCESS_LOST`) and the reconnect window. `context` is operator-facing and **content-free** —
/// never pixels, paths, tokens, or typed text (Invariant 8).
#[derive(Debug, Clone)]
pub struct RasError {
    /// Stable machine code from the shared taxonomy.
    pub code: ErrorCode,
    /// `true` ⇒ rebuild-and-continue; `false` ⇒ fatal stop. Never contradicts `code`.
    pub recoverable: bool,
    /// Operator-facing, content-free detail.
    pub context: &'static str,
}

impl RasError {
    /// Construct a recoverable error.
    #[must_use]
    pub const fn recoverable(code: ErrorCode, context: &'static str) -> Self {
        Self {
            code,
            recoverable: true,
            context,
        }
    }

    /// Construct a fatal error.
    #[must_use]
    pub const fn fatal(code: ErrorCode, context: &'static str) -> Self {
        Self {
            code,
            recoverable: false,
            context,
        }
    }
}

impl core::fmt::Display for RasError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({})", self.code, self.context)
    }
}

impl std::error::Error for RasError {}

/// Reliable control-channel message set (a protobuf `oneof` once codegen lands).
///
/// Transport-scoped only — no grant/lease payloads live here; those ride as opaque bytes in
/// [`ControlMsg::AuthEnvelope`]. Feedback is content-free (counters/timing, never pixels).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ControlMsg {
    /// Session-open handshake: agreed protocol version + feature flags.
    Hello {
        /// Negotiated protocol version.
        protocol_version: u32,
    },
    /// Host → controller: active stream parameters (wire projection of `ras_media::StreamConfig`).
    StreamConfig(StreamConfigWire),
    /// Controller → host: request a fresh IDR (PLI-style). Canonical keyframe request.
    KeyframeRequest(KeyframeRequest),
    /// Controller → host: periodic content-free decoder feedback feeding ABR + resync.
    Feedback(DecoderFeedback),
    /// Phase-2 slot: opaque access-request / consent bytes; empty in Phase 1.
    AuthEnvelope {
        /// Opaque payload; carries no meaning in Phase 1.
        payload: Bytes,
    },
    /// Graceful teardown with a stable reason code.
    Bye {
        /// Reason.
        code: ErrorCode,
    },
    /// Controller → host: remote-pointer position for a "look here" overlay on the host. **Not OS
    /// input** — a purely visual cursor; nothing reaches the host's input system.
    Pointer(PointerUpdate),
    /// Controller → host: request the single OS-input **control lease** (Phase 3, ADR-069). Carries
    /// the capabilities the controller wants; the host clamps to `grant ∩ policy ∩ consent` and never
    /// trusts this list as authority (Inv 15). Escalation past the session grant is refused.
    ControlRequest {
        /// Requested capability identifiers (bounded — [`MAX_CAPABILITIES`] × [`MAX_CAPABILITY_LEN`]).
        capabilities: Vec<String>,
    },
    /// Host → controller: the lease was granted. Host-signed for the future privileged-input-helper
    /// split (S4); MVP enforcement reads the host's own live state, not this token (ADR-069).
    ControlGranted {
        /// The lease identifier the controller echoes on every [`InputEnvelope`].
        lease_id: [u8; 16],
        /// The session generation at issuance; a mismatch on any input is rejected (Inv 5).
        generation: u32,
        /// The capabilities actually granted (⊆ requested); bounded as in [`ControlMsg::ControlRequest`].
        capabilities: Vec<String>,
        /// Absolute expiry (`UnixMillis`); never past the session grant's expiry.
        expires_at: u64,
        /// Opaque host signature over the lease claims (forward-compat; not trusted as authority yet).
        signature: Bytes,
    },
    /// Host → controller: the lease was revoked / transferred away / denied, with a reason code.
    ControlRevoked {
        /// Reason (`ConsentDenied`, `SessionRevoked`, `LeaseInvalid`, …).
        code: ErrorCode,
    },
    /// Controller → host: one OS-input event, bound to the lease that authorizes it (Phase 3). Every
    /// field is re-checked host-side, per message, before anything reaches the OS input sink (Inv 15).
    Input(InputEnvelope),
    /// Host → controller: the host's OS cursor **shape**, sent out-of-band so the controller draws it
    /// client-side at zero latency instead of relying on the (laggy) video (Priority 2, ADR-073).
    /// Cached by `id`. `rgba` is top-down, exactly `width * height * 4` bytes. Display data, not input
    /// (outside Inv 6).
    CursorShape {
        /// Shape cache key — the controller keeps recently-seen shapes by id.
        id: u32,
        /// Hot-spot x within the image (`< width`).
        hotspot_x: u16,
        /// Hot-spot y within the image (`< height`).
        hotspot_y: u16,
        /// Image width in pixels (`1..=`[`MAX_CURSOR_DIM`]).
        width: u16,
        /// Image height in pixels (`1..=`[`MAX_CURSOR_DIM`]).
        height: u16,
        /// Top-down RGBA pixels, exactly `width * height * 4` bytes.
        rgba: Bytes,
    },
    /// Host → controller: reuse an already-sent [`ControlMsg::CursorShape`] by `id` (no RGBA resend).
    CursorCached {
        /// The cache key of a previously-transmitted shape.
        id: u32,
    },
    /// Host → controller: the OS cursor is currently hidden — draw nothing.
    CursorHidden,
    /// Push clipboard **text** to the peer (ADR-076). An **explicit** user action — never auto-synced
    /// and (the load-bearing rule) never auto-**pasted**: the receiver only populates the OS clipboard,
    /// it does not inject a paste keystroke, which severs the clipboard-hijack→RCE chain (Reverse-RDP /
    /// RustDesk CVE class). Direction is implicit in the role: controller→host requires the
    /// host-enforced `clipboard.write` capability, host→controller the `clipboard.read` capability
    /// ([`crate::codec`] never decides authority — see `ras_policy::clipboard_push_allowed`, Inv 15).
    /// Content-bearing — the payload is a secret (passwords get copied); [`Redacted`] keeps it out of
    /// every `Debug`/log (Inv 8). Bounded by [`MAX_CLIPBOARD_BYTES`].
    ClipboardText {
        /// The UTF-8 clipboard text. Redacted in `Debug`; bounded by [`MAX_CLIPBOARD_BYTES`].
        text: Redacted,
    },
    /// In-session **chat** text between the two consented peers (ADR-082). Bidirectional; a received
    /// `ChatMessage` is always *from the remote peer*. This is **base session communication**, not a
    /// privileged behavior — it touches no OS/input/screen surface, so it carries no capability (a live
    /// session already required consent). Content-bearing — chat text is a secret in the Inv-8 sense
    /// (users paste anything), so the payload is [`Redacted`] (its `Debug` prints only a byte count, so
    /// it can never leak through a log/trace line) and it is **never** logged or audited-as-content.
    /// Bounded by [`MAX_CHAT_BYTES`]; an oversized message is refused, never truncated.
    ChatMessage {
        /// The UTF-8 chat text. Redacted in `Debug`; bounded by [`MAX_CHAT_BYTES`].
        text: Redacted,
    },
    /// Controller → host: a request to push a file to a **catalogued** drop target (ADR-086/087 file
    /// transfer). Carries **no path** — only the target name, a leaf `filename`, and the `size`. The host
    /// runs `ras_policy::authorize_file_push` (target-in-catalogue + `file.push.<target>` capability +
    /// safe-leaf filename + size cap) and gets per-transfer local consent, replying [`Self::FileAccept`]
    /// or [`Self::FileReject`]. The filename is **not** a secret (it is a chosen name, not content), but
    /// it is bounded ([`MAX_FILE_NAME`]) and only ever used after `validate_filename` proves it a safe
    /// leaf. (The chunk-streaming + `O_NOFOLLOW` write are a follow-up.)
    FileOffer {
        /// The catalogued drop-target name (bounded by [`MAX_FILE_TARGET`]).
        target: String,
        /// The bare leaf filename the host will validate + resolve (bounded by [`MAX_FILE_NAME`]).
        filename: String,
        /// Declared file size in bytes (checked against the target's cap).
        size: u64,
    },
    /// Host → controller: the file offer was authorized **and** locally consented — the host is ready to
    /// receive (Inv 1). (Byte streaming is a follow-up.)
    FileAccept,
    /// Host → controller: the file offer was refused, with a stable reason code (unknown target,
    /// capability denied, unsafe filename, too large, extension denied, or consent denied). Content-free.
    FileReject {
        /// Why the push was refused.
        code: ErrorCode,
    },
    /// Controller → host: one sequential chunk of an **accepted** file transfer's bytes (ADR-090). Only
    /// valid after a [`Self::FileAccept`]; the host writes it (via `O_NOFOLLOW`) to the resolved
    /// destination and rejects the transfer if the running total exceeds the offered `size`. Bounded by
    /// [`MAX_FILE_CHUNK`]. Not a secret in the Inv-8 sense (a file the local user agreed to receive), but
    /// bounded to cap the per-message DoS surface.
    FileChunk {
        /// The chunk bytes (`≤ MAX_FILE_CHUNK`).
        data: Bytes,
    },
    /// Controller → host: the accepted file transfer's bytes are all sent. The host finalizes the write
    /// iff the received total equals the offered `size`, else aborts (Inv: no partial/oversized file).
    FileComplete,
}

/// A UTF-8 secret whose `Debug` prints only its byte length, never its content — so it physically
/// cannot leak through a `#[derive(Debug)]` log line, `tracing` field, or crash dump (Invariant 8).
/// Used for clipboard text, chat text ([`ControlMsg::ChatMessage`]), and typed Unicode
/// ([`InputAction::TextInput`]) — every content-bearing wire field.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(pub String);

impl core::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "<redacted {} bytes>", self.0.len())
    }
}

impl Redacted {
    /// The wrapped secret. Only call at the point the content is genuinely needed (setting the OS
    /// clipboard) — never to log or format it.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

/// Maximum clipboard-text payload (bytes). Sits comfortably under [`MAX_CONTROL_FRAME`] so a
/// maximal clipboard still fits one framed control message with protobuf headroom. Oversized
/// clipboards are **refused**, never truncated (truncation would silently corrupt the paste).
pub const MAX_CLIPBOARD_BYTES: usize = 768 * 1024;

/// Maximum chat-message payload (bytes). Chat is short prose; a small bound keeps it well under
/// [`MAX_CONTROL_FRAME`] and caps the per-message DoS surface. Oversized messages are **refused**,
/// never truncated.
pub const MAX_CHAT_BYTES: usize = 4 * 1024;

/// Maximum length (bytes) of a [`ControlMsg::FileOffer`] leaf filename. Matches the OS `NAME_MAX`-class
/// bound the host's `validate_filename` enforces; an over-long name is a malformed offer.
pub const MAX_FILE_NAME: usize = 255;
/// Maximum length (bytes) of a file drop-target name.
pub const MAX_FILE_TARGET: usize = 128;
/// Maximum size (bytes) of one [`ControlMsg::FileChunk`]. Sits under [`MAX_CONTROL_FRAME`] with headroom;
/// larger files are sent as many chunks. An oversized chunk is a malformed message.
pub const MAX_FILE_CHUNK: usize = 256 * 1024;

/// Maximum cursor image dimension (pixels) on either axis — a DoS guard. Real cursors are ≤ 32×32,
/// up to ~128 on HiDPI; 256 is generous headroom. A larger dimension is a malformed message.
pub const MAX_CURSOR_DIM: u32 = 256;
/// Maximum cursor RGBA payload (bytes) = `MAX_CURSOR_DIM² × 4`. A longer payload is malformed.
pub const MAX_CURSOR_BYTES: usize = (MAX_CURSOR_DIM as usize) * (MAX_CURSOR_DIM as usize) * 4;

/// Upper bound on the number of capability identifiers in a [`ControlMsg::ControlRequest`] /
/// [`ControlMsg::ControlGranted`] list — a DoS guard; the catalogue is far smaller than this.
pub const MAX_CAPABILITIES: usize = 64;
/// Upper bound on a single capability identifier's length (bytes). Catalogue ids are dotted ASCII.
pub const MAX_CAPABILITY_LEN: usize = 64;
/// Upper bound on a [`InputAction::TextInput`] payload (bytes). Unicode entry is short bursts, never
/// bulk — a longer payload is a malformed message. Content-bearing: never logged (Invariant 8).
pub const MAX_TEXT_INPUT: usize = 256;

/// One OS-input event, bound to the control lease that authorizes it (Phase 3, ADR-067).
///
/// The `generation`/`lease_id`/`seq` are **claims** the host matches against its own authoritative
/// state; they are never trusted as authority (ADR-069, Inv 15). Rides the reliable, ordered control
/// stream so clicks/keys never drop or reorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEnvelope {
    /// The lease this event claims to act under (echoed from [`ControlMsg::ControlGranted`]).
    pub lease_id: [u8; 16],
    /// The generation this event claims; must equal the host's current generation, else rejected.
    pub generation: u32,
    /// Strictly-increasing per lease; the host rejects `seq ≤ last_seen` (replay / reorder guard).
    pub seq: u64,
    /// The normalized action to inject (the closed set — Inv 6).
    pub action: InputAction,
}

/// The closed set of OS-input actions (Invariant 6 — never a shell command, path, OS-API name, or
/// keysym string). Coordinates are normalized fixed-point `0..=65535` (= `0.0..=1.0`) of `display_id`'s
/// logical bounds; the **host** maps them to device pixels after authorization (the controller never
/// sends pixels).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputAction {
    /// Move the OS pointer to a normalized position on a display.
    PointerMove {
        /// Target display id (matches a `CaptureGeometry` display).
        display_id: u32,
        /// Horizontal `0..=65535` = left..right of `display_id`.
        nx: u16,
        /// Vertical `0..=65535` = top..bottom of `display_id`.
        ny: u16,
        /// The capture-geometry layout version this coordinate was computed against.
        layout_version: u32,
    },
    /// Press or release a pointer button at a normalized position.
    PointerButton {
        /// Target display id.
        display_id: u32,
        /// Horizontal `0..=65535`.
        nx: u16,
        /// Vertical `0..=65535`.
        ny: u16,
        /// The capture-geometry layout version this coordinate was computed against.
        layout_version: u32,
        /// Which button.
        button: PointerButton,
        /// `true` = press, `false` = release.
        down: bool,
    },
    /// Scroll by notched deltas (clamped `i16`).
    PointerWheel {
        /// Horizontal notches (right positive).
        dx: i16,
        /// Vertical notches (down positive).
        dy: i16,
    },
    /// Move the OS pointer by a **relative** pixel delta from its current position (ADR-087, §3.6). For
    /// trackpad/touch controllers where an absolute tap is unusable (a phone has no on-screen cursor to
    /// place). No `display_id` / `layout_version`: relative motion is display-independent, so it needs no
    /// capture geometry. Bounded `i16` px per event (a fast swipe is several events). Same `pointer.move`
    /// capability as [`Self::PointerMove`] — it is still cursor movement, gated identically (Inv 15).
    PointerMoveRelative {
        /// Horizontal delta in pixels (right positive).
        dx: i16,
        /// Vertical delta in pixels (down positive).
        dy: i16,
    },
    /// Press or release a **physical** key by USB-HID usage (layout-independent), never a keysym.
    KeyEvent {
        /// USB-HID usage id (Keyboard/Keypad page).
        hid_usage: u16,
        /// `true` = press, `false` = release.
        down: bool,
        /// Modifier bitset (platform-neutral); host maps to OS modifier flags.
        modifiers: u8,
    },
    /// Layout-independent Unicode text entry (the separate `keyboard.text` capability). Never used for
    /// shortcuts. Bounded by [`MAX_TEXT_INPUT`]; **control characters are rejected at decode** (no
    /// terminal-escape / NUL smuggling). Content-bearing plaintext (passwords/PII), so the payload is a
    /// [`Redacted`] — its `Debug` prints only a byte count, so a typed secret can never leak through a
    /// log/trace/crash line (Invariant 8); `.reveal()` only at the OS-injection boundary, never to log.
    TextInput {
        /// The UTF-8 text to type. Redacted in `Debug`; bounded by [`MAX_TEXT_INPUT`]; no control chars.
        utf8: Redacted,
    },
    /// Release every key/button the host currently holds down — key-state cleanup on
    /// transfer/disconnect/stop. Always permitted (it only *clears* state).
    ReleaseAllKeys,
    /// Authoritative CapsLock/NumLock **state** (not an edge): the host slaves its lock keys to these
    /// so case/keypad output matches the controller. Forwarding a lock-key *edge* between two
    /// independently-stated machines guarantees drift — this carries the desired state instead.
    SetLockState {
        /// Desired CapsLock state (`true` = on).
        caps_lock: bool,
        /// Desired NumLock state (`true` = on).
        num_lock: bool,
    },
}

/// The closed set of injectable pointer buttons (Invariant 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PointerButton {
    /// Primary (left) button.
    Left,
    /// Secondary (right) button.
    Right,
    /// Tertiary (middle / wheel) button.
    Middle,
}

/// The controller's pointer position over the shared screen (controller → host), for a **remote
/// pointer** overlay. Purely visual — never OS input (no click, no keyboard), so it sits outside the
/// input-injection invariants. Coordinates are normalized fixed-point fractions of the shared frame
/// (`0..=65535` maps to `0.0..=1.0`) so they survive any resolution/scaling on either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerUpdate {
    /// Horizontal position: `0..=65535` = left..right edge of the shared frame.
    pub x: u16,
    /// Vertical position: `0..=65535` = top..bottom edge.
    pub y: u16,
    /// Whether the pointer is on-screen (`false` → hide the overlay cursor).
    pub visible: bool,
}

/// Canonical keyframe/IDR request (controller → host).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeRequest {
    /// Last `frame_id` the controller has, for host-side coalescing (avoid redundant IDRs).
    pub since_frame: FrameId,
    /// Why the controller needs a keyframe.
    pub reason: KeyframeReason,
}

/// The one keyframe-reason enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyframeReason {
    /// First frame of a new session / late-join subscriber.
    StreamStart,
    /// Gap in `frame_id`s beyond FEC recovery.
    UnrecoverableLoss,
    /// WebCodecs decoder went terminal; a new decoder needs an IDR.
    DecoderReset,
    /// Resolution/codec/monitor change enacted this frame.
    ConfigChanged,
    /// Optional bounded host safety refresh.
    PeriodicRefresh,
}

/// The one content-free feedback message (controller → host, reliable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderFeedback {
    /// Highest contiguous `frame_id` successfully decoded.
    pub last_decoded_frame: FrameId,
    /// Frames dropped since the last report (metrics + ABR).
    pub frames_dropped: u32,
    /// Controller-measured decode/presentation latency estimate (µs); trend only.
    pub decode_latency_us: u32,
    /// Present when the decoder needs a fresh IDR.
    pub keyframe_request: Option<KeyframeRequest>,
}

/// Wire projection of `ras_media::StreamConfig` for the control channel (protobuf-encoded).
///
/// Structurally identical to the in-memory config; separate only because the codec is serialized
/// as its derived string form while the in-memory type stays an enum. `color`/`video_transport`
/// are encoded as small integer tags to avoid a dependency on `ras-media`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfigWire {
    /// Fully-qualified WebCodecs codec string, e.g. `"avc1.4D401F"`.
    pub codec: String,
    /// Output width (px).
    pub width: u32,
    /// Output height (px).
    pub height: u32,
    /// Target frames/sec.
    pub fps: u32,
    /// Target average bitrate (bits/sec), CBR.
    pub target_bitrate_bps: u32,
    /// Color-space tag: 0 = BT.709 limited, 1 = BT.709 full.
    pub color: u8,
    /// Video-transport tag: 0 = per-frame stream, 1 = datagram+FEC.
    pub video_transport: u8,
}

/// Upper bound on a controller's self-declared display name on the bootstrap channel (bytes).
///
/// A DoS/abuse guard on top of [`MAX_CONTROL_FRAME`]: the display name is attacker-controlled and
/// only ever shown in the consent prompt, so it never needs to be large. The codec rejects a longer
/// name as a malformed message. It is untrusted UI text — never a path, command, or capability.
pub const MAX_DISPLAY_NAME: usize = 128;

/// Bootstrap-ALPN (`casual-ras/bootstrap/1`) message set — the Phase-2 authorization handshake.
///
/// **Deliberately separate from [`ControlMsg`]** (the session-phase channel): the two run on
/// different ALPNs, and keeping their vocabularies in distinct types means a bootstrap message can
/// never be injected into the session control stream, or vice versa, at the type level (a
/// security-positive separation — Inv 9 authenticates identity, the *host* authorizes).
///
/// Payloads that carry a signed, canonically-encoded artifact — the `AccessRequest` and the PASETO
/// grant — ride as **opaque [`Bytes`]** owned by `ras-grant`; this crate frames them (exactly like
/// [`ControlMsg::AuthEnvelope`]) and never interprets them. Ids are raw 32-byte Ed25519 public keys;
/// `tier` is a small projection of `ras_identity::AssuranceTier` (this crate does not depend on
/// `ras-identity`). Nothing here is authoritative: it is a request/response envelope, not a decision.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum BootstrapMsg {
    /// Controller → host: open the bootstrap phase with the controller's protocol version.
    ClientHello {
        /// Controller's protocol major version.
        protocol_version: u32,
    },
    /// Host → controller: the host identity + the assurance tier it may advertise (Inv 16).
    HostHello {
        /// Host's 32-byte Ed25519 public key.
        host_id: [u8; 32],
        /// Assurance-tier tag: `0..=3` = Tier0..Tier3 (projection of `AssuranceTier`).
        tier: u8,
    },
    /// Controller → host: first-contact pairing for an unknown controller (local user accepts).
    PairingRequest {
        /// Controller's 32-byte Ed25519 public key (its identity).
        controller_id: [u8; 32],
        /// Untrusted, length-bounded UI text shown in the pairing prompt — never a path/command.
        display_name: String,
        /// Controller's 32-byte Ed25519 public key offered for pairing.
        pubkey: [u8; 32],
        /// Opaque signature over the pairing challenge; verified by `ras-identity`.
        signature: Bytes,
    },
    /// Host → controller: the local user's pairing accept/deny (Inv 1).
    PairingDecision {
        /// `true` ⇒ paired and stored in `trusted_controllers`.
        accepted: bool,
    },
    /// Controller → host: the signed, canonically-encoded `AccessRequest` (opaque; `ras-grant` owns
    /// the encoding + embedded controller signature).
    AccessRequest {
        /// Canonical signed AccessRequest bytes.
        canonical: Bytes,
    },
    /// Host → controller: the consent outcome — exactly one of allowed/denied.
    AccessDecision(AccessOutcome),
    /// Controller → host: abandon the in-flight request.
    CancelRequest,
    /// Either side: a typed protocol error on the bootstrap channel.
    ProtocolError {
        /// Stable reason code.
        code: ErrorCode,
    },
}

/// The host's decision on an `AccessRequest` (payload of [`BootstrapMsg::AccessDecision`]).
///
/// Exactly one outcome: on Allow the controller receives an opaque PASETO grant to present on the
/// session channel; on refuse it receives only a content-free reason code (never *why* the human
/// declined). The codec enforces the exactly-one invariant on the wire.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AccessOutcome {
    /// Consent granted: opaque PASETO v4.public grant bytes to present in [`ControlMsg::AuthEnvelope`].
    Allowed {
        /// Opaque PASETO grant.
        grant: Bytes,
    },
    /// Consent refused (deny or timeout): a stable, content-free reason code.
    Denied {
        /// Reason (e.g. [`ErrorCode::ConsentDenied`]).
        code: ErrorCode,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_have_stable_strings() {
        assert_eq!(ErrorCode::SignatureInvalid.as_str(), "SIGNATURE_INVALID");
        assert_eq!(ErrorCode::Internal.as_str(), "INTERNAL_ERROR");
        assert_eq!(ErrorCode::CapabilityDenied.to_string(), "CAPABILITY_DENIED");
    }

    #[test]
    fn error_code_numeric_round_trips_and_matches_the_wire_numbering() {
        // Every variant round-trips through its stable numeric id, and the ids match the proto enum.
        for (code, n) in [
            (ErrorCode::InvalidMessage, 1u16),
            (ErrorCode::CapabilityDenied, 8),
            (ErrorCode::SessionRevoked, 11),
            (ErrorCode::NormalClosure, 18),
        ] {
            assert_eq!(code.to_code(), n);
            assert_eq!(ErrorCode::from_code(n), Some(code));
        }
        // Exhaustive round-trip over the whole range; 0 and out-of-range are rejected (fail-closed).
        for n in 1..=18u16 {
            assert_eq!(ErrorCode::from_code(n).map(ErrorCode::to_code), Some(n));
        }
        assert_eq!(ErrorCode::from_code(0), None);
        assert_eq!(ErrorCode::from_code(19), None);
    }

    #[test]
    fn ras_error_carries_recoverability() {
        let e = RasError::recoverable(ErrorCode::CaptureFailed, "sck restart");
        assert!(e.recoverable);
        assert_eq!(e.code, ErrorCode::CaptureFailed);
        let f = RasError::fatal(ErrorCode::Internal, "bug");
        assert!(!f.recoverable);
    }

    #[test]
    fn control_msg_is_constructible() {
        let m = ControlMsg::KeyframeRequest(KeyframeRequest {
            since_frame: 41,
            reason: KeyframeReason::UnrecoverableLoss,
        });
        assert!(matches!(m, ControlMsg::KeyframeRequest(_)));
    }
}
