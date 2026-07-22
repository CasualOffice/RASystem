//! Casual RAS — the single unified desktop app (ADR-062).
//!
//! One binary, **both roles**, chosen at runtime from a home screen:
//!
//! - **Connect** (viewer) — dial a host's connection ticket over iroh and render its screen with
//!   WebCodecs. Platform-independent: the viewing side only decodes, so this works on
//!   macOS/Linux/Windows. Commands: [`connect_to_host`], [`disconnect`], [`send_pointer`],
//!   [`request_keyframe`].
//! - **Share** (agent) — capture *this* screen, hold the viewer in the handshake until the local user
//!   clicks **Allow** (real consent — Invariant 1), stream it over iroh, and draw the viewer's remote
//!   pointer on a transparent overlay. macOS-only for now (needs `ras-media-macos`); on other
//!   platforms [`start_sharing`] reports it isn't available yet. Commands: [`start_sharing`],
//!   [`stop_sharing`], [`respond_consent`].
//!
//! Built with `ras-core` `default-features = false`, so the `insecure-no-auth` `AllowAllValidator` is
//! **not even linked** — there is no way to skip consent. No pixels ever cross JSON IPC: encoded
//! access units ride a **binary** Tauri `Channel` (24-byte `RAS1` header + Annex-B).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::{Arc, Mutex};

use ras_core::frame_channel::encode_frame_blob;
use ras_core::transport::Endpoint;
use ras_core::{ControllerSession, CoreError, FrameSink, LifecycleStream, PushResult};
use ras_media::{EncodedFrame, StreamConfig};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager, State};

mod secure_window;

/// Framing magic for the one-shot stream-config message (`"RCFG"` big-endian, sent little-endian).
/// Distinguishes the JSON config blob from the `RAS1` frame blobs on the same channel.
const CONFIG_MAGIC: u32 = u32::from_be_bytes(*b"RCFG");

// ─── Shared app state ────────────────────────────────────────────────────────────────────────────

/// Everything the Tauri commands share. Built in `.setup()` (the Share role's consent gate needs the
/// `AppHandle`).
struct AppState {
    /// A live **viewer** session (Connect role) — dial a host's ticket over iroh.
    session: Mutex<Option<ConnectedSession>>,
    /// The **sharer** side (Share role).
    share: ShareState,
    /// The local user's durable **contacts** address book (ADR-092): saved peers reachable by identity
    /// (no ticket), and the deny-by-default gate for messages/requests. `None` only if the data dir
    /// could not be opened at startup (contacts disabled, everything else still works).
    contacts: Option<Arc<ras_identity::FileContactBook>>,
    /// The always-on iroh endpoint bound with the persistent identity seed in `.setup()`. Shared so
    /// both the always-on `run_share` accept loop and a later `start_sharing` reuse the SAME endpoint —
    /// a second same-seed endpoint would collide in iroh discovery (ADR-094/098). `None` where there is
    /// no capture backend (no always-on loop) or the bind failed.
    endpoint: Option<Arc<ras_transport_iroh::Endpoint>>,
    /// The iroh-gossip actor hosted on the always-on endpoint, driving live **presence** dots for saved
    /// contacts (ADR-094). `None` ⇒ presence is silently disabled (gossip bind/spawn failed, or no
    /// capture backend so no always-on endpoint) — **contacts + sessions run fully regardless**
    /// (FAIL-SAFE). Gossip must never break connectivity.
    gossip: Option<iroh_gossip::net::Gossip>,
    /// This machine's persistent identity (the contact id / endpoint id, ADR-092/093), read once at
    /// startup and reused for every presence topic + beacon signing. `None` if the identity seed was
    /// unavailable (⇒ presence off).
    me: Option<ras_identity::ContactId>,
    /// The persistent-identity keystore for signing presence beacons (ADR-094). Shared into every
    /// per-contact presence task. `None` ⇒ presence off.
    presence_ks: Option<Arc<dyn ras_identity::KeyStore>>,
    /// The live presence state, fed by the per-contact gossip tasks and polled to emit UI updates.
    /// Pure + clock-free (the poll loop passes `now`). Present even when `gossip` is `None` (just never
    /// updated), so callers never branch on its existence.
    presence: Arc<Mutex<ras_signal::presence::PresenceTracker>>,
    /// One running presence task per saved contact, keyed by contact id. Dropping a handle aborts that
    /// contact's beacon loop and leaves its topic — so add/remove/block spawn/drop a single task. `Arc`
    /// so a spawned subscribe task can insert its handle once `spawn_presence` resolves.
    presence_handles: Arc<
        Mutex<std::collections::HashMap<ras_identity::ContactId, ras_signal::net::PresenceHandle>>,
    >,
}

/// A connected viewer session: the controller + the iroh endpoint that must outlive it.
struct ConnectedSession {
    _endpoint: Arc<Endpoint>,
    controller: Arc<ControllerSession>,
    /// Monotonic per-session input sequence (Phase 3): the host rejects any `seq ≤ last_seen`, so this
    /// must strictly increase across every `Input` this viewer sends under its lease.
    input_seq: std::sync::atomic::AtomicU64,
}

