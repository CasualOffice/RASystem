# 25 — UI Design Direction (contacts-first collaboration + calls + remote access)

> **Status: design direction, not yet built.** Produced from a four-track grounded design-research
> sweep (2026-07-25) that studied how the best real apps actually structure contacts, chat, presence,
> calls, notifications, and remote-access sessions — across desktop *and* mobile. Every decision below
> names its influence(s); nothing here is a generic template. This is the reference the app shell +
> messaging/calls/session UI hangs off. **Design before code** (CLAUDE §9): this doc gets sign-off,
> the security-touching parts get ADRs, *then* we build.

The explicit brief: **premium, Teams/Slack-grade UX; NOT a generic AI-generated layout.** So this doc
carries an anti-AI-generic guardrail section (§10) and every surface is specified concretely (widths,
row heights, token values, named influences) rather than "a clean sidebar."

---

## 1. The reframe

Casual RAS is shifting from a *two-button remote-access utility* to a **contacts-first collaboration
app that also does secure remote access and 1:1 calls.** The contact is the home object; chat, calls,
and remote-access sessions all launch *from a contact*. That reframing means the whole app becomes a
**persistent shell** (a left rail + list + main pane), not a stack of dialogs.

Where we sit in the field (research finding): we are closer to **Discord/Slack** (relationships +
presence are first-class) than to Signal (presence hidden) — but we are **not a community server**, so
we don't need Discord's 4-column density. And on remote control we sit with the **dedicated
remote-access incumbents** (Tuple/AnyDesk/Chrome RD/TeamViewer/Parsec), not the conferencing tools
(Discord/Slack have *no* OS control). So our influences are deliberately split:

- **Shell / chat / presence** → Telegram's measured density + Discord's presence expressiveness +
  Slack's rail semantics, held to **Linear's calm-dense discipline**.
- **Calls** → FaceTime's glass restraint + Telegram's reactive motion + Meet's user-controlled PiP +
  Zoom's explicit states + CallKit-grade ring reliability.
- **Remote-access session + consent** → AnyDesk's editable capability checklist + Chrome RD's
  unremovable Stop bar + Tuple's ambient control legibility + Teams' top-of-screen request bar.

---

## 2. Design principles (the through-line)

The premium apps share these traits; we adopt them as rules.

1. **One accent, used for meaning — never flood-filled.** (Linear `#5E6AD2`, Slack green, Discord
   Blurple.) Our accent marks the single primary action, focus rings, active state, and your-own
   reaction. Everything else is neutral.
2. **Depth via value, not scattered shadows.** Discord's rail→list→content darkness gradient layers
   the UI with background values; soft shadow is reserved for true overlays (popovers, modals, the
   incoming-call window, consent dialogs). In dark mode, *higher = lighter surface* (Linear/Material 3).
3. **Hairline structure over boxes.** 1px low-contrast borders (Vercel's `box-shadow: 0 0 0 1px`
   trick) separate regions; we do not wrap everything in a shadowed card.
4. **Metadata appears only when needed.** Timestamps, message actions, who-reacted — hidden until
   hover (Discord/Slack). "Premium = information appears on demand."
5. **A strict grid.** 4px spacing rhythm; a measured type scale; Telegram's exact list grid. The
   premium feeling is *consistency of rhythm*, not ornament.
6. **Restraint — spend boldness in exactly one place.** Signal ships zero presence chrome; we invest
   our motion budget only where there's no live content to respect (voice-call background, §7).
7. **Fast, purposeful motion.** 120–200ms, one shared curve; animate state changes, not decoration.
   **Never animate away the Stop control or a session indicator** (Inv 4/7).

**Invariant constraints that shape the UI** (these are non-negotiable — CLAUDE §5):
- The always-visible **session indicator** and **Stop** are structurally un-hideable and unspoofable
  (Inv 7). Chrome RD's un-dismissable Stop bar is our model, backed by our secure-window capture
  exclusion so the indicator can't be spoofed/re-captured.
- **Capability scope is shown and revocable** (Inv 2/15) — AnyDesk's editable per-capability checklist.
- A control **request is a decision, never a chat message** — it gets a dedicated high-salience surface.
- **Security → Latency → UX** ordering: a stalled feed must never freeze the pointer or Stop; chat
  never overlaps the video or the Stop control. Don't invert this without an ADR.
