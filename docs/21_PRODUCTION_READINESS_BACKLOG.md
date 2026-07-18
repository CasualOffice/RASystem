# 21 — Production-Readiness Backlog (competitive gap analysis)

> Generated 2026-07-18 from a multi-agent deep-research pipeline benchmarking Casual RAS against
> RustDesk (study-only, AGPL — techniques compared, never copied per Inv 18), AnyDesk, TeamViewer,
> Chrome Remote Desktop, Parsec, and Moonlight/Sunshine. Graded on **production behavior**, not
> "compiles + loopback-green." This is the source-of-truth backlog for the production-hardening
> campaign; the live task tracker mirrors its P0/P1 items. Supporting per-competitor research and the
> requirements checklist are archived alongside in the workflow transcript.

This confirms the critical reality: the state machine has `Suspended` but **re-dial itself is deferred** ("Phase 1 has no re-dial"). That's a concrete P0. I have enough verified ground truth. Let me write the backlog.

I have enough verified ground truth to write the backlog. Key confirmations: 20 crates exist as claimed; the session state machine has `Suspended`/reconnect-window states but **actual re-dial is a stub** (`// Re-dial itself is deferred to the iroh` / "Phase 1 has no re-dial") — so reconnection is a UI freeze that always terminates, not a real reconnect. That's a load-bearing P0 the CLAUDE.md status glosses over.

---

# Casual RAS — Prioritized Production-Grade Gap Backlog

**Verdict up front:** Casual RAS has a genuinely strong, differentiated *security core* (signed grants, per-message capability enforcement, hash-chained audit, honest consent) — ahead of every incumbent on the authorization axis. But it is **not a shippable cross-OS product**. The gap is almost entirely in the boring, load-bearing production plumbing: **on-device verification, session reconnection, DPI/reconnect/black-screen correctness, distribution trust, and Wayland**. Two crates deep on security; one inch deep on "does it survive a Wi-Fi blip."

**Honesty note on our own status doc:** CLAUDE.md describes many features as "landed" when they are *code-level spines with loopback tests* — never run on device, never over a lossy network, never on real Windows/Linux hardware. This backlog is graded on **production behavior**, not on "compiles + loopback-green." I verified the reconnection claim directly: `session.rs` has the `Suspended` state and reconnect window, but the re-dial is a stub (`"Phase 1 has no re-dial"` → the window always expires to `Terminated`). Several other "◐ landed" items have the same shape.

**Fixability legend:** `CODE-NOW` (fixable off-device today) · `LINUX-DEV` (needs our Linux hardware on-device run — we have it) · `WIN-HW` (needs Windows hardware we lack) · `$FUND` (needs money: signing certs / audit).

---

## Cross-cutting (capture / encode / input / transport)

### P0 — Ship-blockers

**X1. Real session reconnection (re-dial), not just a freeze-then-die**
*Why it matters:* This is the single biggest lie in the current status. The state machine surfaces `Suspended` on transport loss and keeps the UI live — but there is **no re-dial**; the reconnect window elapses and the session goes `Terminated`. Every competitor (RustDesk, AnyDesk, TeamViewer, CRD, Parsec) auto-restores a session across a Wi-Fi blip / NAT rebind **without losing grant/lease state**. A remote-access tool that dies on every network hiccup is not shippable. The checklist [TS] "automatic reconnection that restores the session (not a fresh handshake that loses grant/lease state) with a fresh keyframe."
*Severity:* **P0** · *Platforms:* all · *Fixability:* `CODE-NOW` (iroh path migration + re-dial loop + forced IDR on restore; grant is endpoint-bound so re-dial must re-prove endpoint without re-consent within the window).
*Done:* Pull the plug on the network mid-session; within the reconnect window the controller goes `Suspended`→`Active`, video resumes with a fresh keyframe, the existing lease/grant survives (or is deliberately re-validated), and an audit event records the reconnect. Tested over a real impaired link (loss/latency/NAT rebind), not loopback.