/// Drain the viewer's `ras-core` lifecycle stream and surface reconnection state to the UI (task #22).
/// The controller re-dials on transport loss (ADR-091); the viewer needs to see it. Emits a **string**
/// `conn-status` (mirrors the host's `share-status`): `reconnecting` on `Suspended`, `connected` on
/// `Resumed`, `ended` on teardown. The task ends when the stream closes (session dropped on disconnect).
async fn drain_viewer_lifecycle(mut events: LifecycleStream, app: tauri::AppHandle) {
    use ras_core::LifecycleEvent;
    while let Some(ev) = events.recv().await {
        match ev {
            LifecycleEvent::Suspended { .. } => {
                let _ = app.emit("conn-status", "reconnecting");
            }
            LifecycleEvent::Resumed => {
                let _ = app.emit("conn-status", "connected");
            }
            LifecycleEvent::ConnectionQuality { sample } => {
                let _ = app.emit(
                    "conn-quality",
                    ConnQualityPayload {
                        path: format!("{:?}", sample.path),
                        rtt_ms: sample.rtt_ms,
                        loss_pct: sample.loss_pct,
                        fps: sample.delivered_fps,
                        kbps: sample.bandwidth_kbps,
                    },
                );
            }
            // Chat received from the host (ADR-082). `.reveal()` here is the sanctioned display
            // boundary — the only place the redacted text is read; it is never logged (Inv 8).
            LifecycleEvent::ChatMessage { text } => {
                let _ = app.emit("chat-message", text.reveal().to_string());
                // Gentle attention only (no focus steal) + a content-free notification — never the
                // message text (Inv 8).
                alert_user(
                    &app,
                    false,
                    "Casual RAS — new message",
                    "You have a new chat message.",
                );
            }
            // The host pushed clipboard and we set it on this viewer's OS clipboard (host→controller,
            // ADR-076). Content-free: emit only the byte count (Inv 8).
            LifecycleEvent::ClipboardApplied { len } => {
                let _ = app.emit("clipboard-received", len);
            }
            // Annotation markup drawn by the HOST (Share role), mirrored onto this viewer's video
            // overlay (ADR-097). Display data only — not OS input, no capability, like the visual
            // pointer. The webview's annotation module renders it. Content-safe (never logged, Inv 8).
            LifecycleEvent::RemoteAnnotation(op) => {
                let _ = app.emit("remote-annotation", annotate_payload(&op));
            }
            // The host authorized + consented to our file offer (ADR-086): start streaming chunks. The
            // UI listens for `file-accepted` to begin `file_chunk`. Content-free.
            LifecycleEvent::FileTransferAccepted => {
                let _ = app.emit("file-accepted", ());
            }
            // The host refused our file offer (unknown target / capability withheld / unsafe filename /
            // too large / consent denied): stop streaming. Surface the stable reason code (content-free).
            LifecycleEvent::FileTransferRejected { code } => {
                let _ = app.emit("file-rejected", format!("{code:?}"));
            }
            LifecycleEvent::SessionEnded { .. }
            | LifecycleEvent::Disconnected { .. }
            | LifecycleEvent::Revoked { .. } => {
                let _ = app.emit("conn-status", "ended");
                // Audio (if it was flowing) stops with the session — clear the "AUDIO SHARED" indicator.
                let _ = app.emit("audio-inactive", ());
                break;
            }
            _ => {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bring the app to the local user's attention for an inbound event (industry-standard "someone is
/// requesting…" UX). The local user owns every authorization decision (Invariant 1), so a request
/// must never sit unnoticed behind another window.
///
/// - `demand = true` (access / control / file requests — a decision is needed *now*): raise, show
///   and focus the main window and flag it Critical (dock bounce / taskbar flash), plus a system
///   notification.
/// - `demand = false` (a chat message — informational): a gentle attention flag + notification, with
///   **no focus steal** (norm: don't yank the user out of what they're doing for a chat line).
///
/// `title`/`body` are **content-free** (Invariant 8): never chat text, clipboard, typed text, keys,
/// or pixels. A filename may appear (a filename is shown to the user by design, not a secret).
fn alert_user(app: &tauri::AppHandle, demand: bool, title: &str, body: &str) {
    if let Some(win) = app.get_webview_window("main") {
        if demand {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            let _ = win.request_user_attention(Some(tauri::UserAttentionType::Critical));
        } else {
            let _ = win.request_user_attention(Some(tauri::UserAttentionType::Informational));
        }
    }
    notify(app, title, body);
}

/// Best-effort native system notification. No-ops (never errors) if notification permission was not
/// granted. Bodies must be content-free (Invariant 8).
fn notify(app: &tauri::AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Wall-clock ms since the Unix epoch, for the Phase-2 authorization timestamps (request/grant
/// validity windows). A pre-epoch clock saturates to 0 (fail-closed: everything reads "not yet
/// valid" rather than silently valid).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The name of the single file-transfer drop target this app exposes (ADR-086). A controller pushes to
/// `"drop"`; the required per-target capability is `file.push.drop` (see [`ras_policy::file`]). Kept flat
/// (no `.`) so the capability namespace stays a single leaf.
const FILE_DROP_TARGET: &str = "drop";

/// The Phase-3 default policy **plus the two clipboard capabilities and the file-push capability**
/// (ADR-076/079/086). `phase3_default_policy()` *withholds* `clipboard.read`/`clipboard.write` and
/// declares no `file.push.*` target (all default OFF), which would make every clipboard/file push refuse
/// fail-closed. Adding them here only **raises the ceiling** so those flows *can* happen: the grant is
/// still `requested ∩ policy ∩ ceiling`, the local Allow/Deny consent gate still runs, the per-transfer
/// file consent still prompts (Inv 1), and the host-side per-message capability gate (Inv 15) still
/// enforces every message + the pure `authorize_file_push` still validates the leaf filename (the danger
/// channel stays core-enforced). This merely lets consent *be able* to grant clipboard/file — nothing
/// else about authorization changes. Used on both ends: the viewer requests it (so its `requested` set
/// includes them) and the host issues against it (so its ceiling admits them).
fn capabilities_with_extras() -> ras_core::policy::CapabilitySet {
    let mut policy = ras_core::policy::phase3_default_policy();
    policy.insert(ras_core::policy::CLIPBOARD_READ.to_string());
    policy.insert(ras_core::policy::CLIPBOARD_WRITE.to_string());
    // The per-target file-push cap (`file.push.drop`). Consent + per-message gate + filename validation
    // still apply — this only lifts the coarse grant ceiling so a push *can* be authorized at all.
    policy.insert(ras_core::policy::file::file_push_capability(
        FILE_DROP_TARGET,
    ));
    // Output audio (`audio.listen`, ADR-077) — recognized-but-withheld by `phase3_default_policy`, so it
    // must be added to the ceiling for a grant to be *able* to carry it. It is only actually consented when
    // the host opted in (see `consented_capabilities`), and the audio pump is gated host-side on the
    // granted capability + the transport's audio plane (Inv 15) — this only lifts the ceiling.
    policy.insert(ras_core::policy::AUDIO_LISTEN.to_string());
    policy
}

/// The capabilities a plain view-**Allow** is treated as consenting to (passed as the `consented`
/// argument to grant issuance — `granted = requested ∩ policy ∩ consented`). This is screen view + the
/// OS-input caps (each still gated by the SEPARATE control-lease consent, a held lease, AND the
/// per-message gate) + the file-push cap (gated by per-transfer file consent). **Clipboard is included
/// only when `clipboard_allowed` is true** — i.e. the host explicitly opted in — because clipboard has
/// NO second gate: the capability alone authorizes a controller→host clipboard write (the RustDesk /
/// Reverse-RDP injection class ADR-076 severs). So clipboard must reflect a real, disclosed choice, not
/// ride silently on a view-Allow (Inv 1 the user's actual choice, Inv 2 not defaulted-on, Inv 7 honest).
///
/// **Audio is included only when `audio_allowed` is true** — same reasoning as clipboard (ADR-077).
/// Output audio (host system audio → viewer) has no second per-message gate the way input/file do, so its
/// `audio.listen` capability alone authorizes the host to be heard. It must reflect a real, disclosed
/// opt-in (the "AUDIO SHARED" indicator, Inv 7), not ride silently on a view-Allow. When withheld the host
/// never fetches an audio sink and the `ras-core` audio pump never runs (Inv 15).
fn consented_capabilities(
    clipboard_allowed: bool,
    audio_allowed: bool,
) -> ras_core::policy::CapabilitySet {
    let mut caps = ras_core::policy::phase3_default_policy();
    caps.insert(ras_core::policy::file::file_push_capability(
        FILE_DROP_TARGET,
    ));
    if clipboard_allowed {
        caps.insert(ras_core::policy::CLIPBOARD_READ.to_string());
        caps.insert(ras_core::policy::CLIPBOARD_WRITE.to_string());
    }
    if audio_allowed {
        caps.insert(ras_core::policy::AUDIO_LISTEN.to_string());
    }
    caps
}

// ─── Connect role (viewer) ─────────────────────────────────────────────────────────────────────

/// A [`FrameSink`] forwarding the decoded-stream config + each encoded access unit to the webview
/// over the binary Tauri channel. `configure` sends one `RCFG` JSON blob; `push` sends `RAS1` frame
/// blobs. No pixels ever cross JSON IPC. Platform-independent.
struct ChannelFrameSink {
    channel: Channel<InvokeResponseBody>,
}

impl FrameSink for ChannelFrameSink {
    fn configure(&self, config: &StreamConfig) -> Result<(), CoreError> {
        let codec = config.codec.webcodecs_string(config.width, config.height);
        let json = serde_json::json!({
            "codec": codec,
            "width": config.width,
            "height": config.height,
            "fps": config.fps,
        })
        .to_string();
        let mut blob = Vec::with_capacity(4 + json.len());
        blob.extend_from_slice(&CONFIG_MAGIC.to_le_bytes());
        blob.extend_from_slice(json.as_bytes());
        // A closed channel (webview gone) isn't fatal; the session tears down via stop.
        let _ = self.channel.send(InvokeResponseBody::Raw(blob));
        Ok(())
    }

    fn push(&self, frame: EncodedFrame) -> PushResult {
        match self
            .channel
            .send(InvokeResponseBody::Raw(encode_frame_blob(&frame)))
        {
            Ok(()) => PushResult::Sent,
            Err(_) => PushResult::Dropped,
        }
    }
}

/// Magic for one output-audio blob on the audio channel (`"RAU1"` big-endian, sent little-endian). A
/// self-describing header lets the webview configure its WebCodecs `AudioDecoder` from the first packet
/// without a separate config message. Distinct from the video channel's `RAS1`/`RCFG`.
const AUDIO_MAGIC: u32 = u32::from_be_bytes(*b"RAU1");

/// An [`ras_core::AudioOutput`] forwarding each received Opus packet to the webview over a binary Tauri
/// channel for WebCodecs playback (ADR-077). `push` is sync + non-blocking (a closed channel just drops —
/// audio is best-effort, never fatal). No audio content is ever logged (Inv 8). Emits an `audio-active`
/// event on the first packet so the UI can show the "AUDIO SHARED"/playing indicator.
struct AppAudioOutput {
    channel: Channel<InvokeResponseBody>,
    app: tauri::AppHandle,
    /// Set on the first packet — flips the UI indicator on exactly once, best-effort.
    active: std::sync::atomic::AtomicBool,
}

impl ras_core::AudioOutput for AppAudioOutput {
    fn push(&self, packet: ras_media::EncodedAudio) {
        // First packet ⇒ audio is now flowing: light the "AUDIO SHARED" indicator (best-effort, once).
        if !self.active.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let _ = self.app.emit("audio-active", ());
        }
        // Self-describing blob: [ magic "RAU1" | sample_rate:u32-le | channels:u8 | seq:u64-le | opus ].
        // The webview parses this to configure its `AudioDecoder` and order/gap-detect packets. Opus has no
        // keyframes, so any packet is independently decodable once the decoder is warmed.
        let cfg = packet.config;
        let opus = &packet.data;
        let mut blob = Vec::with_capacity(4 + 4 + 1 + 8 + opus.len());
        blob.extend_from_slice(&AUDIO_MAGIC.to_le_bytes());
        blob.extend_from_slice(&cfg.sample_rate_hz.to_le_bytes());
        blob.push(cfg.channels);
        blob.extend_from_slice(&packet.seq.to_le_bytes());
        blob.extend_from_slice(opus);
        let _ = self.channel.send(InvokeResponseBody::Raw(blob));
    }
}

/// Dial a host's **connection ticket** over iroh and render its screen. Works on any platform (the
/// viewer only decodes).
///
/// Phase 2 is a **two-phase** dial from **one** endpoint (so both connections share the controller's
/// authenticated endpoint id, which the sender-constraint binds): first the **bootstrap ALPN** —
/// prove identity, send a signed `AccessRequest`, and receive a PASETO grant (or a denial) — then the
/// **session ALPN**, presenting that grant in the `AuthEnvelope`. No pixels flow until the host has
/// validated the grant against this endpoint.
#[tauri::command]
async fn connect_to_host(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    ticket: String,
    on_frame: Channel<InvokeResponseBody>,
    on_audio: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_core::grant::{fresh_id, AccessRequest, MAX_REQUEST_TTL_MS};
    use ras_core::identity::SoftwareKeyStore;
    use ras_core::transport::EndpointAddr;
    use ras_core::{ControllerSessionConfig, IrohSessionTransport};
    use ras_protocol::{AccessOutcome, BootstrapMsg, PROTOCOL_VERSION};

    log::info!("connect: dialing host (two-phase: bootstrap → grant → session)");
    // Tear down any prior viewer session first.
    let _ = disconnect(state.clone()).await;

    let target = EndpointAddr::from_ticket(ticket.trim()).map_err(|e| e.to_string())?;
    // The controller's persistent identity (ADR-092/093): same stable seed drives both the endpoint and
    // the app-identity keystore, so a host can save THIS machine as a contact and reach it by name too
    // (bidirectional). `None` ⇒ ephemeral fallback.
    let id_seed = identity_seed(&app);
    // Dial from an EPHEMERAL endpoint (ADR-098): this outgoing viewer connection must NOT collide with
    // our always-on ACCEPT endpoint, which is bound with our persistent identity so saved contacts can
    // reach us by name. Two endpoints with the same identity would clash in iroh discovery. The grant
    // binds to this dial endpoint's id (the endpoint iroh authenticates); our CONTACT identity
    // (`controller_id`, used for the host to save us back) stays the persistent key via the signing
    // keystore below — so bidirectional contacts still work.
    let endpoint = Arc::new(Endpoint::bind().await.map_err(|e| e.to_string())?);
    let my_endpoint_id = endpoint.id().0;
    let ks = match id_seed {
        Some(seed) => SoftwareKeyStore::from_seed(seed),
        None => SoftwareKeyStore::generate().map_err(|e| e.to_string())?,
    };

    // ── Bootstrap phase (casual-ras/bootstrap/1): request access, receive a grant. ──
    let boot_conn = endpoint
        .connect_bootstrap(&target)
        .await
        .map_err(|e| e.to_string())?;
    let mut boot = boot_conn.bootstrap().await.map_err(|e| e.to_string())?;
    boot.send(BootstrapMsg::ClientHello {
        protocol_version: PROTOCOL_VERSION,
    })
    .await
    .map_err(|e| e.to_string())?;
    let host_id = match boot.recv().await.map_err(|e| e.to_string())? {
        BootstrapMsg::HostHello { host_id, .. } => host_id,
        _ => return Err("unexpected bootstrap reply from host".into()),
    };
    let now = now_ms();
    let request = AccessRequest::signed(
        &ks,
        fresh_id().map_err(|e| e.to_string())?,
        PROTOCOL_VERSION,
        host_id,
        "Casual RAS viewer".to_string(),
        my_endpoint_id,
        // Request the Phase-3 capability set (plus clipboard) so the grant's ceiling can include OS
        // input and clipboard. This only sets what the controller *may later ask for*; actually
        // injecting/clipboarding still needs the host's ceiling ∩ policy + consent (Invariant 1) — the
        // grant is the coarse gate, the lease/per-message gate the fine one.
        capabilities_with_extras(),
        "remote support".to_string(),
        now,
        now + MAX_REQUEST_TTL_MS,
        fresh_id().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    boot.send(BootstrapMsg::AccessRequest {
        canonical: request.encode(),
    })
    .await
    .map_err(|e| e.to_string())?;
    let grant = match boot.recv().await.map_err(|e| e.to_string())? {
        BootstrapMsg::AccessDecision(AccessOutcome::Allowed { grant }) => grant,
        BootstrapMsg::AccessDecision(AccessOutcome::Denied { code }) => {
            log::warn!("connect: host denied access (code {code:?})");
            return Err(format!("access denied ({code})"));
        }
        _ => return Err("unexpected bootstrap decision from host".into()),
    };
    drop(boot);
    drop(boot_conn); // close the bootstrap connection; the endpoint lives on for the session dial

    // ── Session phase (casual-ras/1): present the grant, then render. ──
    let session = endpoint.connect(&target).await.map_err(|e| e.to_string())?;
    // Enable ADR-091 resume over real iroh: on a transport drop the controller re-dials this same
    // target on the session ALPN and re-presents the grant (host re-validates it through the unchanged
    // validator), so a WiFi hiccup / NAT rebind / relay switch resumes the session instead of killing
    // it. Without this the resume path is dead code over iroh (MAJOR real-run blocker).
    let transport = Arc::new(
        IrohSessionTransport::new(endpoint.clone(), session)
            .with_reconnect_controller(target.clone()),
    );
    let controller = Arc::new(ControllerSession::new(
        ControllerSessionConfig::new(target).with_grant(grant),
        transport,
    ));

    let events = controller.connect().await.map_err(|e| e.to_string())?;
    controller
        .attach_renderer(Arc::new(ChannelFrameSink { channel: on_frame }))
        .await
        .map_err(|e| e.to_string())?;

    // Attach the OS-clipboard write backend so a clipboard the host pushes (host→controller, gated
    // host-side on `clipboard.read`, Inv 15) is **set** on this viewer's OS clipboard (never pasted —
    // no-auto-paste rule, ADR-076). Best-effort: if the platform clipboard can't be opened we just skip
    // it — a failed clipboard must never fail the connect.
    if let Ok(sink) = ras_clipboard::ArboardClipboardSink::new() {
        controller.attach_clipboard_sink(Arc::new(sink));
    }

    // Attach the output-audio sink (ADR-077): each Opus packet the host sends (only if it granted
    // `audio.listen`, gated host-side, Inv 15) is forwarded to the webview over `on_audio` for WebCodecs
    // playback. Harmless when no audio is granted — the output simply never receives a packet. Emits
    // `audio-active` on the first packet so the UI can show the "AUDIO SHARED" indicator (Inv 7). Contents
    // are never logged (Inv 8); audio is live-only, never recorded (Inv 12).
    controller.attach_audio_output(Arc::new(AppAudioOutput {
        channel: on_audio,
        app: app.clone(),
        active: std::sync::atomic::AtomicBool::new(false),
    }));

    // Drain the lifecycle stream so reconnection (Suspended/Resumed) surfaces in the viewer UI (#22)
    // instead of being parked. The task ends when the stream closes (the session is dropped below on a
    // later disconnect). Emit an initial "connected" so the UI clears any stale banner.
    log::info!("connect: session live");
    let _ = app.emit("conn-status", "connected");
    tauri::async_runtime::spawn(drain_viewer_lifecycle(events, app));

    *lock(&state.session) = Some(ConnectedSession {
        _endpoint: endpoint,
        controller,
        input_seq: std::sync::atomic::AtomicU64::new(0),
    });
    Ok(())
}

// ─── Contacts (address book + ticketless connect, ADR-092/093) ─────────────────────────────────────

/// A contact as the UI sees it. Content-light (public key + label + timestamps + block flag); no secret.
#[derive(serde::Serialize)]
struct ContactDto {
    /// Hex-encoded Ed25519 public identity (= iroh EndpointId).
    id: String,
    /// Grouped Crockford-base32 verification code (shown next to the QR / for a verbal check).
    code: String,
    label: String,
    added_at: u64,
    last_seen_at: u64,
    blocked: bool,
}

impl From<ras_identity::Contact> for ContactDto {
    fn from(c: ras_identity::Contact) -> Self {
        ContactDto {
            id: hex_id(c.id.as_bytes()),
            code: ras_identity::contact_code(&c.id),
            label: c.label,
            added_at: c.added_at,
            last_seen_at: c.last_seen_at,
            blocked: c.blocked,
        }
    }
}

/// Lowercase-hex a 32-byte identity.
fn hex_id(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse a contact identity from either a **connection ticket** (`CASUALRAS1:…`, whose id we keep) or a
/// bare 64-char hex key. Fail-closed on anything else.
fn parse_contact_id(input: &str) -> Result<[u8; 32], String> {
    let input = input.trim();
    if let Ok(addr) = ras_core::transport::EndpointAddr::from_ticket(input) {
        return Ok(addr.id.0);
    }
    if input.len() == 64 {
        let mut out = [0u8; 32];
        for (i, chunk) in input.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16);
            let lo = (chunk[1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out[i] = (h as u8) << 4 | l as u8,
                _ => return Err("not a valid contact code or key".into()),
            }
        }
        return Ok(out);
    }
    Err("paste a contact's ticket or its 64-character key".into())
}

/// Handle to the contacts book, or a stable error if it could not be opened at startup.
fn contacts_of<'a>(
    state: &'a State<'_, AppState>,
) -> Result<&'a Arc<ras_identity::FileContactBook>, String> {
    state
        .contacts
        .as_ref()
        .ok_or_else(|| "contacts storage is unavailable".to_string())
}

/// Load (or create) this machine's **persistent Ed25519 identity seed** from the app data dir (ADR-092/
/// 093). A stable seed ⇒ a stable EndpointId, so a contact who saved this machine reaches it by name
/// across restarts, and the endpoint id == the contact id == the beacon/signal signing key. Returns
/// `None` if the data dir is unavailable — the caller then uses an ephemeral identity (a saved contact
/// would need a fresh ticket, degraded but never broken). The seed file is a raw 32-byte secret written
/// `0600` (on unix) by `load_or_create`; we read it back so the endpoint's secret key and the keystore's
/// public key are the same identity. It never crosses an IPC/log boundary (Inv 8).
fn identity_seed(app: &tauri::AppHandle) -> Option<[u8; 32]> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("identity.key");
    ras_core::identity::SoftwareKeyStore::load_or_create(&path).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// This machine's own shareable identity (ADR-092/093), so a peer can save you once and reach you by
/// name. `ticket` is an id-only connection ticket to hand out (paste into their "Add contact"); `code`
/// is the Crockford verification code to read aloud/compare against their QR.
#[derive(serde::Serialize)]
struct MyIdentity {
    ticket: String,
    code: String,
    id: String,
}

/// Return this machine's persistent contact identity (id-only ticket + verification code). No network,
/// no secret — the public identity only (Inv 8).
#[tauri::command]
fn my_identity(app: tauri::AppHandle) -> Result<MyIdentity, String> {
    use ras_core::identity::{KeyStore, SoftwareKeyStore};
    let seed = identity_seed(&app).ok_or("identity storage is unavailable")?;
    let pk = SoftwareKeyStore::from_seed(seed).public_key();
    let cid = ras_identity::ContactId::from_bytes(pk);
    let ticket =
        ras_core::transport::EndpointAddr::new(ras_core::transport::EndpointId(pk)).to_ticket();
    Ok(MyIdentity {
        ticket,
        code: ras_identity::contact_code(&cid),
        id: hex_id(&pk),
    })
}

/// List all saved contacts for the address-book UI.
#[tauri::command]
fn list_contacts(state: State<'_, AppState>) -> Result<Vec<ContactDto>, String> {
    use ras_identity::ContactBook;
    let mut v: Vec<ContactDto> = contacts_of(&state)?
        .list()
        .into_iter()
        .map(ContactDto::from)
        .collect();
    v.sort_by_key(|c| c.label.to_lowercase());
    Ok(v)
}

/// Add (or relabel) a contact from a ticket or key + a human label. A re-add preserves the original
/// pairing age and never silently unblocks (Inv 1). Returns the saved contact.
#[tauri::command]
fn add_contact(
    state: State<'_, AppState>,
    input: String,
    label: String,
) -> Result<ContactDto, String> {
    use ras_identity::{Contact, ContactBook, ContactId};
    let id = ContactId::from_bytes(parse_contact_id(&input)?);
    let label = label.trim();
    let label = if label.is_empty() {
        ras_identity::contact_code(&id)
            .split('-')
            .next()
            .unwrap_or("Contact")
            .to_string()
    } else {
        label
            .chars()
            .take(ras_identity::MAX_CONTACT_LABEL)
            .collect()
    };
    let now = now_ms();
    let book = contacts_of(&state)?;
    book.upsert(Contact {
        id,
        label,
        added_at: now,
        last_seen_at: now,
        blocked: false,
    });
    // Start live presence for the newly-saved contact (best-effort; a gossip failure just leaves the
    // dot grey — never breaks the add). No-op if presence is disabled or already running for this id.
    spawn_presence_for(&state, id);
    book.get(&id)
        .map(ContactDto::from)
        .ok_or_else(|| "failed to save contact".into())
}

/// Remove a contact entirely (kill-switch).
#[tauri::command]
fn remove_contact(state: State<'_, AppState>, id: String) -> Result<(), String> {
    use ras_identity::{ContactBook, ContactId};
    let cid = ContactId::from_bytes(parse_contact_id(&id)?);
    contacts_of(&state)?.remove(&cid);
    // Stop presence for the removed contact: drop the handle (aborts its beacon loop + leaves the
    // topic) and clear its cached state so its dot goes dark. Best-effort; guarded for presence-off.
    stop_presence_for(&state, &cid);
    Ok(())
}

/// Block a contact: they can no longer be reached, nor deliver a message / access-request (Inv 1).
#[tauri::command]
fn set_contact_blocked(
    state: State<'_, AppState>,
    id: String,
    blocked: bool,
) -> Result<(), String> {
    use ras_identity::{ContactBook, ContactId};
    let cid = ContactId::from_bytes(parse_contact_id(&id)?);
    let book = contacts_of(&state)?;
    if blocked {
        book.block(&cid);
        // Blocked ⇒ stop beaconing to / observing them, and clear their dot (contacts-only, Inv 1).
        stop_presence_for(&state, &cid);
    } else {
        book.unblock(&cid);
        // Unblocked ⇒ resume live presence (best-effort; no-op if already running or presence off).
        spawn_presence_for(&state, cid);
    }
    Ok(())
}

// ─── Gossip presence (ADR-094) ─────────────────────────────────────────────────────────────────────

/// Beacon cadence: broadcast an "online" beacon every 10s.
const PRESENCE_BEACON_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// Staleness window: a contact is "online" only if a beacon was seen within 30s (3× the beacon
/// interval, so a couple of dropped beacons don't flap the dot — matches presence.rs guidance).
const PRESENCE_FRESHNESS_MS: u64 = 30_000;
/// How often the poll loop diffs the tracker and emits `presence` changes to the UI (well under the
/// freshness window so a state change surfaces promptly without flooding the webview).
const PRESENCE_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Content-free presence update pushed to the UI: which saved contact just changed online/offline.
/// No secret, no message text — just a public id + a bool (Inv 8).
#[derive(serde::Serialize, Clone)]
struct PresencePayload {
    #[serde(rename = "contactId")]
    contact_id: String,
    online: bool,
}

/// Start the pairwise presence loop for one saved contact, storing the handle keyed by id. FAIL-SAFE:
/// every step degrades to "no dot" rather than erroring — a missing gossip actor, an unavailable
/// identity, an already-running task, or a failed gossip subscribe all just leave that contact's dot
/// grey. Never touches session/share state.
fn spawn_presence_for(state: &AppState, contact: ras_identity::ContactId) {
    // Presence disabled (no gossip / no identity) ⇒ nothing to do; contacts still work (FAIL-SAFE).
    let (Some(gossip), Some(me), Some(ks), Some(book)) = (
        state.gossip.as_ref(),
        state.me,
        state.presence_ks.as_ref(),
        state.contacts.as_ref(),
    ) else {
        return;
    };
    // Don't beacon to ourselves, and never re-spawn a task that's already running for this id.
    if contact == me {
        return;
    }
    match state.presence_handles.lock() {
        Ok(handles) if handles.contains_key(&contact) => return,
        Ok(_) => {}
        Err(_) => return,
    }
    // The contact id IS its endpoint id / pubkey (ADR-092/093), so it bootstraps the pairwise topic.
    let Ok(peer) = iroh::EndpointId::from_bytes(contact.as_bytes()) else {
        return;
    };
    let topic = ras_signal::presence::pairwise_topic(&me, &contact);
    let gossip = gossip.clone();
    let ks: Arc<dyn ras_identity::KeyStore> = ks.clone();
    let book: Arc<dyn ras_identity::ContactBook> = book.clone();
    let tracker = state.presence.clone();
    let handles = state.presence_handles.clone();
    let params = ras_signal::net::PresenceParams {
        beacon_every: PRESENCE_BEACON_EVERY,
        freshness_ms: PRESENCE_FRESHNESS_MS,
    };
    // `spawn_presence` awaits the initial gossip subscribe, so drive it on the async runtime and stash
    // the handle once it resolves. A failed subscribe simply never inserts a handle (dot stays grey).
    tauri::async_runtime::spawn(async move {
        match ras_signal::net::spawn_presence(&gossip, topic, peer, ks, book, tracker, params).await
        {
            Ok(handle) => {
                if let Ok(mut map) = handles.lock() {
                    // Guard against a racing add/unblock inserting first: keep the earliest, drop ours.
                    map.entry(contact).or_insert(handle);
                }
            }
            // FAIL-SAFE: presence for this one contact just doesn't start; everything else runs.
            Err(e) => log::warn!("presence: subscribe failed for a contact: {:?}", e.code),
        }
    });
}

/// Stop presence for one contact: abort its beacon loop (drop the handle) and clear its cached
/// presence so the dot goes dark. Best-effort; no-op if presence is off or no task was running.
fn stop_presence_for(state: &AppState, contact: &ras_identity::ContactId) {
    if let Ok(mut map) = state.presence_handles.lock() {
        map.remove(contact); // Drop = abort beacon loop + leave topic.
    }
    if let Ok(mut t) = state.presence.lock() {
        t.forget(contact);
    }
}

/// The presence poll loop: every [`PRESENCE_POLL_EVERY`] read the tracker, diff against the last
/// emitted state, and emit a `presence` event to the UI **only for contacts whose online/offline state
/// CHANGED** (so the webview isn't flooded). Runs for the app's lifetime once gossip is up. FAIL-SAFE:
/// it only *reads* the tracker — it can never touch session/share state or break connectivity.
async fn presence_poll_loop(
    app: tauri::AppHandle,
    tracker: Arc<Mutex<ras_signal::presence::PresenceTracker>>,
) {
    use std::collections::HashMap;
    // Last emitted online-state per contact, so we emit only transitions.
    let mut last: HashMap<ras_identity::ContactId, bool> = HashMap::new();
    loop {
        tokio::time::sleep(PRESENCE_POLL_EVERY).await;
        let now = now_ms();
        let online: std::collections::HashSet<ras_identity::ContactId> = match tracker.lock() {
            Ok(t) => t
                .online_now(now, PRESENCE_FRESHNESS_MS)
                .into_iter()
                .collect(),
            Err(_) => continue,
        };
        // Emit newly-online transitions.
        for id in &online {
            if last.get(id) != Some(&true) {
                last.insert(*id, true);
                let _ = app.emit(
                    "presence",
                    PresencePayload {
                        contact_id: hex_id(id.as_bytes()),
                        online: true,
                    },
                );
            }
        }
        // Emit newly-offline transitions (previously true, now absent from the online set).
        let now_offline: Vec<ras_identity::ContactId> = last
            .iter()
            .filter(|(id, &was_online)| was_online && !online.contains(id))
            .map(|(id, _)| *id)
            .collect();
        for id in now_offline {
            last.insert(id, false);
            let _ = app.emit(
                "presence",
                PresencePayload {
                    contact_id: hex_id(id.as_bytes()),
                    online: false,
                },
            );
        }
    }
}

/// Snapshot of the currently-online saved contacts (hex ids), so the UI can paint initial dots on
/// entering the contacts view before the next diff tick. Content-free (public ids only, Inv 8).
/// Returns an empty list when presence is disabled.
#[tauri::command]
fn list_online(state: State<'_, AppState>) -> Vec<String> {
    let now = now_ms();
    match state.presence.lock() {
        Ok(t) => t
            .online_now(now, PRESENCE_FRESHNESS_MS)
            .into_iter()
            .map(|id| hex_id(id.as_bytes()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Connect to a **saved contact by identity — no ticket** (ADR-093). Builds an id-only target that iroh
/// discovery resolves when the contact is online, then runs the exact same consent-gated two-phase
/// connect as a pasted ticket (the host still validates the grant + asks its user to Allow — a contact
/// removes the ticket step, never the consent, Inv 1). Refuses an unknown or blocked identity
/// (contacts-only).
#[tauri::command]
async fn connect_to_contact(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    on_frame: Channel<InvokeResponseBody>,
    on_audio: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_identity::{ContactBook, ContactId};
    let bytes = parse_contact_id(&id)?;
    let cid = ContactId::from_bytes(bytes);
    // Contacts-only: never dial an identity the user hasn't saved (or has blocked).
    if !contacts_of(&state)?.is_active_contact(&cid) {
        return Err("not a saved contact (or blocked)".into());
    }
    // An id-only ticket: no direct addrs / relay — iroh discovery resolves the live address by id.
    let target = ras_core::transport::EndpointAddr::new(ras_core::transport::EndpointId(bytes));
    connect_to_host(app, state, target.to_ticket(), on_frame, on_audio).await
}

/// Send a signed **out-of-session text message** to a saved contact over the signal ALPN (ADR-095).
///
/// **Online-only, best-effort.** iroh-gossip/relays have no store-and-forward, so a dial to an offline
/// contact fails; there is NO durable mailbox (explicitly out of scope). A failed delivery is surfaced
/// as a benign "not delivered — contact appears offline" state, never a false success. Do not over-claim
/// delivery.
///
/// Security: contacts-only, enforced twice — the receiver's `verify_signed` gate AND this pre-dial guard
/// (a stranger's / blocked contact's message is never even dialed). Signed with the persistent contact
/// identity, so the receiver's contacts check keys on the right pubkey. The body is a secret (Inv 8):
/// wrapped in `Redacted`, passed straight to `send_signal` (which never logs it) — NEVER formatted or
/// logged here, not even in an error.
#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    contact_id: String,
    text: String,
) -> Result<(), String> {
    use ras_core::identity::SoftwareKeyStore;
    use ras_identity::{ContactBook, ContactId};
    use ras_protocol::Redacted;
    use ras_signal::SignalPayload;

    let bytes = parse_contact_id(&contact_id)?;
    let cid = ContactId::from_bytes(bytes);
    // Contacts-only pre-dial guard (defense-in-depth; the receiver enforces it too). Never dial an
    // identity the user hasn't saved or has blocked.
    if !contacts_of(&state)?.is_active_contact(&cid) {
        return Err("not a saved contact (or blocked)".into());
    }
    // Sign with the persistent contact identity so the receiver's contacts-only check keys on the right
    // pubkey (endpoint id == contact id == signing key).
    let seed = identity_seed(&app).ok_or("identity unavailable")?;
    let ks = SoftwareKeyStore::from_seed(seed);
    // The always-on endpoint, published into state at bind. Fails cleanly before it is bound.
    let ep = state
        .endpoint
        .clone()
        .ok_or("not reachable yet — try again in a moment")?;
    // An id-only target: iroh discovery resolves the live address by id when the contact is online.
    let peer =
        iroh::EndpointId::from_bytes(&bytes).map_err(|_| "invalid contact id".to_string())?;
    let target = iroh::EndpointAddr::new(peer);
    // Build the payload — the body goes straight in wrapped as `Redacted`; NEVER formatted/logged (Inv 8).
    // `MAX_SIGNAL_TEXT` is enforced by `encode_signed` inside `send_signal`; an over-long body returns
    // `InvalidMessage`, which we map to a stable "too long" string WITHOUT echoing the text.
    let payload = SignalPayload::DirectMessage {
        issued_at: now_ms(),
        text: Redacted(text),
    };
    ras_signal::net::send_signal(ep.iroh(), target, &ks, &payload)
        .await
        .map_err(|e| {
            if e.code == ras_protocol::ErrorCode::InvalidMessage {
                "message too long".to_string()
            } else {
                // Offline / not-acknowledged: honest best-effort, never a false success (no mailbox).
                "not delivered — contact appears offline".to_string()
            }
        })
}

/// Forward the viewer's pointer position to the host for its remote-pointer overlay ("look here").
/// Normalized `0..=65535`. Best-effort + non-blocking (latency-first). Not OS input — a purely visual
/// cursor. No-op unless a viewer session is live.
#[tauri::command]
async fn send_pointer(
    state: State<'_, AppState>,
    x: u16,
    y: u16,
    visible: bool,
) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_pointer(x, y, visible);
    }
    Ok(())
}

/// Build a core [`ras_protocol::AnnotateOp`] from the webview's JSON args, for the viewer-side
/// [`annotate`] command (Connect role). `op` is `"stroke"` (with `tool`/`color`/`points`), `"undo"`,
/// or `"clear"`. `points` are normalized `0..=65535` `[x, y]` and bounded here (`MAX_ANNOT_POINTS`) —
/// the wire type re-bounds them on decode too.
fn build_annotate_op(
    op: &str,
    tool: u8,
    color: u32,
    points: Vec<[u16; 2]>,
) -> ras_protocol::AnnotateOp {
    use ras_protocol::{AnnotTool, AnnotateOp};
    match op {
        "undo" => AnnotateOp::Undo,
        "clear" => AnnotateOp::Clear,
        _ => {
            let tool = match tool {
                1 => AnnotTool::Highlighter,
                2 => AnnotTool::Arrow,
                3 => AnnotTool::Rect,
                _ => AnnotTool::Pen,
            };
            let points: Vec<(u16, u16)> = points
                .into_iter()
                .take(ras_protocol::MAX_ANNOT_POINTS)
                .map(|p| (p[0], p[1]))
                .collect();
            AnnotateOp::Stroke {
                tool,
                color_rgb: color & 0x00ff_ffff,
                points,
            }
        }
    }
}

/// Send annotation markup to render on the **host's** overlay (ADR-097), from the Connect (viewer)
/// side. Display data only — not OS input, no capability, like the visual pointer. `points` are
/// normalized `0..=65535` `[x, y]`, bounded here and on the host.
#[tauri::command]
async fn annotate(
    state: State<'_, AppState>,
    op: String,
    tool: u8,
    color: u32,
    points: Vec<[u16; 2]>,
) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    let Some(c) = controller else { return Ok(()) };
    c.send_annotation(build_annotate_op(&op, tool, color, points));
    Ok(())
}

/// Send an in-session **chat** message over whichever session is currently active (ADR-082). Chat is
/// base session comms — **no capability** (a live session already required consent, so gating would be
/// security-theater). Prefers a live viewer (Connect) session; falls back to an active share (Share)
/// session. The text is a secret (Inv 8): it is passed straight to `ras-core` (which redacts it on the
/// wire) and is **never** logged/formatted here. Err if no session is active.
#[tauri::command]
async fn send_chat(state: State<'_, AppState>, text: String) -> Result<(), String> {
    // Viewer (Connect) session takes precedence when present.
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_chat(text);
        return Ok(());
    }
    // Otherwise, if a share (Share/host) session is live, send from the host side.
    let host = lock(&state.share.session)
        .as_ref()
        .and_then(|s| s.host.clone());
    if let Some(h) = host {
        h.send_chat(text);
        return Ok(());
    }
    Err("no active session".into())
}

/// Push `text` to the peer's clipboard over the active session (ADR-076). Over a viewer (Connect)
/// session this is a controller→host push (gated host-side on `clipboard.write`); over a share (Share)
/// session it is a host→controller push (gated on `clipboard.read`). The receiver **sets** its OS
/// clipboard, never auto-pastes. The text is a secret (Inv 8): handed straight to `ras-core` (redacted
/// on the wire) and never logged/formatted here. Err if no session is active.
#[tauri::command]
async fn send_clipboard(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_clipboard_text(text);
        return Ok(());
    }
    let host = lock(&state.share.session)
        .as_ref()
        .and_then(|s| s.host.clone());
    if let Some(h) = host {
        h.send_clipboard_text(text);
        return Ok(());
    }
    Err("no active session".into())
}

// ─── Connect role: file transfer (push to the host, ADR-086/090) ─────────────────────────────────
//
// The controller-side of the signed-catalogue file push. The viewer offers a file to the host's single
// `"drop"` target (leaf filename + size — **never a path**; the host resolves the destination), then, once
// the host emits `file-accepted` (its per-transfer consent said Allow), streams the bytes as chunks and
// signals completion. All the danger-channel safety (filename validation, sandbox resolution, O_NOFOLLOW
// write, size cap, per-message capability gate) lives host-side in ras-policy/ras-files/ras-core — this
// side only carries the offer + bytes. Never log file contents (Inv 8); a byte count is fine.

/// Begin a file push to the host's `"drop"` target: offer the leaf `filename` + `size`. The host
/// authorizes it (catalogue + capability + safe-leaf validation) and prompts its local user (Inv 1),
/// replying with a `file-accepted` or `file-rejected` event. The UI waits for `file-accepted` before
/// calling [`file_chunk`]. `filename` is a **leaf name only** — any path is rejected host-side. Err if no
/// viewer session is live.
#[tauri::command]
async fn file_begin(state: State<'_, AppState>, filename: String, size: u64) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_file_offer(FILE_DROP_TARGET.to_string(), filename, size);
        return Ok(());
    }
    Err("no active session".into())
}

/// Stream one chunk of the **accepted** transfer (call only after `file-accepted`). Bytes over
/// `MAX_FILE_CHUNK` are dropped by ras-core (split them). The host aborts if the running total exceeds the
/// offered size. Err if no viewer session is live.
#[tauri::command]
async fn file_chunk(state: State<'_, AppState>, bytes: Vec<u8>) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_file_chunk(bytes::Bytes::from(bytes));
        return Ok(());
    }
    Err("no active session".into())
}