- **No secrets in the UI's logs** (Inv 8): chat/clipboard/typed text/filenames render via `textContent`,
  never `innerHTML`, and never `console.log`.

---

## 3. Design tokens (dark-first — fits a security product)

Namespace everything `--ras-*`, two-tier (global brand tokens → local component tokens, per Stream's
SDK model) so the eventual embeddable build re-themes via stylesheet. Concrete starting set:

**Color** — use **Radix Colors** as the engine (free, correct, inherits the 12-step role map where each
step has a fixed job: 1 app-bg … 6/7 borders … 9 solid brand … 11/12 text):
- **Neutral: Radix `slate`** — a deliberately blue-tinted gray (Linear's effect, systematized). Base
  surface ~`#0A0B0F`; a 4-step surface ladder built from *lighter overlays, not shadows*. Text at
  slate-12; hairlines at slate-6/7. (Do **not** use Tailwind `gray-100` eyeballed neutrals — that's an
  AI tell.)
- **One accent: desaturated indigo** (Linear `#5E6AD2` / Radix `indigo` family). Reads secure + calm;
  it is *not* the AI `blue-500`/`indigo-500` default precisely because it's desaturated and paired with
  tinted-slate neutrals. Used only for the primary action + focus + active state.
- **Semantic scales kept structurally separate from the accent** (mandatory — the invariants demand
  unmistakable status): Radix **red** (danger / emergency-stop / hang-up), **amber** (warning /
  consent-pending), **green** (success / connected / online).
- **A reserved status color for the session indicators** ("REMOTE CONTROL ACTIVE" / "AUDIO SHARED" /
  "SCREEN SHARED") that is **never** the brand accent — so an active-session warning can never be
  confused with a normal UI highlight (Inv 7). Propose a distinct **hot amber-red** reserved solely
  for live-session chrome.

**Type** — Geist Sans (or Inter Variable) + Geist Mono / Berkeley Mono. Weights **400 / 500 / 600
only** (Linear/Vercel discipline — no 700; hierarchy from size + weight, like Slack's two-weight Lato).
Body **15px / 1.5** (apps land 13–15px). Scale **12 · 14 · 15 · 20 · 24 · 32 · 48**. Negative
letter-spacing only ≥24px (−0.02em, tightening with size). **No** Instrument-Serif-on-cream, **no** lone
italic-serif accent word (AI tells).

**Spacing** — 4px base: **4 · 8 · 12 · 16 · 24 · 32 · 48 · 64 · 96**.

**Radius** — one consistent value applied with intent: **8px** for cards/inputs/popovers, **50%** for
avatars, **full** for pills/badges. Not `rounded-2xl` on everything.

**Elevation** — hairline borders at low elevation; a single soft layered shadow only for popovers /
modals / the incoming-call window / consent dialogs. Dark-mode depth = lighter surface.

**Motion** — durations **120 / 160 / 200ms**; one shared easing **`cubic-bezier(0.2, 0, 0, 1)`**
(Material "Emphasized"). Animate state, not decoration; no fade-up-on-scroll.

---

## 4. The shell

**Desktop — three zones + a transient fourth** (a hybrid of Telegram's lean two-pane and Discord's rail,
never Discord's full 4-column density):

```
┌────┬──────────────┬───────────────────────────┬─────────────┐
│ 72 │  ~300         │  main pane                │  transient  │
│ px │  contacts /   │  (conversation / call /   │  right pane │
│rail│  conv list    │   remote-screen session)  │  (slides in)│
└────┴──────────────┴───────────────────────────┴─────────────┘
```

1. **72px identity/nav rail** — Discord/Telegram's exact `72px` (both source-verified). Top: home/
   contacts. Middle: primary sections (Contacts · Recent · Activity/Calls). Bottom: the user's own
   avatar + settings (WhatsApp/Discord pattern). **Persistent, non-collapsible** (Discord tested and
   did *not* ship auto-collapse; don't chase it — but honor Slack's manual Cmd/Ctrl+Shift+D collapse of
   the *list*).
2. **~300px contacts/conversation-list pane** — inside Telegram's proven **260–540px resizable** range,
   default ~300. Row spec in §5.
3. **Flexible main pane** — the conversation by default; becomes the **call view** or the
   **remote-screen session** in place (§7, §8).
4. **Transient right pane** — Telegram's 292–392px slide-in info/shared-media model, **repurposed for
   us** as the **remote-session panel** (screen status, live capability toggles, connection telemetry,
   Stop) and/or contact details. Transient, not a persistent Discord member-list — fits a 1:1/small-
   group tool.

**Responsive collapse order** (borrowed *order* is well-sourced; the px values are our recommendation):
- **≥1280px** — all four zones.
- **1024–1280px** — the **right pane sheds first** (it overlays on demand). Rail + list + main persist.
- **768–1024px** — master-detail: rail + list + main; right pane always an overlay.
- **<768px** — flip to **push-navigation + a bottom tab bar**; list and main become routes (tap contact
  → session/conversation pushes full-screen, back returns). Treat mobile as a **distinct IA**, not a
  squeezed desktop (Discord/Slack/Telegram all do).

**Mobile bottom tab bar — 4 labeled tabs** (labels mandatory for a11y, per Slack): **Contacts · Calls ·
Chats · You.** A contacts+calling product leads with Contacts and Calls (Telegram/WhatsApp lineage),
not Slack's channel-first Home. **Remote sessions are launched from a contact**, not a tab — keeps the
count at 4.

---

## 5. Contacts / conversation list + presence

**The row** — built on **Telegram Desktop's source-exact grid** (the single most copy-worthy structure,
because it's open-source and battle-tested for exactly a contacts+conversations list):
- **62px row height**, **40–46px round avatar** at ~10px inset, **name** at ~68px/top line,
  **preview/subtitle** at ~68px/second line, **right-aligned timestamp**, a **19px accent unread pill**
  (grey when muted), a **muted-bell** + optional **pin**.

**Presence — our differentiator, so lean expressive, but serve Inv 7.** Use **Discord's color + shape
dot in a punched-out avatar cutout, bottom-right** (legible without color = accessibility; the
shape-not-just-color lesson is the most important one on this surface):
- **Online** = filled green circle · **Away** = hollow ring (Slack's fill-state idea) · **DND** = minus
  bar · **Offline** = grey ring.
- **Product-specific state — "In session / screen-shared":** swap the dot for a distinct **monitor
  glyph** on the contact's avatar (Slack swaps to headphones in a huddle; we swap to a screen icon), so
  **an active remote session is always visible on the contact** — directly serving "active remote
  control is always visible" (Inv 7).
- We do **not** copy Signal's zero-presence stance (we're presence-first) but **do** borrow its
  restraint elsewhere. Presence remains privacy-respecting (contacts-only, already enforced server-side).

---

## 6. Message surface

**Flat, left-aligned rows (Discord/Slack), NOT SMS bubbles.** Rationale: reads as a *tool*, better for
collaboration scroll-back, and distinguishes us from consumer messengers.
- **40px round avatar in a ~72px left gutter**; name + timestamp header on line one; body indented.
- **Grouping:** consecutive same-sender messages within ~8 min collapse — only the first shows
  avatar/name; the rest reveal the **timestamp on hover** in the gutter (universal pattern).
- **Reactions:** pill chips under the message (emoji + count, your own highlighted, who-reacted on
  hover). *(New wire type — small ADR, §12.)*
- **Inline media:** image/file/link previews as inline thumbnails + a download affordance; embeds get a
  **left color-bar** card (Discord). File transfer reuses our existing signed-catalogue flow.
- **Composer:** Enter-sends, Shift+Enter newline, vertical-grow (min ~38px, Discord); `+` attach left;
  emoji inline-right; a **mic-morphs-into-send** micro-interaction; a **reply/edit context chip** above
  the input ("Replying to @name ✕"). Keep formatting behind Markdown (Discord/Telegram), not a heavy
  toolbar — less surface, cleaner, and a *messaging* rather than *enterprise* signal.

---

## 7. Calling UX

Direction: **"quiet glass over live video."** FaceTime-grade restraint as the base; Telegram-grade
reactive motion only where there's no video to respect; Meet-grade user-controlled PiP; Zoom-grade
explicit states; CallKit-grade ring reliability.

**Incoming call:**
- *Mobile:* full-screen via **CallKit (iOS)** / **ConnectionService + full-screen-intent-with-heads-up-
  fallback (Android)** — non-negotiable for ring reliability (Discord's no-CallKit is the cautionary
  tale). Layout: large/blurred contact avatar (Signal's calm), name, "[Contact] — Video Call" label,
  **circular green-accept (right) / red-decline (left)**, slide-to-answer on lock; **Decline-with-
  message + Remind Me** above decline (FaceTime/WhatsApp). Offer a **Signal-style CallKit privacy
  opt-out** (no call metadata to iOS Recents/iCloud) — fits Inv 8.
- *Desktop (our Tauri app today):* a **compact corner call window** (Teams' 2026 model, *not* a
  full-screen takeover) with **split Accept-Audio / Accept-Video / Decline** — which maps exactly onto
  our voice-vs-video product split — plus a **native OS toast** (more reliable than in-app-only) and a
  looping local ring. Builds on our existing focus+notify-on-inbound behavior and the overlay/strip
  window pattern.

**In-call:**
- Control bar order: **mute → camera → speaker/flip → screen-share → more(…) → RED End (isolated far
  right, spaced)** — Meet's destructive-action spacing + Zoom's ordering. **Button *fill color* = state**
  (Discord 2025), red-with-slash = off (Meet's most-legible state pattern).
- **Auto-hiding glass controls** that recede over the video and reappear on tap, clustered near the last
  tap (FaceTime). Self-view = small rounded draggable corner-snapping tile, enlargeable (Signal).
- Timer replaces "Ringing" on connect. **Connection quality = Teams' 3-bar indicator with an
  in-self-view *actionable remedy*** ("turn off video to save bandwidth") — not a buried stats panel.
  Since we're P2P/QUIC, surface **named transport sub-states** ("relay fallback," "reconnecting") rather
  than a generic "failed" (Discord's ICE-state honesty). This reuses our `HealthObserver`.

**Voice-only:** **Telegram-style** — centered avatar over a **voice-reactive animated gradient that
breathes with loudness** (this is where we spend the premium-motion budget, since there's no video to
respect). High-contrast, recolorable speaking indicator (avoid Discord's green-ring a11y miss).

**PiP / minimized — our single most important call surface** (a call must survive navigating to chat
*or into a remote-screen session*): model on **Google Meet's auto-PiP with granular user control** — a
floating, always-on-top, draggable, resizable pill that appears **automatically when you leave the call
view**, retains **mute + camera + End** + a one-click **expand-back**, with a **user setting (Never /
on-navigate / Always)**. Mobile: FaceTime corner-snap + pinch-resize + drag-off-to-hide-video-keep-audio.
**Never make it un-dismissable** (Teams' anti-pattern) and **never absent** (Discord's failure).

**States** — adopt **Zoom Phone's explicit enum**: `Ringing → Connecting → Connected → Reconnecting →
Ended` + terminal `No-Answer / Busy / Declined / Failed`. Add a handshake state analogous to Telegram's
**"Exchanging encryption keys"** to honestly show our grant-validation/consent step (the controller is
being *authorized* — Inv 1/9). Drive the ringing/voice background gradient by state.

**Ringtone / notification:** looping, ~30s timeout (≤60s iOS CallKit cap), **generate ringback locally**
for the caller; **per-contact custom ringtones** (Signal supports; Discord's lack is a standing
complaint). **Distinct sounds per event class** — incoming call vs. incoming remote-control request vs.
chat — so a control request is never mistaken for a chat ping.

---

## 8. Remote-access session + the two access flows

### The two access flows

**Flow 1 — Ticket / link ("I share to someone," incl. non-contacts):** [Chrome RD one-time code +
Tuple ⌘L link] — keep our existing `CASUALRAS1:` ticket as the ephemeral, single-use, out-of-band path.
An incoming dial on a ticket **still surfaces the same consent card as Flow 2** (never auto-connect —
the ticket authenticates identity, not authority; Inv 9).

**Flow 2 — Contact requests access (the new relationship path, the common case):** [Tuple contacts-first
+ Parsec friend trust + AnyDesk accept window + Teams top-of-screen request bar] — from the contacts
roster, a contact taps **"Request to view / Request to control [Name]'s screen."** The owner gets a
**dedicated, focus-raising Access Request card** (NOT a chat bubble — a request is a decision):
- Requester **identity + our Crockford-base32 `pairing_code`** for the human key-check.
- **Requested capabilities as an itemized, per-capability checklist the owner can trim before
  accepting** — [AnyDesk's editable list]: View screen · Control keyboard/mouse · Clipboard · File push
  (per-target) · Audio listen. **Deny-by-default; unknown caps never shown as on** (Inv 2).
- **Allow / Deny**, with **Allow once** vs **Remember this contact (skip future prompt)** — [Parsec
  "connect without approval" + our `PairingRegistry`/`unattended_decision`]. The UI must state plainly
  that "Remember" governs only the *prompt* — every connect still mints a fresh, endpoint-bound,
  per-message-enforced grant (our ADR-084/085 structural rule).

This **inverts the incumbents** (who lead with codes); contacts-first is our differentiation.

### The active-session layout

- **Remote screen fills the main pane** (TeamViewer/AnyDesk/Parsec). Launched from a conversation, the
  session takes over the conversation pane with a one-tap **"back to chat"** that keeps the session live
  in a **docked pill** (Slack Huddle-in-channel + Discord floating preview).
- **Controls = a top-center collapsible toolbar** (the TeamViewer/AnyDesk de-facto standard): display
  switch, quality, **live capability toggles editable mid-session** (AnyDesk), file push, chat, and a
  prominent **Stop**. Auto-hide, hover-reveal (Teams).
- **Owner-side always-visible consent** = a **persistent full-window Screen Frame** (AnyDesk) **plus**
  an **always-present, non-removable Stop bar** (Chrome RD) — impossible to hide (Inv 7), backed by our
  secure-window exclusion so it can't be captured/spoofed. Distinguish **"REMOTE VIEWING ACTIVE"**
  (border only) from **"REMOTE CONTROL ACTIVE"** (border + a colored who's-in-control indicator, per
  Tuple's avatar ring). Emergency stop always reachable (Inv 4; our global hotkey covers focus-stuck).
- **Chat coexists as a collapsible side rail decoupled from the screen** (TeamViewer/AnyDesk) — never
  overlapping the video or the Stop (honors Latency > UX). Reuses our `ChatMessage`.
- **Premium execution bar:** real OS cursor never scaled/tinted (Tuple); live latency/loss telemetry
  present but unobtrusive (Parsec); "App-Veil"-style sensitive-app hiding surfaced in the share picker
  (Tuple App Veil ↔ our secure-window flag); minimal edge-docked chrome; keyboard-first; one-click start.

### Mobile controller — a genuine differentiator
The conferencing tools **cannot** control from mobile; only the dedicated remote-access incumbents can.
Adopt **Touch mode + Trackpad/Mouse mode** with a compact bottom session toolbar + a pie/expandable
control (TeamViewer/AnyDesk mobile). The owner-side indicator and Stop must be equally unspoofable on
mobile. There is **no premium ambient-control precedent on mobile — we get to define it.**

---

## 9. Notifications & ringtone system

- **Foreground (app open):** a custom in-app **top banner** (Telegram's pull-to-expand) + a toast
  system with **hover-to-pause** (Discord's reverse progress bar), momentum swipe-to-dismiss, ~4s
  auto-dismiss for info / persistent for action-required, **cap 3–5 stacked** + queue. Chat notifications
  stay content-free where they cross a boundary (Inv 8 — the codebase already does gentle chat-notify
  with no message text).
- **Background/closed (native builds):** hand off to **OS-native** (inherit lock screen / Focus /
  recents). Incoming **call** → CallKit (iOS) / full-screen-intent (Android). Incoming **remote-access
  request** → a **time-sensitive communication notification with inline Accept/Deny**, *not* the phone-
  call takeover — it maps to our consent model and reserves CallKit for actual calls.
- **Web/embeddable build:** no CallKit — in-page ring UI as a core component; Notifications API only as
  a permission-gated enhancement, never a dependency. The ring/consent UI and the always-visible
  indicator/Stop must be **in-DOM, in-container** elements that survive a hostile host stacking context
  (the browser owns the "you are sharing" indicator anyway, which *aligns* with Inv 7).

---

## 10. Anti-AI-generic guardrails (explicit)

The brief's core requirement. **Avoid these documented tells:**
cream/beige + serif + terracotta · near-black + a lone acid-green/vermilion glow · purple→blue gradient
hero · Inter/Space-Grotesk/Geist as the unmodified default · emoji as section markers/bullets ·
everything center-aligned · uniform `rounded-lg`/`rounded-2xl` on everything · **accent-colored left
border on a rounded card** ("the single most recognizable tell") · fade-and-slide-up on every scroll ·
Tailwind/shadcn defaults untouched (`shadow-md`, `gray-100`, `blue-500`) · symmetric 3-card feature row
with lucide-icons-in-circles · numbered 01/02/03 badges · all-caps eyebrows · gradient text ·
glassmorphism/glow everywhere · oversized avatars/timestamps + "big empty" low-density layouts.

**Positively — what reads as intentionally designed:** typography *is* the brand (one committed family
+ system); a near-monochrome tinted-slate palette with **one** confident accent used for *meaning*;
**borders/hairlines over drop-shadows** at low elevation; one distinctive consistent radius; asymmetric/
left-aligned editorial layout; **density in behavior** — six crafted microstates per control (default /
hover / focus / active / disabled / loading); physical purposeful motion; real content + honest copy.
The one-line test: **if an element doesn't encode something true about *this* product, it's decoration —
and reflexive decoration is what reads as AI.**

---

## 11. What we do / what we explicitly don't

**We do:** the 72px rail + ~300px Telegram-grid list + flat Discord/Slack message pane + transient
right-pane-as-session-panel; Discord-style color+shape presence with a product "in-session" state; the
FaceTime/Telegram/Meet call direction; AnyDesk-editable-checklist + Chrome-RD-unremovable-Stop consent;
contacts-first access requests; first-class mobile controller.

**We don't:** Discord's 4-column density; a persistent member-list; SMS-style chat bubbles; Signal's
zero-presence; a heavy formatting toolbar; a full-screen phone-call takeover for a *remote-access*
request; an un-dismissable or absent call PiP; a control request rendered as a chat message; any
hideable/removable session indicator; inverting Security → Latency → UX.

---

## 12. Decisions that need ADRs before their code

- **Calling expands the media posture to two-way + mic + camera.** Today: audio is output-only, live-
  only, never recorded, no mic (Inv 12). A 1:1 voice/video call inherently needs **mic capture** (new)
  and **camera capture** (new — we capture *screen*, not camera). Done right this is consent-gated,
  always-disclosed with an active indicator, and still never recorded — but it's a deliberate,
  ADR-worthy change to Inv 12. **ADR required.**
- **Call signaling** — a new `SignalPayload`/`ControlMsg` set (INVITE / RING / ACCEPT / REJECT / BUSY /
  HANGUP) + an in-call state machine. New wire surface. **ADR required.**
- **Contact-request access flow** — one new signal variant + the consent-card UX; the authorization
  core is unchanged (small security surface). **Short ADR.**
- **Message reactions** — a small reaction wire type + storage. **Note in the protocol doc; likely no
  full ADR** (touches the wire but adds no capability/authorization).

---

## 13. Provenance & confidence

Sourced from live research (2026-07) across Slack/Discord/Telegram/Teams/WhatsApp/Signal (shell, chat,
presence), Apple CallKit/PushKit + Android Telecom + FaceTime/Zoom/Meet/Teams (calls),
Tuple/AnyDesk/TeamViewer/Chrome RD/Parsec (remote-access consent + session), and
Linear/Vercel-Geist/Apple-HIG/Material-3/Fluent-2/Radix (design system + anti-AI catalog).

**High-confidence, safe to build against:** Telegram Desktop px (open-source `.style` files) and
Discord constants (community-corroborated); the CallKit/Telecom framework constraints; the collapse
*order* and mobile-tab archetypes. **Recommendation, not spec:** the exact breakpoint px values (no app
publishes theirs), and Linear's exact weight/motion values (observed, not an official export).
**Version-dependent — confirm on the current build before finalizing:** Slack/Teams/WhatsApp/Signal
px/hex (unpublished), and mobile tab labels (shifted in 2025–2026 redesigns).

---

## Next steps (proposed)

1. **This doc → sign-off.** Adjust the direction where you disagree.
2. **A clickable visual mockup** (an Artifact) of the shell + one conversation + the consent card + an
   incoming-call window + an active-session layout — so you react to the *look* before any app code.
3. **The ADRs** (§12) for the security-touching parts, in parallel.
4. **Build in phases**, the shell first (everything renders inside it), then messaging/reactions/media,
   then the contact-request access flow, then calling (voice → video). Sequencing is the user's call.
