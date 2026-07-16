# 20 — Feature Gaps & Roadmap (where we lapse vs. the incumbents)

> **Status:** research-backed design note (2026-07). A catalog of the features mature remote-access
> products ship that Casual RAS does **not** yet have — clipboard sync, file transfer, audio, multi-
> monitor, chat, whiteboard, auto-update, unattended access, address books — with, for each: how the
> incumbents do it, a **safe design that fits our invariants**, the invariant-friction, and a priority.
> Grounded in the cross-device research (RustDesk/TeamViewer/AnyDesk/Parsec/CRD) and their CVE record.
>
> **The through-line:** almost every one of these features has shipped as a **CVE** in the incumbents,
> and each CVE lands on exactly the invariant we already hold. So the gap is not "we're behind" — it's
> "we add these in a shape the incumbents didn't, deny-by-default and host-enforced." Two of these are
> **not gaps but deliberate refusals** (session recording, browse-anywhere file transfer) — see §4.
>
> **Feeds:** ADRs in `docs/14` (each feature needs one), `docs/15` (fraud posture), `docs/16` (tiers),
> `docs/19` (platform mechanics). Backed by a five-stream cross-device research sweep (keyboard-layout,
> display/coordinates, clipboard/file/audio, mobile/touch, identity/discovery) — all complete; their
> most important conclusion is that our **core input + coordinate spine is validated** (physical-HID
> keyboard = the Chrome-Remote-Desktop model; normalized-per-display coords avoid RustDesk's DPI bug
> class), so the gaps below are *additions in the right shape*, not a redesign.

---

## 1. The gap table

Friction = how hard the feature fights the Non-Negotiable Invariants. Priority = product value ÷ risk.

| Feature | Today | Incumbents | Friction | Priority | Needs |
|---|---|---|---|---|---|
| **Audio forwarding** | ❌ | RustDesk/Parsec Opus; AnyDesk on-by-default | **Low** | **P1** | Opus media sub-stream; `audio.system.play` cap |
| **Cursor-shape channel** | ❌ (cursor in video) | Out-of-band, client-rendered (all but game-streamers) | **Low** | **P1** | `CursorShape` control msg, id-cached |
| **Multi-monitor** | ◐ single-display | Monitor picker + switch (all) | **Low** | **P1** | monitor enumeration + selection protocol |
| **Clipboard sync** | ❌ (withheld) | All; TeamViewer most granular | **Medium** | **P1** | `clipboard.text.push`; **no-auto-paste rule** |
| **Chat (in-session text)** | ❌ | TeamViewer/RustDesk/AnyDesk | **Low** | **P2** | content-free-of-logs text channel |
| **Whiteboard / annotation** | ◐ **partial-done** | TeamViewer/Zoom | **Low** | **P2** | extend the existing overlay |
| **Auto-update** | ❌ (unsigned alpha) | All auto-update | **Medium** (supply-chain) | **P1** | signed updates (Tauri updater + Ed25519), key in HSM |
| **Unattended access** | ❌ | All (password/account) | **High** | **P2** | Tier-gated standing grant, revocable |
| **Address book / persistent pairing** | ❌ | All (account/self-host) | **Medium** | **P2** | host-side paired-controller registry |
| **File transfer** | ❌ (withheld) | Dual-pane browsers | **Highest** | **P3** | **signed-catalogue only**, never controller paths |
| **Mobile controller** | ❌ | All native apps | **Medium** | **P3** | relative-pointer + `keyboard.text` + gesture translator |
| **Lock-state sync** (Caps/Num) | ❌ | CRD authoritative state | **None** | **P1** | authoritative lock state, host slaves to controller |
| **Cmd↔Ctrl remap** | ❌ | Parsec/TeamViewer swap | **Low** | **P1** | explicit primary-modifier remap policy |
| **Unicode / IME text** (`keyboard.text`) | ◐ cap exists, withheld | CRD `TextEvent`, RustDesk Translate | **Medium** | **P2** | gated, never-logged Unicode commit path |
| **Session recording** | ✗ **by design** | All record | — | **never** | intentional refusal (Inv 12) |