/// Signal that every chunk of the accepted transfer has been sent; the host finalizes the write iff the
/// received total equals the offered size (else it aborts — no partial/oversized file). Err if no viewer
/// session is live.
#[tauri::command]
fn file_end(state: State<'_, AppState>) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_file_complete();
        return Ok(());
    }
    Err("no active session".into())
}

/// End the live viewer session (idempotent).
#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    use ras_core::StopReason;
    let session = lock(&state.session).take();
    if let Some(s) = session {
        s.controller.disconnect(StopReason::UserRequested).await;
    }
    Ok(())
}

/// Ask the host (via the control channel) for a fresh IDR — used by the webview once its decoder is
/// configured (infinite-GOP means the lone startup keyframe may predate it) and on any decoder reset.
#[tauri::command]
async fn request_keyframe(state: State<'_, AppState>) -> Result<(), String> {
    use ras_protocol::KeyframeReason;
    let c = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = c {
        let _ = c.request_keyframe(KeyframeReason::DecoderReset).await;
    }
    Ok(())
}

// ─── Connect role: OS-input control (Phase 3) ────────────────────────────────────────────────────

/// Request the OS-input control lease from the host (Phase 3). The host prompts its local user
/// (Invariant 1); on Allow it replies with a lease and the viewer's subsequent input is injected.
/// No-op unless a viewer session is live. Requesting is not controlling — input flows only once the
/// host has granted the lease (surfaced via `is_controlling`).
#[tauri::command]
async fn request_control(state: State<'_, AppState>) -> Result<(), String> {
    let c = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = c {
        c.request_control(vec![
            "pointer.move".into(),
            "pointer.click".into(),
            "pointer.scroll".into(),
            "keyboard.key".into(),
        ]);
    }
    Ok(())
}