**X2. No-black-screen guarantee on connect / reconnect / resume**
*Why it matters:* Black-screen-on-(re)connect is the #1 reported failure for RDP, AnyDesk, Parsec. It must be *structurally impossible*: every session start, resume, and monitor/resolution change forces a fresh IDR + capture rebind. We have forced-IDR-on-demand in the codec, but no wired guarantee that a resumed/reconnected/decoder-reset path always re-requests one before showing frames.
*Severity:* **P0** · *Platforms:* all · *Fixability:* `CODE-NOW`.
*Done:* A decoder that joins/rejoins/resizes never renders a stale or garbage frame; the first visible frame after any resync is a keyframe. Covered by a fault-injection test (decoder reset, transport cut+restore, mid-session config change).

**X3. On-device input verification on the two platforms we own (macOS + Linux)**
*Why it matters:* Input injection on **all three** backends is compile-checked/loopback-only. Injection is the highest-consequence, most environment-sensitive path (TCC, Secure Input, X11 grabs, focus, stuck modifiers). "Cross-compile-clean" tells you nothing about whether a keystroke lands. Until a real remote peer drives a real host and we watch a character appear, Share is not verifiable.
*Severity:* **P0** · *Platforms:* macOS, Linux · *Fixability:* macOS `CODE-NOW` (we have Macs), Linux `LINUX-DEV`.
*Done:* On macOS: live CGEvent injection with the PostEvent-TCC prompt appearing, Secure-Input correctly dropping injection in a password field, `release_all` clearing modifiers on teardown. On Linux/X11: XTEST injection into a real X/Xwayland session, fail-closed when no X server. Both recorded end-to-end from controller keypress → host action.

**X4. Full keyboard coverage + lock-state + modifier-reconciliation verified live**
*Why it matters:* Checklist [TS]: F13–F24, nav cluster, numpad w/ NumLock, non-character keys, HID-usage map (not char map), authoritative lock-state sync, `release_all` on focus loss. The wire + gate + per-backend overrides exist (ADR-074), but "taps only on mismatch" and "no stuck modifier" are exactly the things that only fail on real hardware. A stuck Ctrl or drifted CapsLock is an instant credibility-killer.
*Severity:* **P0** · *Platforms:* macOS, Linux (Windows deferred to WIN group) · *Fixability:* macOS `CODE-NOW`, Linux `LINUX-DEV`.
*Done:* A live cross-device matrix run (Mac controlling Linux and vice versa): every key class injects correctly; Caps/Num state stays in sync across 100 toggles; killing the controller mid-chord leaves no stuck modifier on the host.

### P1 — Important

**X5. Hardware encode on Linux/Windows (VA-API / Media Foundation / NVENC / AMF) with software fallback**
*Why it matters:* Linux/Windows currently run **software OpenH264 only**. Parsec/Sunshine set the bar with zero-copy HW encode; software H.264 at 60 fps on a 4K desktop will pin a CPU core and blow the latency budget. macOS already has VideoToolbox. The `VideoEncoderBackend` trait + `PlatformSurface` seam exist to slot these in.
*Severity:* **P1** · *Platforms:* Linux, Windows · *Fixability:* Linux `LINUX-DEV`, Windows `WIN-HW` (VA-API buildable/testable on our Linux; MF/NVENC need Windows).
*Done:* Linux VA-API encode path verified on-device at 60 fps with encode latency in single-digit ms and OpenH264 as automatic fallback when no HW encoder is present.