Cross-cutting rule for **every** row: a **deny-by-default named capability** (Inv 2), inside the signed
grant ceiling, behind **its own lease** (never implied by a view/control grant — CVE-2026-58056 is
exactly one grant leaking into another channel), enforced **per-message host-side** (Inv 15), content
crosses only the authorized session boundary and **never** touches logs/traces/verdicts (Inv 8/11), an
**always-visible active-use indicator** (Inv 7), and an **ADR** in `docs/14`.

---

## 2. The near-term wins (P1 — low friction, high value)

### 2.1 Audio forwarding — *the easiest gap, architecturally native*
- **Incumbents:** RustDesk (Opus LowDelay, 10 ms/48 kHz) and Parsec (Opus/RAW) converge on **Opus**; capture is WASAPI-loopback (Windows), PulseAudio/PipeWire (Linux), and — the key finding — **ScreenCaptureKit** on macOS (system audio rides the *same `SCStream` we already use for video capture*).
- **Safe design:** an **Opus sub-stream** over the existing session media transport. Capabilities: `audio.system.play` (host→controller) and, **strictly separate**, `audio.mic.capture` — never conflate them (RustDesk's hot-mic bug #8718 was exactly conflation: transmitting the *input* device by default). System audio is genuine session media, like pixels — same content boundary (Inv 8/11), no *new* boundary crossed. Permissive `opus`/`libopus` (BSD) clears Inv 18; do **not** vendor RustDesk's `magnum_opus` wrapper.
- **Friction: low.** Playback is low-risk and reuses the media pipeline. Mic capture is the one caveat — distinct capability, prominent "MIC LIVE" disclosure (Inv 7), deferred harder. **Build audio first.**

### 2.2 Multi-monitor — *the coordinate model already supports it (research-confirmed)*
- **Today:** single-display MVP (`display id 0`), but the hard part is **already done** — coordinates are **normalized per named display**, `HostSession` emits `CaptureGeometry` bounds, and `ScreenCaptureBackend::captured_bounds` reports the shared display's global rect. `layout_version` already invalidates stale coordinates (`StaleLayout`).
- **The display research validated this spine strongly:** because the *host* resolves normalized→pixels against its *own current* capture geometry, there is **no stale client-side scale factor to drift** — which is RustDesk's #1 bug class (its absolute-host-pixel model produces a documented DPI-misalignment cluster at 175% scaling / Wayland fractional). Normalizing to `[0,1]` of the video rect gets **AnyDesk-grade correctness without mutating the host's display config**. *(Note: our normalized-0..65535 is a sound own-protocol choice — the RDP *wire* mouse event is actually 16-bit pixel-absolute; 0..65535 is the Win32 `SendInput` injection convention. Don't justify it with "RDP does it on the wire.")*
- **What's missing** (ranked from the research): (a) **enumeration** — advertise displays as a **signed virtual-desktop** `MonitorDef{id, left, top, right, bottom, primary, scale}` list (negative origins for left/above — the universal convention: RDP `TS_MONITOR_DEF`, RustDesk `DisplayInfo`, Sunshine `offset_x/y`); (b) **selection** — `SelectDisplay{id}` + a `Displays[]` announce in the peer info; (c) **switching** mid-session (bump `layout_version`, re-key capture). **Design the signed multi-monitor coord space now** so adding monitors later isn't a wire migration.
- **HiDPI metadata (Rank 2 from the research):** the normalized model makes *clicks land* regardless of DPI (its strength), but the controller can't render **crisp, correctly-sized** output without the host's scale — CRD's hardcoded-96-DPI is the cautionary tale. Extend `CaptureGeometry` with `logical_w/h` **and** `pixel_w/h`, a `scale_factor`, optionally `physical_mm` + `orientation` (RDP's model). This also lets the controller correctly fold its own browser `devicePixelRatio` when normalizing input.
- **Aspect / letterbox (Rank 4):** the controller must subtract letterbox centering before normalizing (RustDesk's `input_model.dart` does exactly this), else a click in a black bar normalizes to a bogus in-rect coordinate. Carry the capture-rect aspect; the host defensively **rejects** normalized coords from implausible regions.
- **Explicitly NOT recommended: host-resolution matching / virtual displays.** The industry's dominant "resize host to client" strategy **mutates the local owner's display config** (conflicts with Inv 1), and the permissive building blocks are uneven (Windows MIT `parsec-vdd` wraps a proprietary driver; macOS only has the *private* `CGVirtualDisplay` — App-Store-blocking). Our normalize-against-live-geometry approach is the better fit; revisit virtual displays only for a headless/cloud-host track.
- **Friction: low.** Pure additive protocol + capture-target selection. **P1.**

### 2.5 Cursor-shape channel — *the gap the display research surfaced (Priority-2 bug today)*
- **The problem:** the host's **own** cursor currently appears only inside the encoded video, so under any video stall or compression it lags and blurs — a direct **Priority-2 (latency)** violation. (We already draw the *viewer's* remote pointer on a host overlay, ADR-061 — but not the host's real OS cursor on the *controller*.)
- **The universal fix:** every desktop-grade tool sends the cursor **shape out-of-band as a low-rate, id-cached control message** and composites it **client-side** at zero latency — RFB `-239`, SPICE cursor channel (cached by `unique`), RDP `TS_CACHEDPOINTERATTRIBUTE`+`hotSpot`, CRD `CursorShapeInfo`, RustDesk `CursorData{hotx,hoty}`. Only game-streamers (Sunshine) bake it into the frame — and consequently *can't* show shape changes (arrow→I-beam), a documented limitation.
- **Safe design:** a `CursorShape{ id, hotspot_x, hotspot_y, w, h, rgba }` + `CursorHidden` control message, cached by `id` (send common shapes once), rendered on the existing WebCodecs canvas; keep a `cursor_embedded` fallback flag for backends that can't exclude the HW cursor. It's host→controller **display** data, not input — squarely outside Inv 6.
- **Friction: low.** Highest value-to-effort of the display gaps; directly serves Priority 2. **P1.**

### 2.6 Cross-device input correctness (keyboard) — *our design is validated; three gaps to complete it*
The keyboard research **confirmed our core choice is sound and well-precedented**: physical **USB-HID usage 0x07 + modifier bitset**, host-mapped after authorization, with a **separate, withheld `keyboard.text` Unicode capability**, is *exactly* the Chrome Remote Desktop architecture — the most security-conscious mainstream design — and **cleaner than RustDesk's Map mode**, which ships *platform-native* position codes (`chr` = "win: scancode, linux: keycode, macos: keycode") needing OS→OS translation, where our canonical USB-HID code space is one space for all OSes. The controller reads `KeyboardEvent.code` (positional, defined against the HID table) → static-maps to HID usage, never `event.key`. It's a closed, fuzzable, fail-closed enum, not an unbounded keysym/text string. **Keep it.** But complete it:
- **Lock-state sync — P1, no security risk (functional bug guaranteed today).** Forwarding the CapsLock/NumLock *key edge* between two independently-stated machines guarantees drift (every VNC/RDP/Sunshine tracker proves it — stuck-Shift, inverted-Caps). **Fix:** carry authoritative `caps_lock`/`num_lock` **state** booleans in the input envelope; the host **slaves** its lock state to the controller's (CRD's model). Small, closed, enum-shaped, fits Inv 6.
- **Cmd↔Ctrl (primary-modifier) remap — P1, UX (mildly security-relevant).** Without it, a Mac controller's ⌘C lands as **Win+C** on a Windows/Linux host and Mac muscle-memory fails (⌘/Win/Super is one HID usage 0x0700E3 with three meanings). **Fix:** a controller-side, host-OS-aware toggle ("use Mac shortcuts") that rewrites *which HID usage* is sent for the primary-shortcut modifier — a **policy above passthrough**, scoped to only the primary modifier, **explicit and user-visible** (never silent), deterministic/auditable. Still a closed enum, no new wire surface. (Parsec/TeamViewer both ship this.)
- **Promote `keyboard.text` (Unicode/IME) to a real gated mode — P2, security-SENSITIVE.** The positional path **cannot** do CJK/emoji/accented composition (IME lives *above* the keycode layer — no HID usage "is" 你); those users are blocked today. The capability exists and is correctly withheld. Making it first-class requires: (a) **separate deny-by-default capability + its own lease bit** — it's a broader "type-anything-into-focus" authority (effectively scripting if focus is a terminal); (b) **Inv 8 — the field is literal plaintext** (passwords/PII): never logged/traced/audited-as-content; audit records only a content-free "text injected" event; (c) UTF-8-scalar-validated, length/rate-bounded, no control-char smuggling. This is the RustDesk-Translate / CRD-`TextEvent` analogue — the right shape, invariants enforced at compile time where possible. **Also unblocks the mobile controller** (§3.6), where soft-keyboard Unicode is unavoidable.
- **Do NOT add** a raw keysym-string channel or any free-form input field — the unbounded surface Inv 6 forbids, buying nothing over HID + gated-Unicode.

### 2.3 Clipboard sync — *moderate, gated on one hard rule*
- **Incumbents:** all sync text (+ images/files); TeamViewer is most direction-granular (2-way / local→remote / remote→local / off, + "paste as keystrokes"); AnyDesk splits text vs files into two perms. **The CVE record is damning:** Check Point's Reverse-RDP showed a malicious *host* can silently read the controller's clipboard *and* push content the user never copied, chained with file-drop path traversal to RCE. RustDesk leaks: pre-connection clipboard syncs (#9010), cross-session bleed (#7346).
- **Safe design:** `clipboard.text.push` as **two separate direction capabilities** (direction *is* a capability). **The one hard rule: no auto-paste, ever.** Sync is an **explicit push** (the user hits "send clipboard"); the receiver **only populates the OS clipboard — never injects a paste keystroke.** Auto-paste + input injection is the hijack-to-RCE chain; keeping paste a manual local act severs it. Guardrails: size cap (≈1 MiB, like `MAX_CONTROL_FRAME`), echo-suppression ownership tag (RustDesk's pattern), pre-connection clipboard **never** auto-synced, "clipboard shared" indicator (Inv 7), default **off**. No `clipboard.files` (that's file transfer — §3.3, don't smuggle files through the clipboard). CRLF/LF translation is undocumented in all five tools — leave bytes as-is (normalizing corrupts non-text).
- **Friction: medium.** Invariant-compatible **iff** auto-paste is forbidden and direction is per-capability. The moment it can paste, it's an Inv 6 injection vector. **P1** (behind the no-auto-paste rule).

### 2.4 Auto-update — *low effort, high supply-chain stakes*
- **Incumbents:** all auto-update — and this is where two of them were **breached**: **AnyDesk 2024** (build/sign systems compromised, **code-signing cert + keys stolen**, used to sign 500+ malware samples) and the **fake `rustdesk[.]work`** site shipping the genuine binary + a backdoor. The update/signing channel *is* the supply chain.
- **Safe design (two independent signing layers — ADR-072):**
  - **Update-integrity signing — free, use from day one.** Tauri v2's **updater plugin with Ed25519/minisign signature verification**: the client verifies the release signature before applying and the **update public key is pinned in the app**. This keypair is **self-generated, no CA, no cost** — so an unsigned OS build still has *verified updates*. Private key **off build machines** (CI secret store now → HSM once funded), rotate-able. CycloneDX **SBOM per release** + `cargo-deny` + reproducible builds harden the chain. For the **white-label/embedded** case the *host app* controls update cadence — never silently self-update inside someone else's product.
  - **OS code-signing / notarization — DEFERRED until a GitHub sponsor funds it (ADR-072).** Apple notarization + Windows EV cert are what stop Gatekeeper/SmartScreen warnings; they cost recurring money we don't have pre-revenue, so **alpha builds ship unsigned** (users allow the app past the OS warning; download page shows the fingerprint for out-of-band verification — Inv 17). A config/CI flip, no code impact, the moment funding lands.
- **Friction: medium** — not against a runtime invariant, but the highest-consequence-if-wrong item here (a compromised updater is total). **P1**, built with **update-signature verification from day one** — never an *un-integrity-signed* auto-updater, even while the OS-level signing stays deferred.

---

## 3. The deferred / higher-friction features

### 3.1 Chat (in-session text) — P2, low friction
Simple text channel between the two peers during a session. Design: content-free-of-logs (Inv 8 — chat text is content, never logged), a bounded message size, an ADR. Low risk; mostly UI. Useful for the support use-case ("click the button top-right"). **P2.**

### 3.2 Whiteboard / annotation — P2, **partly already built**
Not a full gap: the app **already** has viewer-side annotation + an overlay **remote pointer** drawn on the host's screen (ADR-061). The gap is persistence/richness (shapes, host-side draw-back, multi-user annotation). Extend the existing transparent overlay + `Pointer` channel (visual, outside Inv 6/14 by design). **P2.**

### 3.3 File transfer — P3, **fights the invariants hardest**
- **The danger channel.** Three distinct recent RustDesk CVE classes, all on our threat model: **path-traversal/zip-slip** (`FileEntry.name` with `../`/absolute/drive-letter — PR #14678), **symlink-follow arbitrary read as SYSTEM** (CVE-2026-2490, whose own fix admits path-string checks are TOCTOU-prone → needs `openat`/`O_NOFOLLOW`), and the **FileTransfer session injecting input/reaching screenshots** because per-capability flags weren't cleared (CVE-2026-58056 — Invariant 15 *verbatim*).
- **A controller writing arbitrary host paths is exactly what Inv 6 forbids.** So we do **not** build the dual-pane browse-anywhere file manager (violates S7 + Inv 6).
- **Safe design — bend it to the signed catalogue (S7):** the vendor pre-declares **named drop targets** (e.g. `deliver_config_bundle` → a fixed sandboxed dir); the controller may only invoke a catalogued action with **schema-validated args — never a free-form path**; the host resolves the destination. Capability `file.push.<catalogued-target>`, per-transfer local confirmation, size/rate cap, `openat`/`O_NOFOLLOW` writes, reject `..`/absolute/drive-letter/null in any filename, and the file cap **never** confers input/capture (Inv 15). **P3** — and only in this shape; the convenient version stays rejected.

### 3.4 Unattended access — P2, high friction (needs the identity work first)
- **Incumbents:** permanent password (RustDesk), Easy-Access/account (TeamViewer — whose 2016 credential-stuffing wave and shared-group amplification show the wrong shape), PIN+account (CRD).
- **Safe design (opposite of a standing password):** a **standing signed grant the host pre-authorizes once**, bound to a **paired controller key**, that is **Tier-gated** (Tier ≥1 TPM-attested required; software-fallback capped at Tier 0 — Inv 16, already in `docs/16`), **short-lived + auto-renewing + endpoint-bound** (never permanent — Inv 3), **enumerated capabilities enforced per-message** (Inv 2/15), **emergency-stop-overridable** (Inv 4), and **revocable** by removing the key. Unattended just means the *issuer* pre-authorizes without a live click — which *raises* the bar on expiry/scope/revocation. **P2, after §3.5.**

### 3.5 Address book / persistent pairing — P2, medium friction (enables §3.4)
- **Highest-leverage identity add.** After a first attended, consented session, the host **persists the controller's Ed25519 pubkey in a local allow-list**; future sessions from a known controller skip re-pairing but **still mint a fresh short-lived grant and still honor emergency stop**. Model identity **Syncthing-style (ID = hash of the pubkey)** so re-pairing detects key changes structurally. Preserves Inv 1 (host owns the list), Inv 3 (session still gets a fresh grant), Inv 9 (registry authenticates identity; grant is authority). Pairs with **host-displayed QR** (host shows, controller scans — the strict direction that avoids the Signal-QR-hijack coached-victim vector). **P2.**

### 3.6 Mobile controller — P3, medium friction
From the mobile research: needs a **relative-pointer `InputAction`** (absolute-tap is unusable on a phone), the **`keyboard.text` Unicode/IME cap** (soft keyboards emit composed CJK/emoji that can't be HID scancodes — becomes *essential* here), a **client-side touch-gesture→closed-action translator** (host only ever sees clicks/wheel/moves — preserves Inv 6), a virtual-key toolbar with sticky modifiers, and **client-side-only zoom/pan**. Must be a **native app** (iOS WebKit has no Keyboard/Pointer Lock — why CRD ships native, not a PWA). The WebRTC/PWA path stays the deferred ADR-057 track. **P3.**

---

## 4. Not gaps — deliberate refusals (state these proudly)

These are features the incumbents ship that Casual RAS **chooses not to build**, and should say so honestly (Inv 17):

- **Session recording / content at rest** — the fraud subsystem keeps **zero content at rest** (Inv 12): no screenshots, no keystroke logs, no session recording. This is a differentiator for regulated buyers, not a missing feature.
- **Browse-anywhere file transfer** — rejected (S7 + Inv 6); only the signed catalogue (§3.3).
- **Secure-desktop / UAC input injection** — refused (Inv 14), now also OS-enforced (Windows Jan-2026 hardening). UAC prompts and the login screen are **not** remotely controllable, by design.
- **Unattended access on software-only key storage above Tier 0** — capped (Inv 16); no phishable factor recovers a phishing-resistant one.
- **Any auto-paste, clipboard-triggered keystroke, or controller-supplied path** — structurally forbidden.

---

## 5. Suggested sequencing

Grouped by dependency and rising friction. Each item = its own ADR + capability + lease + indicator.

1. **Wave 1 (P1, low friction, ship-enabling):** **cursor-shape channel** (§2.5 — Priority-2 fix) · **audio** (Opus sub-stream) · **multi-monitor** (signed coord model + enumeration/selection + HiDPI metadata) · **lock-state sync** + **Cmd↔Ctrl remap** (§2.6 — input correctness) · **clipboard** (text push, no-auto-paste) · **auto-update** (signed, Ed25519, HSM key). The visible "parity" wins; none fight the invariants hard, and cursor-shape + lock-state are outright correctness bugs today.
2. **Wave 2 (P2, builds the trust/identity layer + IME):** **persistent paired-controller registry** (§3.5) → **QR pairing** → **unattended access** (§3.4, Tier-gated) · **`keyboard.text` Unicode/IME** (§2.6 — gated, never-logged) · **chat** · **whiteboard** extension. The registry unlocks unattended safely; `keyboard.text` unblocks CJK + mobile.
3. **Wave 3 (P3, high friction / large surface):** **file transfer** (signed catalogue only) · **mobile controller** (relative-pointer + gesture translator, on top of Wave-2 `keyboard.text`) · **relative-pointer capability** (also for games/CAD) · the **directory/rendezvous control plane** (Tailscale-shaped — distributes keys, never authority; authenticate every registration — the RustDesk-CVE-2026-30784 anti-pattern) · **WebRTC/PWA** embedding track (ADR-057).

**Every wave keeps the same discipline:** deny-by-default capability, own lease, per-message host enforcement, content off logs, visible indicator, ADR. That discipline is *why* we can add the incumbents' feature set without inheriting the incumbents' CVE set.

---

## 6. Sources

Backed by a completed five-stream cross-device research sweep (full source lists live in each stream's
report). Key CVEs/incidents referenced: RustDesk **CVE-2026-57850 / CVE-2026-58056** (session-scope not
enforced host-side → keyboard/mouse injection from a FileTransfer session — the exact Inv-15 class our
`authorize_input` gate defends), **CVE-2026-2490** (file-transfer symlink read), **CVE-2026-30784**
(unauth rendezvous), PR #14678 (path traversal), issue #8718 (hot-mic); **Check Point Reverse-RDP**
(clipboard read + CF_HDROP RCE); **AnyDesk 2024** code-signing-cert breach; **TeamViewer 2016**
credential-stuffing. Protocol grounding: Chrome Remote Desktop `event.proto` (USB-HID `KeyEvent` +
`caps/num_lock_state` + `TextEvent`), RustDesk `KeyboardMode` (Legacy/Map/Translate), W3C UI-Events
`code` vs `key`, USB HID Usage Tables v1.21; RFB `-239`/SPICE/RDP cursor + monitor + DPI models
(MS-RDPEDISP scale factors). RustDesk/Sunshine are AGPL/GPL — **study-only** (Inv 18); no code linked
or vendored.