/// Whether this viewer currently holds an OS-input lease (i.e. its input is being injected). The UI
/// polls this to reflect control state and to gate its input capture.
#[tauri::command]
async fn is_controlling(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(lock(&state.session)
        .as_ref()
        .and_then(|s| s.controller.current_lease())
        .is_some())
}

/// Stamp and forward one OS-input action under the held lease. No-op if no lease is held (the host
/// would reject it anyway — this just avoids the round-trip). The host re-checks lease/generation/seq/
/// capability per message (ADR-069): this is a claim, not authority.
fn send_input_action(state: &State<'_, AppState>, action: ras_protocol::InputAction) {
    let guard = lock(&state.session);
    if let Some(s) = guard.as_ref() {
        if let Some((lease_id, generation)) = s.controller.current_lease() {
            let seq = s
                .input_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            s.controller.send_input(ras_protocol::InputEnvelope {
                lease_id,
                generation,
                seq,
                action,
            });
        }
    }
}

/// Move the host's OS pointer to a normalized position (`0..=65535`) on the shared display.
#[tauri::command]
async fn input_pointer_move(state: State<'_, AppState>, nx: u16, ny: u16) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::PointerMove {
            display_id: 0,
            nx,
            ny,
            layout_version: 0,
        },
    );
    Ok(())
}

/// Press or release a pointer button (`"left"`/`"right"`/`"middle"`) at a normalized position.
#[tauri::command]
async fn input_pointer_button(
    state: State<'_, AppState>,
    nx: u16,
    ny: u16,
    button: String,
    down: bool,
) -> Result<(), String> {
    let button = match button.as_str() {
        "right" => ras_protocol::PointerButton::Right,
        "middle" => ras_protocol::PointerButton::Middle,
        _ => ras_protocol::PointerButton::Left,
    };
    send_input_action(
        &state,
        ras_protocol::InputAction::PointerButton {
            display_id: 0,
            nx,
            ny,
            layout_version: 0,
            button,
            down,
        },
    );
    Ok(())
}

/// Scroll by notched deltas (clamped `i16`).
#[tauri::command]
async fn input_pointer_wheel(state: State<'_, AppState>, dx: i16, dy: i16) -> Result<(), String> {
    send_input_action(&state, ras_protocol::InputAction::PointerWheel { dx, dy });
    Ok(())
}

/// Press or release a physical key by USB-HID usage (+ modifier bitset: 1 shift, 2 ctrl, 4 alt,
/// 8 cmd). Never a keysym — the host maps HID → OS keycode (Inv 6).
#[tauri::command]
async fn input_key(
    state: State<'_, AppState>,
    hid_usage: u16,
    down: bool,
    modifiers: u8,
) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::KeyEvent {
            hid_usage,
            down,
            modifiers,
        },
    );
    Ok(())
}

/// Slave the host's CapsLock/NumLock to the controller's authoritative *state* (not key edges — see
/// ADR-074). Gated host-side on `keyboard.key`, so a pointer-only lease can't flip a lock (Inv 15).
#[tauri::command]
async fn input_set_lock_state(
    state: State<'_, AppState>,
    caps_lock: bool,
    num_lock: bool,
) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::SetLockState {
            caps_lock,
            num_lock,
        },
    );
    Ok(())
}

// ─── Share role (agent) ──────────────────────────────────────────────────────────────────────────

/// The sharer side. `session` is `Some` while a share is active; `consent` is the local Allow/Deny
/// gate reached by both the running share task and the `respond_consent` command.
struct ShareState {
    session: Mutex<Option<ShareSession>>,
    consent: Arc<LocalConsent>,
}

/// A running share: the `watch` sender used to tear the whole share down, plus (once a viewer is
/// actually connected) a handle to the live host session for out-of-band chat/clipboard sends.
struct ShareSession {
    stop: tokio::sync::watch::Sender<bool>,
    /// The live host session while a viewer is connected (Share role), erased behind [`ShareControl`]
    /// so `AppState` needn't be generic over the per-platform capture/encoder backends. `None` before
    /// a viewer connects and after they leave (`serve_one` sets/clears it). Only exposes the two
    /// content-carrying-but-redacting send APIs — never the whole session.
    host: Option<Arc<dyn ShareControl>>,
}

/// The subset of the Share-role host session the app needs out-of-band: send chat / clipboard to the
/// connected viewer. Object-safe so a `HostSession<C, E>` of any backend pair can be stored in the
/// non-generic [`AppState`]. Both take an owned `String` the session redacts on the wire (Inv 8) — the
/// text is never logged here.
trait ShareControl: Send + Sync {
    fn send_chat(&self, text: String);
    fn send_clipboard_text(&self, text: String);
}

impl<C, E> ShareControl for ras_core::HostSession<C, E>
where
    C: ras_media::ScreenCaptureBackend + Send + 'static,
    E: ras_media::VideoEncoderBackend + Send + 'static,
{
    fn send_chat(&self, text: String) {
        ras_core::HostSession::send_chat(self, text);
    }
    fn send_clipboard_text(&self, text: String) {
        ras_core::HostSession::send_clipboard_text(self, text);
    }
}

/// The local-consent gate. Implements `ras-core`'s `GrantValidator`: when a viewer requests access it
/// emits a `consent-request` and **blocks the session until the local user answers** (or a timeout
/// denies). No pixels flow before Allow. One viewer at a time, so a single pending slot suffices.
struct LocalConsent {
    app: tauri::AppHandle,
    pending: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
    /// A separate pending slot for the Phase-3 **control-lease** consent (Invariant 1): requesting OS
    /// input is a distinct, higher-stakes act than viewing, so it re-prompts on its own channel.
    pending_control: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
    /// A separate pending slot for a **file-push** transfer consent (ADR-086, Invariant 1): file transfer
    /// is the danger channel, so each push re-prompts the local user on its own channel — even a
    /// catalogued, capability-granted push needs a live Allow. One transfer at a time.
    pending_file: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
    /// The `(filename, size)` of the most recently offered file, stashed at the consent prompt so the
    /// share loop can populate the `file-received` event payload when the corresponding
    /// `FileTransferAccepted` lifecycle event arrives (the lifecycle event itself is content-free). A
    /// filename is shown to the user, not a secret (Inv 8).
    last_file_offer: Mutex<Option<(String, u64)>>,
    /// Whether the local user has opted in to **clipboard sharing** with a connecting viewer. Default
    /// **false**: clipboard has no per-message consent gate (unlike control + file), so its capability
    /// is only placed in the issued grant's `consented` set when this is true — otherwise a plain
    /// view-Allow would silently authorize controller→host clipboard writes (Inv 1/2/7, ADR-076). Set
    /// on the Share screen before a viewer connects (the grant's capabilities are fixed at issue time).
    clipboard_allowed: std::sync::atomic::AtomicBool,
    /// Whether the local user has opted in to **output-audio sharing** (host system audio → viewer,
    /// ADR-077). Default **false**. Output audio is display-side only (no mic, live-only, never recorded —
    /// Inv 12) and is always disclosed by an Inv-7 "AUDIO SHARED" indicator, but it is kept opt-in and
    /// consistent with the clipboard toggle: the `audio.listen` capability is placed in the issued grant's
    /// `consented` set only when this is true. With it withheld the host never fetches an audio sink and no
    /// audio is captured or sent (the `ras-core` audio pump is gated on the granted capability, Inv 15).
    /// Set on the Share screen before a viewer connects (the grant's capabilities are fixed at issue time).
    audio_allowed: std::sync::atomic::AtomicBool,
}

impl LocalConsent {
    fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            pending: Mutex::new(None),
            pending_control: Mutex::new(None),
            pending_file: Mutex::new(None),
            last_file_offer: Mutex::new(None),
            clipboard_allowed: std::sync::atomic::AtomicBool::new(false),
            audio_allowed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the local user has opted in to clipboard sharing (default false).
    fn clipboard_allowed(&self) -> bool {
        self.clipboard_allowed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set the clipboard-sharing opt-in (from the Share-screen toggle). Takes effect for the next
    /// viewer to connect (an already-issued grant's capabilities are immutable).
    fn set_clipboard_allowed(&self, allowed: bool) {
        self.clipboard_allowed
            .store(allowed, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the local user has opted in to output-audio sharing (default false).
    fn audio_allowed(&self) -> bool {
        self.audio_allowed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set the output-audio-sharing opt-in (from the Share-screen toggle). Takes effect for the next
    /// viewer to connect (an already-issued grant's capabilities are immutable).
    fn set_audio_allowed(&self, allowed: bool) {
        self.audio_allowed
            .store(allowed, std::sync::atomic::Ordering::SeqCst);
    }

    /// Deliver the local user's decision to a waiting `prompt`. Extra/late calls are no-ops.
    fn respond(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending).take() {
            let _ = tx.send(allow);
        }
    }

    /// Deliver the local user's decision to a waiting control-consent prompt. Late calls are no-ops.
    fn respond_control(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending_control).take() {
            let _ = tx.send(allow);
        }
    }

    /// Deliver the local user's decision to a waiting file-transfer consent prompt. Late calls are no-ops.
    fn respond_file(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending_file).take() {
            let _ = tx.send(allow);
        }
    }

    /// Prompt the local user (Invariant 1) and block until they answer, emitting `consent-request`
    /// with the requester's short identity. A 90 s silence **denies** (fail-closed) so a pending
    /// request can't hang the share forever. Returns `true` only on an explicit Allow.
    async fn prompt(&self, peer_short: String) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending) = Some(tx);
        let _ = self.app.emit("consent-request", peer_short);
        alert_user(
            &self.app,
            true,
            "Casual RAS — access request",
            "A viewer is asking to see this screen. Open Casual RAS to Allow or Deny.",
        );
        let allow = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), rx).await,
            Ok(Ok(true))
        );
        *lock(&self.pending) = None;
        let _ = self.app.emit("consent-closed", ());
        // Content-free security-relevant event (the decision, not the peer's content) — Inv 8.
        log::info!(
            "consent: view access {}",
            if allow { "ALLOWED" } else { "denied" }
        );
        allow
    }
}

/// Phase-3 control-lease consent (Invariant 1): when a connected viewer requests OS input, prompt the
/// local user on a distinct channel and return the consented subset (Allow ⇒ exactly what was asked,
/// Deny or a 90 s silence ⇒ empty = denied, fail-closed). The host clamps this again to grant ∩ policy.
#[async_trait::async_trait]
impl ras_core::ControlConsent for LocalConsent {
    async fn consent_to_control(
        &self,
        requested: &ras_core::policy::CapabilitySet,
    ) -> ras_core::policy::CapabilitySet {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending_control) = Some(tx);
        // Surface the human-readable requested caps so the panel can list what input is being asked for.
        let caps: Vec<String> = requested.iter().cloned().collect();
        let _ = self.app.emit("control-consent-request", caps);
        alert_user(
            &self.app,
            true,
            "Casual RAS — remote control request",
            "A viewer is asking to control this machine (keyboard & mouse). Open Casual RAS to Allow or Deny.",
        );
        let allow = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), rx).await,
            Ok(Ok(true))
        );
        *lock(&self.pending_control) = None;
        let _ = self.app.emit("control-consent-closed", ());
        if allow {
            requested.clone()
        } else {
            ras_core::policy::CapabilitySet::new()
        }
    }
}