**X6. Mid-session resolution / monitor-hotplug / DPI-change handling verified on device**
*Why it matters:* Checklist [TS], and a top incumbent failure class (Windows' own April-2026 RDP mixed-DPI dialog bug). The wire carries `StreamConfig` atomically with each frame and we have the `MonitorDef`/HiDPI model (ADR-081), but "monitor unplugged mid-session → new config arrives atomically with its keyframe, no torn frame, client re-renders crisp" is unverified. Stale-DPI blur (incumbents cache it) must be structurally re-derived.
*Severity:* **P1** · *Platforms:* all (verify on macOS + Linux) · *Fixability:* macOS `CODE-NOW`, Linux `LINUX-DEV`.
*Done:* Hot-plug/unplug a monitor and change scale mid-session on a real host; controller re-renders crisply with no torn frame, no persistent blur, correct input mapping including negative-coordinate displays.

**X7. Windowed-loss ABR validated under real impairment**
*Why it matters:* We have windowed (non-cumulative) loss + RTT ABR — architecturally correct and a stated differentiator over cumulative-loss incumbents. But it's only unit-tested on synthetic math. Checklist [TS]: "bounded, non-growing latency under congestion; drop-at-source." Must be proven under real jitter/loss to claim it.
*Severity:* **P1** · *Platforms:* all · *Fixability:* `CODE-NOW` (netem/network-link-conditioner on our own machines).
*Done:* Under injected 5% loss + 60 ms jitter, latency stays bounded (no queue growth), bitrate drops then *recovers* when the link clears, and no frame-queue latency build-up. Reproducible benchmark harness committed.

**X8. Relative-pointer / mouse-capture (pointer-lock) path verified**
*Why it matters:* Checklist [TS] for games/3D/CAD. `PointerMoveRelative` (ADR-087) + all three OS overrides landed at the code level but never run. Without a verified raw-delta path, we can't serve the creative/gaming use case Parsec owns.
*Severity:* **P1** · *Platforms:* macOS, Linux · *Fixability:* macOS `CODE-NOW`, Linux `LINUX-DEV`.
*Done:* Pointer-lock in a controller drives raw deltas into a real game/3D app on a real host with correct clamping to the desktop union.

### P2 — Polish

**X9. Connection diagnostics readout (RTT / bw / loss / fps / codec / direct-vs-relay)** — [TS] operability, surfaced from the existing `HealthObserver`. `CODE-NOW`. *Done:* live per-session stats panel + exportable.
**X10. HEVC/AV1 + 4:4:4 for crisp text** — [DIFF] vs RustDesk's AV1. `CODE-NOW`/`LINUX-DEV`. *Done:* optional codec negotiated, text-sharpness A/B better than H.264 baseline.

---

## Windows

*Reality: everything here is compile-gated only; we cannot runtime-verify any of it without hardware. This is an honesty problem as much as an engineering one.*

### P0

**W1. Acquire Windows on-device verification capability (hardware or cloud VM/CI-with-GPU)**
*Why it matters:* Windows is the **production target** (S5), yet **zero** Windows paths (WGC capture, SendInput, file-write `CreateFileW`, clipboard, lock-state) have ever run. "Compile-clean on windows-msvc" is not evidence of function. Every Windows item below is blocked on this. Shipping a Windows host we've never run is indefensible for a security product.
*Severity:* **P0** · *Platforms:* Windows · *Fixability:* `WIN-HW` / `$FUND` (a cloud Windows GPU runner is the cheapest unblock).
*Done:* A repeatable Windows runtime environment (physical, cloud VM, or self-hosted CI runner) where the app launches, captures, and injects — with a real display.

**W2. Windows capture (WGC) + SendInput + file/clipboard end-to-end run** — blocked on W1. *Severity:* **P0** · `WIN-HW`. *Done:* controller drives a real Windows host: video, input, clipboard, file-drop all verified; secure-desktop/UAC correctly refused-and-surfaced (not a silent no-op), matching Inv 14.

### P1

**W3. Session-0 service + session-agent split (S4) for unattended access + login-screen + survive-logoff**
*Why it matters:* Every serious incumbent (RustDesk, AnyDesk, TeamViewer, CRD, Parsec) ships a SYSTEM service + session agent. Our single-user-process MVP posture cannot do unattended access, cannot survive user logoff/switch, cannot reach the login screen. CLAUDE.md itself flags S4 as "the security story is not complete until it is separated." This is the biggest architectural gap vs the field for the IT-support market.
*Severity:* **P1** (P0 for the unattended market, but gated behind W1) · *Platforms:* Windows first, then Linux/macOS analogues · *Fixability:* `WIN-HW`.
*Done:* Service in Session 0 (re)launches per-session agents over authenticated named-pipe IPC; unattended reconnect survives logoff; honest "cannot drive secure desktop" boundary (Inv 14) preserved.

**W4. Windows lock-state (`GetKeyState`) + modifier reconciliation live** — blocked on W1. `WIN-HW`. *Done:* Caps/Num sync verified on a real Windows host.

---

## Linux

*We own Linux hardware — none of this is blocked on funding, only on doing the runs. This is our cheapest path to a second verified host platform.*

### P0

**L1. Full Linux Share on-device run (scap/PipeWire capture + OpenH264 + XTEST input)**
*Why it matters:* Linux Share is "◐ experimental, compile-only." The whole capture→encode→transport→input chain has never run on a real Linux desktop. This is verifiable today with our hardware; not doing it is the gap.
*Severity:* **P0** · *Platforms:* Linux · *Fixability:* `LINUX-DEV`.
*Done:* A real Linux (X11) host shares to a controller: portal consent flows, frames render, XTEST input lands, static-frame coalescing works, portal `Ok(None)` doesn't stall the pump.

**L2. Wayland host support (PipeWire capture + `libei`/RemoteDesktop-portal input)**
*Why it matters:* **This is the single biggest competitive wedge in the entire backlog.** Wayland is default on Ubuntu 22.04+/Fedora/modern GNOME/KDE. AnyDesk **flatly refuses** Wayland host; TeamViewer is "experimental, use Xorg"; CRD/Parsec don't host Linux at all; RustDesk is portal-fragile. Our current `ras-input-linux` is **XTEST/X11-only** — so today we're at *parity* (X11-only), not ahead. Delivering robust Wayland-incoming (capture via portal/PipeWire + input via **libei**, since XTEST can't inject on native Wayland) is the defensible differentiator. libei avoids the uinput/`input`-group privilege friction that RustDesk/Sunshine carry.
*Severity:* **P0** (differentiator that's currently vaporware) · *Platforms:* Linux · *Fixability:* `LINUX-DEV`.
*Done:* On a native Wayland GNOME *and* KDE session: capture via `org.freedesktop.portal.ScreenCast`→PipeWire, input via libei, portal consent + monitor picker handled, restore-token path for re-connect. Degrades with a clear typed error where a compositor lacks the protocol.

### P1

**L3. Compositor-matrix testing (Mutter / KWin / wlroots-Sway/Hyprland)**
*Why it matters:* Checklist [TS] — an app that works on KDE routinely fails on GNOME (different capture protocols: `wlr-screencopy` vs `ext-image-copy-capture`). Capability-detect + degrade-with-message, don't crash.
*Severity:* **P1** · *Platforms:* Linux · *Fixability:* `LINUX-DEV`.
*Done:* CI/manual matrix across ≥3 compositors; each either works or fails with an actionable typed error naming the missing protocol.

**L4. VA-API hardware encode on Linux** — see X5. `LINUX-DEV`.

### P2

**L5. Modern Linux packaging breadth (`.rpm`, Flatpak) beyond `.deb`/AppImage** — [TS] distribution; CRD ships Debian-only, AnyDesk has no AppImage/Flatpak — beating them here is cheap. `CODE-NOW`. *Done:* signed `.rpm` + Flatpak added to release matrix.
**L6. Unattended Wayland / login-screen (DRM-KMS or libei-persistent)** — [DIFF], the hard problem nobody solves cleanly. `LINUX-DEV`. Defer until L2 lands.

---

## macOS

*Our most-mature platform, but "mature" here means capture/encode spike-verified; input and the whole app-integrated path still need on-device confirmation.*

### P0

**M1. macOS Share on-device app run: input injection + TCC + Secure Input**
*Why it matters:* Capture/encode were spike-verified, but the **integrated app** doing live CGEvent injection with the real TCC prompt, Secure-Input drop, and overlay exclusion has not been run end-to-end. macOS is the lead platform and our reference "verified" host — the claim must be true.
*Severity:* **P0** · *Platforms:* macOS · *Fixability:* `CODE-NOW`.
*Done:* Full app session on a real Mac: TCC prompt appears and is handled, injection lands, password field drops injection (Secure Input), host's own overlay/indicator excluded from capture, `release_all` on stop.

### P1

**M2. First-run guided TCC permission flow (Screen Recording + Accessibility)**
*Why it matters:* TCC friction is the documented #1 macOS onboarding failure across every competitor. Need: deep-link to the exact System Settings pane, detect grant state, handle the must-relaunch-after-granting quirk.
*Severity:* **P1** · *Platforms:* macOS · *Fixability:* `CODE-NOW`.
*Done:* A cold-install user is walked to both grants with live state detection and a clean relaunch; no silent black-screen from a missing grant.

**M3. macOS global-hotkey emergency stop**
*Why it matters:* Inv 4 requires emergency stop overriding everything ≤250 ms. macOS has no kernel SAS; the always-visible Stop button exists but a global hotkey that works even when a remote app has focus is the production bar.
*Severity:* **P1** · *Platforms:* macOS · *Fixability:* `CODE-NOW`.
*Done:* A registered global hotkey triggers `revoke_all` + `release_all` within budget regardless of focused app; audited.

### P2

**M4. Unattended access surviving reboot / at login window (LaunchDaemon + MDM PPPC profile story)** — [DIFF]; parallels W3. `CODE-NOW`. Defer behind the unattended-access product decision.

---

## Distribution / Release

### P0

**D1. macOS Developer-ID signing + notarization + stapling**
*Why it matters:* A signed-but-unnotarized app is **Gatekeeper-blocked** outright (not just warned). Currently unsigned (ADR-072) — users must right-click-Open. No credible security product ships Gatekeeper-blocked. TeamViewer/AnyDesk/CRD all notarize. This is a hard ship-blocker for macOS, our lead platform.
*Severity:* **P0** · *Platforms:* macOS · *Fixability:* `$FUND` (Apple Developer Program $99/yr — trivial money; the blocker is a decision, not cost).
*Done:* Release `.dmg` is Developer-ID-signed, notarized, stapled; installs with no Gatekeeper warning.

**D2. Windows code-signing (EV or Azure Trusted Signing)**
*Why it matters:* Unsigned Windows installers trigger SmartScreen (and **Smart App Control hard-blocks with no bypass** — RustDesk's exact post-cert-revocation pain). OV certs no longer earn instant SmartScreen reputation since June 2023; EV or Azure Trusted Signing is required. For a remote-access tool a SmartScreen scare-off kills adoption.
*Severity:* **P0** · *Platforms:* Windows · *Fixability:* `$FUND` (EV cert / Azure Trusted Signing subscription).
*Done:* Windows installer is signed with a reputation-bearing identity; no SmartScreen warning on a clean download.

**D3. Activate the wired Ed25519 signed auto-update**
*Why it matters:* Update-integrity signing is fully wired (ADR-078) but **inert** (empty pubkey, `createUpdaterArtifacts:false`). A remote-access tool with no working update path can't push a security fix. This is separate from OS signing and costs nothing — just the runbook steps.
*Severity:* **P0** · *Platforms:* all · *Fixability:* `CODE-NOW` (+ secret provisioning; no external funding).
*Done:* Generate key → provision secrets → flip the flag; a real signed update is downloaded, signature-verified, and installed via the two-click flow on all three platforms. (Note the competitive edge: our endpoint is configurable — RustDesk's is hardcoded to GitHub, a documented embedder pain point.)

### P1

**D4. Real download/marketing site + platform-parity matrix + permission-setup docs** — [TS]. Buyers must see exactly what works where (attended/unattended × OS × feature). Honesty beats silent gaps, and it's *our* differentiator to publish one. `CODE-NOW`. *Done:* platform-detecting download page, published parity matrix, per-OS permission docs, security/VDP page.
**D5. SBOM (CycloneDX) + THIRD-PARTY-NOTICES per release** — [TS] supply chain; cargo-deny gate already exists, extend to SBOM + license notices. `CODE-NOW`.
**D6. Versioning, changelog, EOL/support policy, rollback** — [TS]. `CODE-NOW`.

### P2

**D7. Enterprise deployment (MSI/GPO/Intune/Jamf, silent-install flags) + package-manager presence (winget/Homebrew)** — [DIFF]. `CODE-NOW`/`WIN-HW`.

---

## Security / Privacy

*This is our strongest area — most items here are "verify/wire the real thing," not "build from scratch." Don't let the strong core mask that most of it is loopback-tested only.*

### P0

**S1. Emergency stop verified ≤250 ms on real hosts, not loopback**
*Why it matters:* Inv 4 is the load-bearing safety promise. `emergency_stop` is loopback-tested (≤250 ms local) but never verified with a real media pump mid-encode on a real host over a real link (where "halt before next send" and "flush Bye" contend with actual encode/transport latency). The one invariant you cannot afford to be wrong about.
*Severity:* **P0** · *Platforms:* macOS, Linux (Win deferred to W1) · *Fixability:* macOS `CODE-NOW`, Linux `LINUX-DEV`.
*Done:* On a real host under active control, Stop revokes + releases all input + halts frames within 250 ms measured, idempotent, non-downgradable — recorded.

**S2. Wire the security spines into the actual UX (consent/lease/clipboard/file/audit are loopback-only)**
*Why it matters:* Clipboard, file transfer, chat, cursor, audit, control-lease consent all have "landed" *spines* with loopback tests but are not wired into the running app UX / not enabled by grant. An audit journal that never records a real session, a consent prompt that's never been clicked in the GUI, a file-accept dialog that doesn't exist yet — these aren't "done." Inv 10 (audit) especially: it must record *real* sessions to be worth anything.
*Severity:* **P0** (for audit + consent) / P1 (clipboard/file/chat UX) · *Platforms:* all · *Fixability:* `CODE-NOW` (+ on-device confirm).
*Done:* A real end-to-end session produces a verifiable signed audit chain on disk; control-lease consent + file-accept + clipboard indicators appear and function in the GUI; each gated feature enabled only when its capability is granted.

### P1

**S3. Unattended-access + paired-registry: durable store + flow wiring + revocation UI**
*Why it matters:* Unattended access is the single feature AnyDesk/TeamViewer/RustDesk productize that we have only as a *decision model* (ADR-084/085, in-memory). It's the bread-and-butter IT-support workflow. Needs SQLite durable store, connect/consent-flow wiring, host-displayed QR pairing, grant/revoke UI, and (Inv 16) TPM/Secure-Enclave-tier gating so software-only stays Tier 0.
*Severity:* **P1** · *Platforms:* all · *Fixability:* `CODE-NOW` (Secure-Enclave attestation verify needs on-device).
*Done:* Pair a controller via QR, reconnect unattended (fresh grant + per-message enforcement each time), revoke as kill-switch; software-only install refused unattended above Tier 0.

**S4. Published threat model, VDP, security contact, third-party pen-test before GA**
*Why it matters:* [TS] for a security product; regulated buyers require it. Parsec has SOC 2 + pen-tests; we claim a stronger architecture but have no external validation. Our whole wedge is "trust our security" — unaudited, that's just a claim.
*Severity:* **P1** · *Platforms:* n/a · *Fixability:* `$FUND` (pen-test) + `CODE-NOW` (docs/VDP).
*Done:* Public threat model + vuln-disclosure policy + security contact live; one external pen-test completed with findings resolved before GA.

### P2

**S5. Fuzzing on every wire decoder in CI + long-haul soak tests** — [DIFF]/[TS]. Decoders are fuzzed ad-hoc; make it a permanent CI gate + multi-day soak (leak/stuck-modifier/orphaned-capture detection). `CODE-NOW`.
**S6. 2FA on connection + IP allow-listing** — [DIFF], matches AnyDesk/TeamViewer/Parsec. `CODE-NOW`.

---

## The honest one-paragraph summary

**We can reach a credibly shippable macOS + Linux product without any Windows hardware and with only ~$100–$hundreds of funding (Apple Dev + a Windows signing identity).** The critical path is: **X1 (real reconnection)** + **X2 (no-black-screen)** + **X3/X4/M1/L1 (on-device input + Share verification on the two platforms we own)** + **S1/S2 (verify emergency stop + wire the security spines into real sessions)** + **D1/D3 (macOS notarize + activate signed updates)**. That yields an honest, verified, notarized macOS host and a real Linux host. **L2 (Wayland host via libei) is the one item that converts our security-only differentiation into a *capability* differentiator no incumbent matches.** Everything Windows (**W1–W4**) is gated behind acquiring a Windows runtime — until then, Windows Share must be labeled "unverified/experimental" in the parity matrix, never "supported." The security core is our moat, but most of it is loopback-tested vaporware until S1/S2 prove it on a real session; don't ship on the strength of `cargo test` green.