/// Per-transfer **file-push** consent (ADR-086, Invariant 1): file transfer is the danger channel, so
/// even a catalogued, capability-granted push re-prompts the local user before any byte is written. The
/// request has already passed the pure host-side `authorize_file_push` (catalogue + capability + safe-leaf
/// filename validation + size cap) by the time this runs — this is the final human gate. Emits
/// `file-offer` with the leaf filename + size (a filename is shown to the user, not a secret; contents are
/// never touched here) and blocks until the local user answers. Deny or a 90 s silence ⇒ refuse
/// (fail-closed).
#[async_trait::async_trait]
impl ras_core::FileConsent for LocalConsent {
    async fn consent_to_file(&self, _target: &str, filename: &str, size: u64) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending_file) = Some(tx);
        // Stash the offer so the share loop can label the later `file-received` event (the accepted-
        // lifecycle event is content-free). A filename is not a secret (Inv 8).
        *lock(&self.last_file_offer) = Some((filename.to_string(), size));
        let _ = self.app.emit(
            "file-offer",
            FileOfferPayload {
                filename: filename.to_string(),
                size,
            },
        );
        // A filename is shown to the user by design (not a secret, Inv 8); file contents never are.
        alert_user(
            &self.app,
            true,
            "Casual RAS — incoming file",
            &format!("A viewer wants to send \"{filename}\" ({size} bytes). Open Casual RAS to Allow or Deny."),
        );
        let allow = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), rx).await,
            Ok(Ok(true))
        );
        *lock(&self.pending_file) = None;
        let _ = self.app.emit("file-offer-closed", ());
        allow
    }
}

/// A `ras_core::FileWriteSink` wrapping [`ras_files::SafeFileWriter`] (ADR-090). The `dest` is
/// **host-resolved** by ras-policy (a validated leaf inside the sandbox — never a controller path); the
/// underlying writer opens it with `O_NOFOLLOW | O_CREAT | O_EXCL`, so a symlink or an existing entry is
/// refused (the TOCTOU / clobber CVE-class defenses). This wrapper only maps `io::Error` → `CoreError`.
/// One transfer at a time. Never logs file contents (Inv 8).
#[derive(Default)]
struct AppFileWriteSink {
    inner: ras_files::SafeFileWriter,
}

impl ras_core::FileWriteSink for AppFileWriteSink {
    // The `io::Error` detail is deliberately dropped: `RasError::context` is a `&'static str`, and a raw
    // OS error could echo the destination path (Inv 8 hygiene). The stable `InputFailed` code + a static
    // context is all the host loop needs to abort + emit a content-free rejection.
    fn open(&self, dest: &std::path::Path, size: u64) -> Result<(), ras_core::CoreError> {
        self.inner.open(dest, size).map_err(|_| {
            ras_core::CoreError::fatal(ras_protocol::ErrorCode::InputFailed, "file open failed")
        })
    }
    fn write(&self, data: &[u8]) -> Result<(), ras_core::CoreError> {
        self.inner.write(data).map_err(|_| {
            ras_core::CoreError::fatal(ras_protocol::ErrorCode::InputFailed, "file write failed")
        })
    }
    fn finish(&self) -> Result<(), ras_core::CoreError> {
        self.inner.finish().map_err(|_| {
            ras_core::CoreError::fatal(ras_protocol::ErrorCode::InputFailed, "file finish failed")
        })
    }
    fn abort(&self) {
        self.inner.abort();
    }
}

/// The received-files sandbox directory for the `"drop"` target: `<home>/CasualRAS-Received`, created if
/// missing. This is the **host/vendor-chosen** destination (Inv 6 / S7 — never a controller path); the
/// controller only ever supplies a leaf filename, which ras-policy validates and joins onto this dir.
/// Falls back to the current dir if no home is resolvable (still host-side, never controller-supplied).
fn received_files_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = home.join("CasualRAS-Received");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The single file-push drop-target catalogue this app exposes (ADR-086): one `"drop"` target into
/// `<home>/CasualRAS-Received`, 500 MiB cap, any extension. The per-target capability is `file.push.drop`
/// (see [`capabilities_with_extras`]); consent + per-message gate + filename validation still apply.
fn file_catalogue() -> ras_core::policy::file::DropCatalogue {
    use ras_core::policy::file::{DropCatalogue, DropTarget};
    DropCatalogue::new(vec![DropTarget {
        name: FILE_DROP_TARGET.to_string(),
        description: "Received files".to_string(),
        dest_dir: received_files_dir(),
        max_bytes: 500 * 1024 * 1024,
        allowed_extensions: None,
    }])
}

/// File-offer payload pushed to the webview when a viewer offers a file (host side). The filename is shown
/// to the local user in the confirmation prompt (a filename is not a secret); no contents are ever
/// carried (Inv 8).
#[derive(Clone, serde::Serialize)]
struct FileOfferPayload {
    filename: String,
    size: u64,
}

/// An inbound contact message surfaced to the webview (ADR-095). `text` is the revealed plaintext body
/// for display ONLY — it crosses only the local Tauri IPC to the app's own webview (the same trust
/// boundary the in-session `chat-message` string already crosses), and is NEVER logged (Inv 8). It
/// lives only here + in the DOM; there is no on-disk copy.
#[derive(Clone, serde::Serialize)]
struct MessagePayload {
    contact_id: String,
    text: String,
    at: u64,
}

/// Freshness / replay bound for an inbound out-of-session message (ADR-095): a signed message whose
/// `issued_at` is more than this far from local wall-clock (in either direction) is refused by
/// `verify_signed`. 5 minutes tolerates modest clock skew while bounding replay; a message from a
/// badly-skewed peer is refused (the sender learns it — `recv_signal` withholds the ACK).
const MAX_SIGNAL_AGE_MS: u64 = 5 * 60_000;

/// Pointer position pushed to the overlay window (normalized 0..=65535).
#[derive(Clone, serde::Serialize)]
struct PointerPayload {
    x: u16,
    y: u16,
    visible: bool,
}

/// Annotation op pushed to the host overlay window (ADR-097). `op` is `"stroke"`/`"undo"`/`"clear"`;
/// `points` are normalized `0..=65535` `[x, y]`. Display data only (no secret) — mirrors the viewer's
/// markup onto the shared screen.
#[derive(Clone, serde::Serialize)]
struct AnnotatePayload {
    op: &'static str,
    tool: u8,
    color: u32,
    points: Vec<[u16; 2]>,
}

/// Project a core [`ras_protocol::AnnotateOp`] into the overlay's JSON payload.
fn annotate_payload(op: &ras_protocol::AnnotateOp) -> AnnotatePayload {
    use ras_protocol::{AnnotTool, AnnotateOp};
    match op {
        AnnotateOp::Undo => AnnotatePayload {
            op: "undo",
            tool: 0,
            color: 0,
            points: vec![],
        },
        AnnotateOp::Clear => AnnotatePayload {
            op: "clear",
            tool: 0,
            color: 0,
            points: vec![],
        },
        AnnotateOp::Stroke {
            tool,
            color_rgb,
            points,
        } => AnnotatePayload {
            op: "stroke",
            tool: match tool {
                AnnotTool::Pen => 0,
                AnnotTool::Highlighter => 1,
                AnnotTool::Arrow => 2,
                AnnotTool::Rect => 3,
            },
            color: *color_rgb,
            points: points.iter().map(|(x, y)| [*x, *y]).collect(),
        },
    }
}

/// Connection-quality readout for the viewer HUD (task #22). Projects `ras_core::QualitySample` for the
/// JS side; `path` is stringified because the transport's `PathKind` enum isn't `Serialize`.
#[derive(Clone, serde::Serialize)]
struct ConnQualityPayload {
    path: String,
    rtt_ms: u32,
    loss_pct: f32,
    fps: u16,
    kbps: u32,
}

/// A short, log-safe rendering of a peer identity (first 8 hex of the public key). A public identity,
/// not a secret; kept terse for display.
fn short_id(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for b in id.iter().take(4) {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

/// Deliver the local user's Allow/Deny for a pending viewer (Invariant 1).
#[tauri::command]
fn respond_consent(state: State<'_, AppState>, allow: bool) {
    state.share.consent.respond(allow);
}

/// Deliver the local user's Allow/Deny for a pending **control-lease** request (Phase 3, Invariant 1).
#[tauri::command]
fn respond_control_consent(state: State<'_, AppState>, allow: bool) {
    state.share.consent.respond_control(allow);
}

/// Deliver the local user's Allow/Deny for a pending **file-push** transfer (ADR-086, Invariant 1).
/// `accept` ⇒ allow the write; deny (or the 90 s timeout) refuses fail-closed and the host aborts.
#[tauri::command]
fn respond_file_offer(state: State<'_, AppState>, accept: bool) {
    state.share.consent.respond_file(accept);
}

/// Opt in/out of **clipboard sharing** with a connecting viewer (Share screen toggle, default OFF).
/// Clipboard has no per-message consent gate, so it is only placed in a viewer's grant when this is on
/// (Inv 1/7, ADR-076). Set it BEFORE a viewer connects — a grant's capabilities are fixed at issue time.
#[tauri::command]
fn set_clipboard_allowed(state: State<'_, AppState>, allowed: bool) {
    state.share.consent.set_clipboard_allowed(allowed);
}

/// Opt in/out of **output-audio sharing** with a connecting viewer (Share screen toggle, default OFF,
/// ADR-077). Output audio (host system audio → viewer) has no per-message consent gate, so its
/// `audio.listen` capability is only placed in a viewer's grant when this is on (Inv 1/7 — always
/// disclosed by the "AUDIO SHARED" indicator; no mic, live-only, never recorded — Inv 12). Set it BEFORE
/// a viewer connects — a grant's capabilities are fixed at issue time.
#[tauri::command]
fn set_audio_allowed(state: State<'_, AppState>, allowed: bool) {
    state.share.consent.set_audio_allowed(allowed);
}

/// Stop the whole share (drop the ticket, stop accepting, end any live viewer). Idempotent.
#[tauri::command]
fn stop_sharing(state: State<'_, AppState>) {
    if let Some(s) = lock(&state.share.session).take() {
        let _ = s.stop.send(true);
    }
}

/// Bring the main Casual RAS window back from the compact sharing strip (issue #5). Un-minimizes +
/// shows + focuses it so the host can reach chat / files / the full UI mid-share. It re-appears in the
/// shared screen while up (the host chose to open it); the strip's Stop stays available regardless.
#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Begin sharing this screen: bind an iroh endpoint, publish a ticket, and accept one viewer at a
/// time behind the local consent gate. Returns immediately; the ticket/status arrive as events.
/// Supported on macOS (hardware) + Linux/Windows (scap + OpenH264), ADR-063.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tauri::command]
async fn start_sharing(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // Already sharing? No-op (the UI reflects the current ticket).
    if lock(&state.share.session).is_some() {
        return Ok(());
    }
    // Reuse the always-on persistent endpoint (bound once in `.setup()`); never bind a second same-seed
    // endpoint (it would collide in iroh discovery). In the always-on model this command is a no-op
    // after startup (the session slot is already `Some`), but keep it correct if a Stop cleared it.
    let Some(endpoint) = state.endpoint.clone() else {
        let _ = app.emit("share-status", "Network endpoint unavailable.");
        return Err("network endpoint unavailable".into());
    };
    let gossip = state.gossip.clone();
    let contacts = state.contacts.clone();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    *lock(&state.share.session) = Some(ShareSession {
        stop: stop_tx,
        host: None,
    });
    let consent = state.share.consent.clone();
    tauri::async_runtime::spawn(async move {
        run_share(app, stop_rx, consent, endpoint, gossip, contacts).await;
    });
    Ok(())
}

/// On platforms with no capture backend the Share role is unavailable. The Connect role still works.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[tauri::command]
async fn start_sharing(app: tauri::AppHandle, _state: State<'_, AppState>) -> Result<(), String> {
    let _ = app.emit(
        "share-status",
        "Screen sharing isn't available on this platform. You can still Connect to another machine.",
    );
    Err("screen sharing is not available on this platform".into())
}

/// Construct the platform's capture + encoder pair for a share session (ADR-063). macOS uses the
/// zero-copy hardware path (VideoToolbox H.264); Linux/Windows use scap capture + the VP9 software
/// encoder (`ras-media-vpx`) — WebKitGTK/WebView2 decode VP9 where they often can't decode the H.264
/// the OpenH264 path emitted. The scap capture now stamps `VideoCodec::Vp9` in its `StreamConfig`
/// (`ras-media-scap`), so the capture-declared codec and the VP9 encoder's bytes agree end-to-end.
#[cfg(target_os = "macos")]
fn make_backends() -> (
    ras_media_macos::MacScreenCapture,
    ras_media_macos::VideoToolboxEncoder,
) {
    (
        ras_media_macos::MacScreenCapture::new(),
        ras_media_macos::VideoToolboxEncoder::new(),
    )
}

// Linux host: VP9 (WebKitGTK can't reliably decode H.264 — the black-screen fix). The scap capture
// stamps `VideoCodec::Vp9` on Linux so the declared codec matches the VP9 bytes.
#[cfg(target_os = "linux")]
fn make_backends() -> (ras_media_scap::ScapCapture, ras_media_vpx::VpxEncoder) {
    (
        ras_media_scap::ScapCapture::new(),
        ras_media_vpx::VpxEncoder::new(), // VP9
    )
}

// Windows host: OpenH264 (WebView2 decodes H.264 natively; no libvpx dependency on Windows). Cross-OS
// decode for Linux *viewers* of a Windows/macOS host is delivered by codec negotiation (follow-up), not
// by the host's default encoder here.
#[cfg(target_os = "windows")]
fn make_backends() -> (
    ras_media_scap::ScapCapture,
    ras_media_openh264::OpenH264Encoder,
) {
    (
        ras_media_scap::ScapCapture::new(),
        ras_media_openh264::OpenH264Encoder::new(),
    )
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn run_share(
    app: tauri::AppHandle,
    mut stop: tokio::sync::watch::Receiver<bool>,
    consent: Arc<LocalConsent>,
    // The always-on endpoint, pre-bound in `.setup()` with the persistent identity seed. Passed in (not
    // bound here) so ONE endpoint serves session + bootstrap + gossip — a second same-seed endpoint
    // would collide in iroh discovery (ADR-094/098). `identity_seed` is read exactly once, at bind.
    endpoint: Arc<ras_transport_iroh::Endpoint>,
    // The gossip actor attached to that same endpoint, or `None` if gossip setup failed (⇒ presence
    // off, sharing/contacts on — FAIL-SAFE). The accept loop routes `GOSSIP_ALPN` connections to it.
    gossip: Option<iroh_gossip::net::Gossip>,
    // The durable contacts book, so the accept loop can verify an inbound SIGNAL_ALPN message
    // contacts-only (deny-by-default, ADR-095). `None` ⇒ contacts storage unavailable, so inbound
    // signals are dropped (FAIL-SAFE — a message can never arrive from an unverifiable book).
    contacts: Option<Arc<ras_identity::FileContactBook>>,
) {
    let _ = app.emit("share-active", true);
    let _ = app.emit(
        "share-status",
        "Starting… contacting a relay for a reachable address.",
    );
    // The persistent identity seed drove the pre-bound endpoint; re-read it here only to derive the
    // host application keystore/grant issuer below (same seed ⇒ host_id == endpoint id == contact id).
    let id_seed = identity_seed(&app);
    // Wait for relay connectivity, but NEVER hang forever. `endpoint.online()` returns only once a
    // home relay reports connected; on a machine that can't reach one (offline, captive portal,
    // corporate firewall blocking relay UDP/hosts, or the relay is down) it loops indefinitely — and
    // because this sits before the accept loop's stop-select, Stop couldn't break it either. That
    // wedges the Share with no ticket and no error (a real-run-only blocker: loopback/direct-dial
    // tests skip `online()` entirely). Bound the wait, keep Stop responsive throughout, and fall back
    // to a direct-address ticket so a same-network viewer can still dial even with no relay.
    let online = tokio::select! {
        _ = endpoint.online() => true,
        _ = stop.changed() => {
            // Stop pressed while still contacting a relay — tear down cleanly instead of parking.
            if let Some(ov) = app.get_webview_window("overlay") {
                let _ = ov.hide();
            }
            let _ = app.emit("share-active", false);
            let _ = app.emit("share-status", "Sharing stopped.");
            return;
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => false,
    };
    if !online {
        log::warn!("share: no relay reachable within 20s — direct-address only (LAN)");
        let _ = app.emit(
            "share-status",
            "No relay reachable — sharing a direct-address code. It will work only if the viewer is on the same network. Check your internet connection or firewall, then try again for remote access.",
        );
    } else {
        log::info!("share: online, endpoint reachable");
    }
    // The ticket carries this endpoint's direct socket addresses (known since bind) plus its relay, so
    // it is dialable on a LAN even when the relay never came up.
    let _ = app.emit("share-ticket", endpoint.addr().to_ticket());
    let _ = app.emit("share-status", "Waiting for a viewer to connect…");

    // This host's application identity + grant issuer (Phase 2). Derived from the SAME persistent seed
    // as the endpoint (ADR-092/093), so `host_id == host_endpoint_id == the contact id` — one identity.
    // The issuer's key IS the host id, so the grants it mints verify against the same key the
    // session-phase validator checks (`GrantSessionValidator` uses `ctx.host_id`).
    use ras_core::grant::{LocalHostGrantIssuer, NonceCache, MAX_REQUEST_TTL_MS};
    use ras_core::identity::{KeyStore, SoftwareKeyStore};
    let host_ks = match id_seed {
        Some(seed) => SoftwareKeyStore::from_seed(seed),
        None => match SoftwareKeyStore::generate() {
            Ok(k) => k,
            Err(_) => {
                log::error!("share: failed to create host identity");
                let _ = app.emit("share-status", "Failed to create a host identity.");
                let _ = app.emit("share-active", false);
                return;
            }
        },
    };
    let host_id = host_ks.public_key();
    let host_endpoint_id = endpoint.id().0;
    // Grant ceiling includes clipboard + file-push so consent CAN grant them (see
    // `capabilities_with_extras`); OS input, clipboard, file transfer, etc. are still each subject to the
    // local consent + per-message gate + (for files) the per-transfer consent and filename validation (Inv 15).
    let issuer = LocalHostGrantIssuer::new(host_ks, capabilities_with_extras(), 1);
    // Shared replay cache for AccessRequest nonces across bootstrap connections (the accept loop
    // handles one connection at a time, so a `&mut` borrow suffices).
    let mut nonces = NonceCache::new(MAX_REQUEST_TTL_MS, 4096);

    // Holds the most recent bootstrap connection AFTER its grant was sent, keeping the QUIC link
    // alive so the grant is reliably delivered/retransmitted. It is released (dropped) only when the
    // next connection is accepted — by which point the controller has already dialed the session
    // ALPN with that grant, proving delivery. Without this the connection dropped the instant the
    // grant was sent, discarding un-acked bytes on a real link (the "bootstrap read failed after
    // Allow" real-run blocker).
    let mut pending_bootstrap: Option<ras_transport_iroh::Session> = None;

    loop {
        if *stop.borrow() {
            break;
        }
        let accepted = tokio::select! {
            _ = stop.changed() => { if *stop.borrow() { break } else { continue } },
            a = endpoint.accept() => a,
        };
        // NOTE: a held bootstrap connection must be kept alive until the controller's *session dial*
        // arrives (the grant-drain fix — QUIC retransmits the grant until then; dropping early discards
        // un-acked grant bytes on a real-RTT link). So we do NOT drop it unconditionally here: a gossip
        // presence dial or a signal message arriving in the grant-delivery window must NOT evict the
        // held bootstrap. Only a *bootstrap* (a new controller supersedes the old) or a *session*
        // connection (the controller proved delivery) drops it — in those two arms below.
        match accepted {
            // Route by negotiated ALPN. A GOSSIP_ALPN connection is an inbound presence dial: hand it to
            // the Gossip actor (fire-and-forget, spawned so it never blocks the accept loop that also
            // serves screen sessions — the latency invariant) (ADR-094). FAIL-SAFE: the `let _ =`
            // swallows any gossip error, so a hostile/broken gossip connection can never stall or crash
            // the accept loop. Does NOT touch `pending_bootstrap` — a presence dial is not the
            // controller's session dial, so a held grant keeps draining. `is_gossip`/`is_signal`/
            // `is_bootstrap` are mutually exclusive; a session is none of them.
            Ok(Some(session)) if session.is_gossip() => {
                if let Some(g) = gossip.as_ref() {
                    let conn = session.into_connection();
                    let g = g.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = g.handle_connection(conn).await;
                    });
                }
                // else: presence disabled ⇒ drop the connection; sessions/contacts unaffected.
            }
            // A SIGNAL_ALPN connection carries one signed out-of-session message from a contact
            // (ADR-095). Verify it contacts-only and surface it; no consent, no grant, no pixels — it
            // authorizes nothing (Inv 9). Spawned so a slow/hostile signal never blocks the accept loop
            // that also serves screen sessions (the latency invariant). FAIL-SAFE: the whole arm no-ops
            // if the contacts book is unavailable (a message can't be verified without it).
            Ok(Some(session)) if session.is_signal() => {
                if let Some(book) = contacts.clone() {
                    let sig_app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        handle_signal(&sig_app, session, book).await;
                    });
                }
                // else: contacts storage unavailable ⇒ drop the connection; sessions unaffected.
                // Does NOT touch `pending_bootstrap` — a signal is not the controller's session dial.
            }
            // A bootstrap connection runs consent + issuance; a session connection presents the
            // resulting grant and streams frames. A NEW bootstrap supersedes any held one (drop it
            // first, closing that stale link), then this one's grant is held pending its session dial.
            Ok(Some(session)) if session.is_bootstrap() => {
                drop(pending_bootstrap.take());
                pending_bootstrap = handle_bootstrap(
                    &app,
                    session,
                    host_id,
                    host_endpoint_id,
                    &issuer,
                    &mut nonces,
                    &consent,
                )
                .await;
            }
            Ok(Some(session)) => {
                // The controller's session dial: it proves the grant was delivered, so the held
                // bootstrap connection has done its job — drop it (closing that QUIC link cleanly)
                // before serving the session.
                drop(pending_bootstrap.take());
                serve_one(&app, &endpoint, session, host_id, &consent, &mut stop).await;
            }
            Ok(None) => break, // endpoint closed
            Err(_) => continue,
        }
    }

    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.hide();
    }
    // Dismiss the compact sharing strip and restore the main window now that the session is over.
    if let Some(strip) = app.get_webview_window("control-strip") {
        let _ = strip.hide();
    }
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_minimizable(true);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
    let _ = app.emit("share-viewer", false);
    let _ = app.emit("share-active", false);
    let _ = app.emit("share-status", "Sharing stopped.");
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
/// CGWindowIDs of our own windows (overlay + main/indicator) to keep out of the shared capture, so
/// the viewer never sees the remote-pointer overlay we draw for them (a feedback loop) or our local
/// control UI. Fail-safe: any window whose id can't be read is simply not excluded (capture is
/// unchanged), never fatal. macOS-only — the capture backend that consumes these is macOS-only.
#[cfg(target_os = "macos")]
fn host_excluded_windows(app: &tauri::AppHandle) -> Vec<ras_media::WindowId> {
    ["overlay", "main", "control-strip"]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .filter_map(|w| w.ns_window().ok())
        .filter_map(|ns| {
            let obj = ns as *mut objc2::runtime::AnyObject;
            if obj.is_null() {
                return None;
            }
            // SAFETY: `obj` is a live NSWindow handed out by Tauri for this window; `windowNumber`
            // takes no arguments and returns the CGWindowID as an NSInteger.
            let number: isize = unsafe { objc2::msg_send![obj, windowNumber] };
            (number > 0).then_some(ras_media::WindowId(number as u64))
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn host_excluded_windows(_app: &tauri::AppHandle) -> Vec<ras_media::WindowId> {
    Vec::new()
}

/// Handle a **signal-ALPN** connection (ADR-095): read one signed out-of-session message from a
/// contact, verify it (signature → contacts-only, deny-by-default → freshness) via
/// [`ras_signal::net::recv_signal`], and surface a verified [`SignalPayload::DirectMessage`] to the
/// webview as a `message` event. Far simpler than the bootstrap handler: no consent, no grant, no
/// pixels — a delivered message authorizes nothing (Inv 9). `recv_signal` ACKs **only** on success, so
/// a stranger's / blocked / stale / forged signal is dropped un-ACKed (the sender learns it was
/// refused). The body is `.reveal()`d exactly once, at the event emit — the single sanctioned display
/// boundary (mirrors `LifecycleEvent::ChatMessage`); it is never logged (Inv 8).
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn handle_signal(
    app: &tauri::AppHandle,
    session: ras_transport_iroh::Session,
    book: Arc<ras_identity::FileContactBook>,
) {
    use ras_identity::ContactBook;
    use ras_signal::SignalPayload;
    match ras_signal::net::recv_signal(
        session.connection(),
        book.as_ref() as &dyn ContactBook,
        now_ms(),
        MAX_SIGNAL_AGE_MS,
    )
    .await
    {
        Ok(verified) => match verified.payload {
            SignalPayload::DirectMessage { text, .. } => {
                // `.reveal()` here is the sole display boundary for the body — emitted only to our own
                // webview over local IPC, never to a log/trace (Inv 8).
                let _ = app.emit(
                    "message",
                    MessagePayload {
                        contact_id: hex_id(verified.sender.as_bytes()),
                        text: text.reveal().to_string(),
                        at: now_ms(),
                    },
                );
                // Gentle attention + a content-free notification — never the message text (Inv 8),
                // matching the in-session chat pattern.
                alert_user(
                    app,
                    false,
                    "Casual RAS — new message",
                    "You have a new message.",
                );
            }
            // An access-request intent raises a consent prompt elsewhere; presence beacons arrive via
            // gossip, not this ALPN. Neither is wired here (out of this increment's scope).
            SignalPayload::AccessRequestIntent { .. } | SignalPayload::PresenceBeacon { .. } => {}
        },
        // Bad signature / non-contact / stale: content-free warning only (never the sender key detail
        // beyond a short id, never the body). `recv_signal` already withheld the ACK.
        Err(_) => {
            log::warn!("signal: inbound message refused");
        }
    }
}

/// Handle a **bootstrap-ALPN** connection (Phase 2): read the controller's `ClientHello` +
/// signed `AccessRequest`, validate it host-side (signature, endpoint sender-constraint, freshness,
/// replay, capability recognition), get local consent (Invariant 1), and — only on Allow — issue a
/// PASETO grant bound to this controller's endpoint. Every failure sends a content-free `Denied`
/// reason and returns; no session/pixels are involved here.
///
/// Returns `Some(session)` only when a grant was actually sent, so the caller can KEEP the bootstrap
/// connection alive until delivery is proven (see the accept loop). Returns `None` on any denial or
/// error (nothing to keep alive — the connection is dropped immediately). This is the fix for the
/// real-run-only blocker where dropping the connection right after `boot.send(grant)` let QUIC
/// discard the still-un-acked grant bytes on a non-zero-RTT link, so the controller never received
/// the grant and the connect failed right after the local user clicked Allow. Zero-RTT loopback
/// always delivered before the drop, so tests never saw it.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn handle_bootstrap(
    app: &tauri::AppHandle,
    session: ras_transport_iroh::Session,
    host_id: [u8; 32],
    host_endpoint_id: [u8; 32],
    issuer: &ras_core::grant::LocalHostGrantIssuer<ras_core::identity::SoftwareKeyStore>,
    nonces: &mut ras_core::grant::NonceCache,
    consent: &Arc<LocalConsent>,
) -> Option<ras_transport_iroh::Session> {
    use ras_core::grant::{
        fresh_id, validate_access_request, AccessRequest, SessionGrantIssuer, SessionParams,
        MAX_REQUEST_TTL_MS,
    };
    use ras_protocol::{AccessOutcome, BootstrapMsg, ErrorCode};

    // The controller's transport-authenticated endpoint — the identity the grant is bound to.
    let peer_endpoint = session.remote().0;
    let Ok(mut boot) = session.bootstrap().await else {
        return None;
    };

    // Small helper: send a content-free denial and stop.
    macro_rules! deny {
        ($boot:expr, $code:expr) => {{
            let _ = $boot
                .send(BootstrapMsg::AccessDecision(AccessOutcome::Denied {
                    code: $code,
                }))
                .await;
            return None;
        }};
    }

    // ClientHello → HostHello (advertise our identity + Tier 0).
    match boot.recv().await {
        Ok(BootstrapMsg::ClientHello { .. }) => {}
        _ => return None,
    }
    if boot
        .send(BootstrapMsg::HostHello { host_id, tier: 0 })
        .await
        .is_err()
    {
        return None;
    }

    // AccessRequest (opaque, signed) → decode + validate.
    let canonical = match boot.recv().await {
        Ok(BootstrapMsg::AccessRequest { canonical }) => canonical,
        _ => return None,
    };
    let request = match AccessRequest::decode(&canonical) {
        Ok(r) => r,
        Err(code) => deny!(boot, code),
    };
    let now = now_ms();
    if let Err(code) = validate_access_request(&request, &host_id, &peer_endpoint, now, nonces) {
        deny!(boot, code);
    }

    // Surface the viewer's stable contact code so the host can save them back as a contact — this is
    // what makes contacts **two-way** after a connection (ADR-092/093) without any always-on presence
    // layer: the controller already saved the host (they dialed by its code/ticket); this lets the host
    // reciprocate. It's a public-key-derived code (display data, no secret), and the host chooses
    // whether to add them (Invariant 1 — never auto-saved).
    let _ = app.emit(
        "peer-contact",
        ras_identity::contact_code(&ras_identity::ContactId::from_bytes(request.controller_id)),
    );

    // Local human consent (Invariant 1) — no grant is minted until the user clicks Allow.
    let _ = app.emit("share-status", "A viewer is requesting access…");
    if !consent.prompt(short_id(&request.controller_id)).await {
        deny!(boot, ErrorCode::ConsentDenied);
    }

    // Issue a sender-constrained grant for the consented (view-only) capabilities.
    let params = SessionParams {
        session_id: fresh_id().unwrap_or([0u8; 16]),
        host_endpoint_id,
        session_generation: 1,
        session_nonce: fresh_id().unwrap_or([0u8; 16]),
        issued_at: now,
        not_before: now,
        expires_at: now + MAX_REQUEST_TTL_MS,
    };
    // The `consented` set is what the local user actually agreed to — NOT the app's maximal ceiling.
    // Screen view + OS-input + file-push ride the view-Allow (each has its own second gate: control-lease
    // consent, a held lease + per-message gate, or per-transfer file consent). Clipboard has no second
    // gate, so it is consented ONLY if the host opted in on the Share screen (default off) — otherwise a
    // view-Allow must never silently authorize controller→host clipboard writes (Inv 1/7, ADR-076).
    match issuer
        .issue(
            &request,
            &consented_capabilities(consent.clipboard_allowed(), consent.audio_allowed()),
            &params,
        )
        .await
    {
        Ok(grant) => {
            if boot
                .send(BootstrapMsg::AccessDecision(AccessOutcome::Allowed {
                    grant,
                }))
                .await
                .is_err()
            {
                return None;
            }
            // Grant sent. Finish the send stream (drop `boot`), but hand the still-open connection
            // back to the accept loop so QUIC keeps the link up and retransmits the grant until the
            // controller has it (proven when its session-ALPN dial arrives). Dropping the connection
            // here instead would discard un-acked grant bytes on a real-RTT link.
            drop(boot);
            Some(session)
        }
        Err(e) => deny!(boot, e.code),
    }
}

async fn serve_one(
    app: &tauri::AppHandle,
    endpoint: &Arc<ras_transport_iroh::Endpoint>,
    session: ras_transport_iroh::Session,
    host_id: [u8; 32],
    consent: &Arc<LocalConsent>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) {
    use ras_core::{
        GrantSessionValidator, HostSession, HostSessionConfig, IrohSessionTransport,
        LifecycleEvent, StopReason,
    };
    use ras_media::MonitorId;

    let _ = app.emit("share-status", "A viewer is connecting…");

    // Phase-3 OS-input backend. Held concretely so we can feed it the shared display's bounds (below).
    // macOS: prompt for PostEvent access up front so that, by the time a viewer asks for control,
    // `input_permitted()` is true; otherwise the host refuses the lease fail-closed.
    #[cfg(target_os = "macos")]
    let input_sink = {
        let s = Arc::new(ras_input_macos::CgEventSink::new());
        let _ = s.request_access();
        s
    };
    // Linux: XTEST over x11rb (ADR-070). No permission prompt — it connects to $DISPLAY as the user
    // and is fail-closed when no X server is reachable (`input_permitted()` false ⇒ lease refused).
    #[cfg(target_os = "linux")]
    // Pick the best Linux backend at runtime (ADR-098+): kernel uinput when /dev/uinput is available
    // (works on Wayland AND X11), else the unprivileged X11 XTEST backend (X11/Xwayland only), else a
    // fail-closed sink whose input_permitted() is false so the host refuses the lease and the honest
    // "needs Xorg / uinput setup" banner shows — never dead control.
    let input_sink = ras_input_linux::best_input_sink();
    // Windows: SendInput over windows-rs (ADR-071). In-session, no UIAccess (Inv 14).
    #[cfg(target_os = "windows")]
    let input_sink = Arc::new(ras_input_windows::SendInputSink::new());

    // Tell the sharer UP FRONT if OS-input injection won't be available on this machine, so a later
    // "Take control" request doesn't just silently hang (the host refuses the lease fail-closed when
    // `input_permitted()` is false — Inv 15/fail-closed — but that refusal was previously invisible to
    // the user). Screen viewing is unaffected; only remote control needs OS-level injection rights.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        use ras_core::control::OsInputSink as _;
        if !input_sink.input_permitted() {
            #[cfg(target_os = "macos")]
            let msg =
                "Remote control needs Accessibility permission: open System Settings → Privacy \
                       & Security → Accessibility, enable Casual RAS, then stop and start sharing \
                       again. Screen viewing works without it.";
            #[cfg(target_os = "linux")]
            let msg = "Remote control is unavailable: no X11 server is reachable. A pure-Wayland \
                       session cannot receive injected input (XTEST) — log in using an Xorg/X11 \
                       session to allow control. Screen viewing works without it.";
            #[cfg(target_os = "windows")]
            let msg =
                "Remote control is currently unavailable on this machine. Screen viewing works.";
            log::warn!("share: OS input not permitted up front — {msg}");
            let _ = app.emit("share-input-warning", msg);
        }
    }

    let (capture, encoder) = make_backends();
    // Host side of ADR-091 resume: on a transport drop the host re-accepts on the same endpoint and
    // waits for the same peer (by authenticated EndpointId) to re-dial, then resumes. Symmetric to the
    // controller's re-dial above.
    let transport =
        Arc::new(IrohSessionTransport::new(endpoint.clone(), session).with_reconnect_host());
    let host = HostSession::new(
        // Exclude our own overlay/indicator windows from the shared feed (privacy + no feedback loop).
        HostSessionConfig::new(MonitorId(0))
            .with_excluded_windows(host_excluded_windows(app))
            .with_host_id(host_id),
        transport,
        capture,
        encoder,
        // The session-phase gate: validate the PASETO grant the controller presents against the
        // endpoint iroh just authenticated (consent already happened in the bootstrap phase).
        Arc::new(GrantSessionValidator),
    )
    // The control-lease consent prompt (Invariant 1) — a second, input-specific Allow/Deny.
    .with_control_consent(consent.clone());
    // On macOS/Linux, feed the OS-input backend so a granted lease can actually inject (elsewhere, no
    // backend ⇒ control requests are refused fail-closed).
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    let host = host.with_input_sink(input_sink.clone());
    // Feed the OS-clipboard write backend (ADR-079) so a `clipboard.write`-granted push can set the
    // host clipboard (never pastes — ADR-076). It stays inert while clipboard.write is withheld
    // (default OFF); a clipboard the platform can't open just leaves the host refusing pushes.
    let host = match ras_clipboard::ArboardClipboardSink::new() {
        Ok(sink) => host.with_clipboard_sink(Arc::new(sink)),
        Err(_) => host,
    };
    // File transfer (ADR-086/090): the vendor-declared `"drop"` catalogue (host-chosen sandbox dir + size
    // cap), the per-transfer local consent prompt (Inv 1 — reuses `LocalConsent`'s `FileConsent` impl), and
    // the `O_NOFOLLOW|O_EXCL` write backend. All three are needed for a push to land; with `file.push.drop`
    // in the grant ceiling (see `capabilities_with_extras`), the danger channel stays fully core-enforced:
    // ras-policy validates the leaf filename + resolves the destination, ras-files refuses symlink/clobber,
    // and the per-message gate checks the capability (Inv 15). Consent (`LocalConsent`) is `Arc`-shared.
    let host = host
        .with_file_catalogue(file_catalogue())
        .with_file_consent(consent.clone())
        .with_file_write_sink(Arc::new(AppFileWriteSink::default()));
    // Output-audio pipeline (ADR-077): a per-OS `AudioCaptureBackend` (host system audio — no mic) +
    // the shared `OpusEncoder`. `ras-core` runs the audio pump **only if** the grant carries
    // `audio.listen` (Inv 15) AND the transport has an audio plane (iroh does) — so this stays inert
    // unless the host opted in on the Share screen (default OFF). No audio content is ever logged (Inv 8);
    // audio is live-only, never recorded (Inv 12), and disclosed by an "AUDIO SHARED" indicator when active.
    #[cfg(target_os = "macos")]
    let audio_capture = ras_audio_macos::MacAudioCapture::new();
    #[cfg(target_os = "linux")]
    let audio_capture = ras_audio_linux::LinuxAudioCapture::new();
    #[cfg(target_os = "windows")]
    let audio_capture = ras_audio_windows::WindowsAudioCapture::new();
    let host = host.with_audio(
        Box::new(audio_capture),
        Box::new(ras_audio_opus::OpusEncoder::new()),
    );
    // Share the built host session behind `Arc` so the `send_chat`/`send_clipboard` commands can reach
    // it out-of-band (via `ShareControl` in `ShareState`) while this loop also drives it. All the send
    // APIs take `&self`, so the `Arc` clone and the local `host` coexist safely.
    let host = Arc::new(host);

    // `start()` runs the handshake, then blocks in the consent gate until Allow/Deny. Deny → Err.
    let mut events = match host.start().await {
        Ok(events) => events,
        Err(_) => {
            let _ = app.emit(
                "share-status",
                "Access denied. Waiting for the next viewer…",
            );
            return;
        }
    };

    // Approved: register the live host handle so out-of-band chat/clipboard sends can find it. Cleared
    // when this loop exits (below). `host` is a concrete `HostSession<C, E>`; store it erased behind
    // `ShareControl` so `ShareState` stays non-generic.
    {
        let state = app.state::<AppState>();
        let mut guard = lock(&state.share.session);
        if let Some(s) = guard.as_mut() {
            let erased: Arc<dyn ShareControl> = host.clone();
            s.host = Some(erased);
        }
        drop(guard);
    }

    // Approved: session is Active. Show the indicator + the pointer overlay.
    log::info!("share: viewer connected — REMOTE VIEWING ACTIVE");
    let _ = app.emit("share-status", "Viewer connected — REMOTE VIEWING ACTIVE.");
    let _ = app.emit("share-viewer", true);
    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.show();
        // Click-through — set only AFTER show(), so the GDK window is realized. Calling it before the
        // window is shown panics tao on Linux (window().unwrap() on None); post-show it is safe on all
        // OSes. The show + this call are ordered on the same WindowRequest channel, so realization is
        // done by the time this is processed.
        let _ = ov.set_ignore_cursor_events(true);
    }
    // Keep the host's own control UI OUT of the shared screen (issue #5): minimize the big main window
    // and raise a compact always-on-top strip carrying the live indicator + Stop. Invariant 7 holds —
    // the Stop control stays visible on the strip. Invariant 1 holds too: a mid-session consent prompt
    // (control-lease / file) calls `alert_user`, which un-minimizes + focuses main so the host can
    // answer. On Linux the strip is small (scap can't exclude windows, so a big window would otherwise
    // fill the shared feed); on macOS the strip is also excluded via `host_excluded_windows`.
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.minimize();
    }
    if let Some(strip) = app.get_webview_window("control-strip") {
        let _ = strip.show();
    }

    // Dedupe key for the InputRejected diagnostic below: a rejected control gate fires per-event (up to
    // ~25 Hz for pointer moves), so we surface only reason *changes* to avoid flooding the log/UI.
    let mut last_input_reject: Option<String> = None;
    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    log::info!("share: Stop pressed — halting session (emergency stop path, Inv 4)");
                    host.stop(StopReason::UserRequested).await;
                    break;
                }
            }
            ev = events.recv() => match ev {
                Some(LifecycleEvent::RemotePointer { x, y, visible }) => {
                    if let Some(ov) = app.get_webview_window("overlay") {
                        let _ = ov.emit("pointer", PointerPayload { x, y, visible });
                    }
                }
                // Viewer annotation markup (ADR-097) → render it on the host overlay. Purely visual.
                Some(LifecycleEvent::RemoteAnnotation(op)) => {
                    if let Some(ov) = app.get_webview_window("overlay") {
                        let _ = ov.emit("annotate", annotate_payload(&op));
                    }
                }
                Some(LifecycleEvent::CaptureGeometry { x, y, width, height }) => {
                    // Place the pointer overlay over exactly the shared display (logical/point
                    // coordinates, which macOS global space and Tauri's Logical* share), so the
                    // normalized remote pointer lands on the right pixels — including on a secondary
                    // monitor. Best-effort: positioning failures leave the default overlay.
                    if let Some(ov) = app.get_webview_window("overlay") {
                        use tauri::{LogicalPosition, LogicalSize};
                        // A maximized window ignores set_position/set_size (its geometry is locked to
                        // the primary monitor) — the multi-monitor bug where the overlay stuck to the
                        // primary display while capture streamed a secondary one. Unmaximize first so the
                        // overlay can be moved to (and sized for) exactly the shared display, even at a
                        // negative origin (a monitor left of / above the primary).
                        let _ = ov.unmaximize();
                        let _ = ov.set_position(LogicalPosition::new(x, y));
                        let _ = ov.set_size(LogicalSize::new(width, height));
                    }
                    // Feed the same bounds to the input backend so normalized input maps to the right
                    // pixels on the shared display (display id 0 in the single-display MVP).
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
                    input_sink.set_display_bounds(
                        0,
                        f64::from(x),
                        f64::from(y),
                        f64::from(width),
                        f64::from(height),
                    );
                }
                // Control-lease lifecycle (Phase 3), content-free. Surface it so the sharer's UI can
                // show that the viewer now has (or lost) OS-input control.
                Some(LifecycleEvent::ControlLeaseGranted { .. }) => {
                    last_input_reject = None; // fresh control session — let the first rejection resurface
                    let _ = app.emit("share-control", true);
                    let _ = app.emit(
                        "share-status",
                        "Viewer has REMOTE CONTROL of this screen.",
                    );
                }
                Some(LifecycleEvent::ControlLeaseEnded { .. }) => {
                    let _ = app.emit("share-control", false);
                    let _ = app.emit("share-status", "Viewer connected — REMOTE VIEWING ACTIVE.");
                }
                // The host-side per-message gate (Inv 15) rejected a remote input event. Content-free
                // (a reason code only). Normally silent, but when EVERY event is rejected the viewer's
                // "Take control" looks dead — so surface the reason (deduped) to the sharer + the log.
                // This is the diagnostic for "I have control but clicks/keys do nothing": the code
                // (StaleGeneration / NoActiveLease / LeaseExpired / ReplayedInput / StaleLayout /
                // CapabilityDenied / InputFailed) names exactly which check failed.
                Some(LifecycleEvent::InputRejected { code }) => {
                    let c = format!("{code:?}");
                    if last_input_reject.as_deref() != Some(c.as_str()) {
                        log::warn!("share: remote input rejected by the control gate — {c}");
                        let _ = app.emit("share-input-rejected", c.clone());
                        last_input_reject = Some(c);
                    }
                }
                // Chat received from the viewer (ADR-082). `.reveal()` here is the sanctioned display
                // boundary — the only place the redacted text is read; it is never logged (Inv 8).
                Some(LifecycleEvent::ChatMessage { text }) => {
                    let _ = app.emit("chat-message", text.reveal().to_string());
                    alert_user(
                        app,
                        false,
                        "Casual RAS — new message",
                        "You have a new chat message.",
                    );
                }
                // The viewer pushed clipboard and we set it on the host's OS clipboard (controller→host,
                // ADR-076). Content-free: emit only the byte count (Inv 8).
                Some(LifecycleEvent::ClipboardApplied { len }) => {
                    let _ = app.emit("clipboard-received", len);
                }
                // A viewer's file push was authorized + locally consented (ADR-086); the host is writing it
                // (O_NOFOLLOW|O_EXCL). The lifecycle event is content-free — label it with the filename+size
                // stashed at the consent prompt (a filename is shown to the user, not a secret — Inv 8).
                Some(LifecycleEvent::FileTransferAccepted) => {
                    let (filename, size) = consent
                        .last_file_offer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                        .unwrap_or_default();
                    let _ = app.emit("file-received", FileOfferPayload { filename, size });
                }
                // A viewer's file push was refused (unknown target / capability withheld / unsafe filename /
                // too large / consent denied / short transfer). Surface the stable reason (content-free).
                Some(LifecycleEvent::FileTransferRejected { code }) => {
                    let _ = app.emit("file-rejected", format!("{code:?}"));
                }
                Some(LifecycleEvent::SessionEnded { .. })
                | Some(LifecycleEvent::Revoked { .. })
                | Some(LifecycleEvent::Disconnected { .. })
                | None => break,
                _ => {}
            },
        }
    }

    // The viewer is gone: clear the out-of-band host handle so chat/clipboard commands stop finding a
    // dead session (the share task may still loop for the next viewer with a fresh `host`).
    {
        let state = app.state::<AppState>();
        let mut guard = lock(&state.share.session);
        if let Some(s) = guard.as_mut() {
            s.host = None;
        }
        drop(guard);
    }

    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.hide();
    }
    let _ = app.emit("share-viewer", false);
    let _ = app.emit(
        "share-status",
        "Viewer disconnected. Waiting for the next viewer…",
    );
}

// ─── Entrypoint ──────────────────────────────────────────────────────────────────────────────────

// ─── Signed auto-update (ADR-078) ─────────────────────────────────────────────────────────────────
// The updater verifies each downloaded artifact against the embedded Ed25519 (minisign) public key
// before applying (Inv-spirit: the machine only runs code the publisher signed). Updates are
// **user-initiated** here — no silent background replacement — and applied only on explicit consent,
// consistent with "the local user is the final owner of the machine" (Inv 1).

/// Return recent diagnostics — app version, OS/arch, and the tail of the **content-free** log file —
/// for the user to copy and share when reporting an issue. This is what makes the field logging
/// actionable: on-device, one click yields a shareable trail. Content-free by construction (the log
/// never holds pixels/keystrokes/clipboard/typed-text/secrets — Inv 8), so this is always safe to copy.
#[tauri::command]
fn read_diagnostics(app: tauri::AppHandle) -> Result<String, String> {
    let mut out = format!(
        "Casual RAS {} · {} · {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Ok(dir) = app.path().app_log_dir() {
        match std::fs::read_to_string(dir.join("casual-ras.log")) {
            // Last ~200 lines is plenty for a recent-events tail (line-based, so never a UTF-8 split).
            Ok(contents) => {
                let mut lines: Vec<&str> = contents.lines().collect();
                let start = lines.len().saturating_sub(200);
                lines.drain(..start);
                out.push_str("\n\n--- recent log ---\n");
                out.push_str(&lines.join("\n"));
            }
            Err(_) => out.push_str("\n(no log recorded yet)"),
        }
    }
    Ok(out)
}

/// Check the configured endpoint for a newer signed release. `Ok(Some(version))` if one is available,
/// `Ok(None)` if up to date, `Err(msg)` if the updater is not configured / unreachable (surfaced to
/// the user, never silently swallowed).
#[tauri::command]
async fn check_for_updates(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(update.version.clone())),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Download and apply the pending signed update, then relaunch. Only ever called after the local user
/// explicitly consents in the UI. The download is signature-verified by the plugin; a bad signature
/// aborts the install (no unsigned code is ever run).
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update available".to_string())?;
    // Signature is verified against the embedded pubkey inside download_and_install; on failure this
    // returns Err and nothing is applied.
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    // Relaunch into the freshly-installed version. `restart` diverges (never returns).
    app.restart();
}

/// Tear down any active share deterministically (Invariant 7). Pixels must never outlive the
/// indicator + Stop surface: the in-app indicator/Stop live in the `main` window, but the capture→
/// stream loop runs in a detached `run_share` task and the always-on-top `overlay` window keeps the
/// process alive after `main` closes — so without this, closing the main window would leave the screen
/// streaming to the viewer with every indicator gone. Called from the window-close / exit event handler,
/// so the stop is synchronous and in-process (no unawaited-IPC race like the JS `beforeunload` path).
/// Returns `true` if a share was active (and has now been signalled to stop).
fn stop_active_share(handle: &tauri::AppHandle) -> bool {
    use tauri::Manager;
    let state = handle.state::<AppState>();
    // Take the session out in its own statement so the `MutexGuard` temporary is dropped at the `;`
    // (before `state`), rather than living to the end of an `if let` block.
    let session = lock(&state.share.session).take();
    if let Some(s) = session {
        let _ = s.stop.send(true);
        true
    } else {
        false
    }
}

fn main() {
    // WebKitGTK's DMABUF renderer crashes or paints white artifacts on many Linux
    // GPU/driver/compositor combinations — a well-known Tauri-on-Linux failure, and worse here
    // because we use transparent overlay windows. Force the stable non-DMABUF path before the
    // WebView (and any GTK thread) initializes, unless the user has explicitly chosen a value.
    // Costs a little GPU compositing, never correctness. See issue #1.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        // Signed auto-update (ADR-078). The plugin verifies each downloaded update against the
        // embedded public key before applying; a bad/absent signature is refused. Registration is
        // harmless when no key/endpoint is provisioned — `check_for_updates` just reports "not
        // configured".
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Native OS notifications for inbound requests / chat (see `alert_user`).
        .plugin(tauri_plugin_notification::init())
        // Field diagnostics: a rotating log file in the OS log dir + stderr. Content-free (Inv 8).
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(2_000_000)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("casual-ras".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stderr),
                ])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            connect_to_host,
            connect_to_contact,
            send_message,
            my_identity,
            list_contacts,
            add_contact,
            remove_contact,
            set_contact_blocked,
            disconnect,
            send_pointer,
            annotate,
            send_chat,
            send_clipboard,
            file_begin,
            file_chunk,
            file_end,
            request_keyframe,
            request_control,
            is_controlling,
            input_pointer_move,
            input_pointer_button,
            input_pointer_wheel,
            input_key,
            input_set_lock_state,
            start_sharing,
            stop_sharing,
            show_main_window,
            respond_consent,
            set_clipboard_allowed,
            set_audio_allowed,
            respond_control_consent,
            respond_file_offer,
            check_for_updates,
            install_update,
            read_diagnostics,
            list_online,
        ])
        .setup(|app| {
            log::info!(
                "Casual RAS {} started on {}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            );
            let consent = Arc::new(LocalConsent::new(app.handle().clone()));
            // Open the durable contacts book under the app data dir (created if absent). Best-effort:
            // if the data dir is unavailable the app still runs, contacts just aren't persisted.
            let contacts = app
                .path()
                .app_data_dir()
                .ok()
                .and_then(|dir| {
                    let _ = std::fs::create_dir_all(&dir);
                    ras_identity::FileContactBook::open(dir.join("contacts.rcb")).ok()
                })
                .map(Arc::new);
            // Always-on reachability (ADR-098, bidirectional contacts): run the host accept loop from
            // startup so a saved contact can reach this machine by name WITHOUT it clicking "Share"
            // first — true two-way contacts. Every incoming connection is still gated by local
            // Allow/Deny consent (Invariant 1) and mints a fresh endpoint-bound grant (Inv 3); the
            // accept loop merely listens. The user can turn reachability off with Stop and back on with
            // Share. Only started where a capture backend exists (the accept loop serves screen frames).
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

            // ── Presence identity + always-on endpoint + gossip (ADR-094) ──
            // Read the persistent identity seed ONCE (the endpoint id == the contact id == the beacon
            // signing key). `me`/`presence_ks` are the pair that drives every presence topic + signed
            // beacon. All optional: any failure ⇒ presence off, everything else on (FAIL-SAFE).
            let id_seed = identity_seed(app.handle());
            let (me, presence_ks): (
                Option<ras_identity::ContactId>,
                Option<Arc<dyn ras_identity::KeyStore>>,
            ) = match id_seed {
                Some(seed) => {
                    use ras_core::identity::{KeyStore, SoftwareKeyStore};
                    let ks = SoftwareKeyStore::from_seed(seed);
                    let cid = ras_identity::ContactId::from_bytes(ks.public_key());
                    (
                        Some(cid),
                        Some(Arc::new(ks) as Arc<dyn ras_identity::KeyStore>),
                    )
                }
                None => (None, None),
            };

            // Bind the always-on endpoint here (with the persistent seed) and attach a Gossip actor to
            // the SAME endpoint, so one endpoint serves session + bootstrap + gossip. Only where a
            // capture backend exists (that's where the always-on accept loop runs). Every step is
            // fail-safe: a bind/spawn failure leaves `endpoint`/`gossip` = `None` → presence off, and
            // `run_share` simply isn't started (no always-on reachability that run), never a crash.
            #[allow(unused_mut)]
            let mut endpoint: Option<Arc<ras_transport_iroh::Endpoint>> = None;
            #[allow(unused_mut)]
            let mut gossip: Option<iroh_gossip::net::Gossip> = None;
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            {
                let bound = tauri::async_runtime::block_on(async {
                    match id_seed {
                        Some(seed) => ras_transport_iroh::Endpoint::bind_with_key(&seed).await,
                        None => ras_transport_iroh::Endpoint::bind().await,
                    }
                });
                match bound {
                    Ok(ep) => {
                        let ep = Arc::new(ep);
                        // Attach gossip to the raw endpoint (ADR-094). `spawn` returns a `Gossip`
                        // directly; if it ever fails to build, presence stays off while sharing/contacts
                        // still run on this same endpoint (FAIL-SAFE — set below only on success).
                        gossip = Some(iroh_gossip::net::Gossip::builder().spawn(ep.raw()));
                        endpoint = Some(ep);
                    }
                    Err(_) => {
                        log::error!("startup: always-on endpoint bind failed — reachability off");
                    }
                }
            }

            let presence = Arc::new(Mutex::new(ras_signal::presence::PresenceTracker::new()));
            let presence_handles = Arc::new(Mutex::new(std::collections::HashMap::new()));

            app.manage(AppState {
                session: Mutex::new(None),
                share: ShareState {
                    session: Mutex::new(Some(ShareSession {
                        stop: stop_tx,
                        host: None,
                    })),
                    consent: consent.clone(),
                },
                contacts,
                endpoint: endpoint.clone(),
                gossip: gossip.clone(),
                me,
                presence_ks,
                presence: presence.clone(),
                presence_handles,
            });

            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            {
                if let Some(ep) = endpoint.clone() {
                    let reach_app = app.handle().clone();
                    let reach_gossip = gossip.clone();
                    // Route inbound SIGNAL_ALPN messages contacts-only (ADR-095). Read the book back
                    // from managed state (the local `contacts` was moved into `AppState`).
                    let reach_contacts = app.state::<AppState>().contacts.clone();
                    tauri::async_runtime::spawn(async move {
                        run_share(
                            reach_app,
                            stop_rx,
                            consent,
                            ep,
                            reach_gossip,
                            reach_contacts,
                        )
                        .await;
                    });
                } else {
                    // No endpoint ⇒ no always-on accept loop this run; contacts + Connect still work.
                    let _ = (stop_rx, consent);
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            {
                let _ = (stop_rx, consent); // no capture backend ⇒ no accept loop
                let _ = (&endpoint, &gossip);
            }

            // Spawn live presence per saved active contact + the poll→UI loop (ADR-094). All fail-safe:
            // if gossip is off these are no-ops; one bad contact never breaks the others or the app.
            {
                let state = app.state::<AppState>();
                if gossip.is_some() {
                    if let Some(book) = state.contacts.as_ref() {
                        use ras_identity::ContactBook;
                        for c in book.list() {
                            if !c.blocked {
                                spawn_presence_for(&state, c.id);
                            }
                        }
                    }
                    // The poll loop emits `presence` UI events on state changes for the app's lifetime.
                    let poll_app = app.handle().clone();
                    tauri::async_runtime::spawn(presence_poll_loop(poll_app, presence.clone()));
                }
            }

            // Keep the overlay hidden at startup. Do NOT call `set_ignore_cursor_events` here: on
            // Linux/GTK it does `window.window().unwrap()`, which panics (non-unwinding → aborts the
            // whole app) because a not-yet-shown window has no realized GDK window. Click-through is
            // instead set right after the overlay is shown (see the Share path), when it is realized.
            if let Some(ov) = app.get_webview_window("overlay") {
                let _ = ov.hide();
            }

            // "Secure window": keep our own windows out of any screen capture / recording (macOS +
            // Windows; Linux no-op). The consent dialog, in-session chat, clipboard preview, a
            // pairing code, and — on the Connect side — the remote screen feed itself must not leak
            // into a recording or the shared stream. Invariant 7 holds: this hides the windows from
            // capture, NOT from the local user's own screen, so the "REMOTE … ACTIVE" indicator and
            // Stop control stay visible to the human. Uses the native window handle (no GTK
            // realization requirement), so it is safe to call before the windows are shown.
            for label in ["main", "overlay"] {
                if let Some(w) = app.get_webview_window(label) {
                    secure_window::exclude_from_capture(&w);
                }
            }

            // Ask for notification permission once, up front, so the first inbound request can raise
            // a system notification (see `alert_user`). Best-effort — a denied/undecided state just
            // means notifications are skipped; the in-app prompt + window focus still fire.
            {
                use tauri_plugin_notification::NotificationExt;
                let _ = app.handle().notification().request_permission();
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Casual RAS")
        .run(|handle, event| match &event {
            // Closing the `main` window — the sole home of the in-app REMOTE-ACTIVE indicator and the
            // Stop button — must halt any active share (Invariant 7). The always-on-top `overlay` window
            // would otherwise keep the process (and the detached capture→stream loop) alive with no
            // visible indicator. Stop first (synchronous, in-process), then exit so no headless share
            // lingers. `ExitRequested` covers Cmd-Q / quit-menu paths for the same reason.
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } if label.as_str() == "main" => {
                if stop_active_share(handle) {
                    // A share was live. Let the window close, but keep the process alive briefly so the
                    // detached `run_share` task can observe the stop and flush its `Bye{Revoked}` + audit
                    // to the viewer before we exit — the task isn't joined by `exit`, so an immediate
                    // `exit(0)` would race it and the viewer would see a bare transport drop. The grace is
                    // well above the host's internal `BYE_FLUSH_GRACE` (~50 ms). Capture stops regardless
                    // (the stop signal halts the media pump; process exit is the backstop).
                    let h = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        h.exit(0);
                    });
                } else {
                    handle.exit(0);
                }
            }
            tauri::RunEvent::ExitRequested { .. } => {
                stop_active_share(handle);
            }
            _ => {}
        });
}
