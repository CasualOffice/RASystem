# 24 — UI/UX Design System & Plan

> The working reference for making Casual RAS **premium and trustworthy-by-design** — to the standard of
> a senior Google (Docs/Material) designer, without inventing a look. It is grounded in a multi-agent UX
> audit of the real app and the approved design review (living visual version: the design Artifact). UX
> is now an explicit product priority — see `CLAUDE.md` §2 for the fixed ordering it lives inside.
>
> **Priority guardrail (non-negotiable).** `CLAUDE.md` §2 fixes **Security → Latency → UX**. "UX is a
> priority" means UX is *world-class in the ~95% where it does not conflict* with security/latency; it
> only yields in the rare genuine conflict. Never remove consent, the always-visible indicator (Inv 7),
> or the emergency Stop, and never cache authorization "to feel faster." Elevating UX craft is
> sanctioned; weakening the security spine is not.

---

## 1. Design principles

1. **Trust is the product.** Every screen answers, unasked: *who is connected, what can they do, how do I
   stop it.* Legibility of power beats decoration.
2. **Calm authority.** Premium here is certainty, not flash — generous space, one accent, quiet neutrals.
3. **One source of truth.** A tokenized system (color, type, space, elevation, motion) defined once in
   `app/ui/style.css :root` and referenced everywhere. Consistency is structure, not willpower.
4. **Motion with meaning.** Transitions explain change; never motion for delight alone. All decorative
   motion is guarded by `prefers-reduced-motion`.
5. **UX is first-class** — within the priority ordering above.

---

## 2. Tokens (the single source of truth) — shipped in `style.css :root`

**Color roles** (cool-biased near-black, *chosen* not defaulted):

| Token | Hex | Use |
|---|---|---|
| `--ground` | `#0A0C11` | app background |
| `--surface-1/2/3` | `#12151C` / `#191D27` / `#21262F` | layered surfaces / overlays |
| `--hairline` | `#262C38` | borders, dividers |
| `--ink` / `--ink-dim` / `--ink-faint` | `#E8EBF1` / `#98A2B3` / `#5E6777` | text hierarchy |

**The one accent + the security semantics (kept strictly separate):**

| Token | Hex | Meaning — **never overloaded** |
|---|---|---|
| `--signal` | `#2DD4BF` | **a live secure connection** (viewing/controlling/live/presence-online) |
| `--allow` | `#3FB950` | allowed / granted / safe / ready |
| `--pending` | `#E3B341` | awaiting consent / checking / countdown warning |
| `--stop` | `#F85149` | stop / deny / emergency / critical error |

> **The load-bearing rule: color never lies about state.** `--signal` teal = live connection; red is
> *only ever* Stop/deny; amber is *only ever* pending. (Pre-fix, view-only rendered red and control
> rendered amber — backwards. Fixed in `6818e26`.)

**Type** — editorial serif for authority (docs/marketing), system sans for the app UI, mono for technical
truth (tickets, IDs, metrics): `--font-sans`, `--font-mono`; scale `--fs-display/title/heading/body/label/data/micro`.
Use `font-variant-numeric: tabular-nums` for any aligned digits (HUD, countdowns).

**Space** — 4px scale `--space-1..7` = 4/8/12/16/24/32/48. **Radius** `--radius-xs..pill`. **Elevation**
`--elev-1..4`. **Motion** `--motion-fast/mid/slow` + `--ease/--ease-out`.

---

## 3. Component requirements

Every interactive component (shipped in `94666f2`) defines all four states off tokens:
- **hover** — subtle surface lift; **:focus-visible** — a visible `--signal` ring (keyboard a11y, global
  default + explicit rings on inputs); **:active** — a small press; **:disabled** — reduced opacity, no hover.
- Buttons: primary = `--allow`, secondary = surface/neutral, danger = `--stop` — read by weight + color.
- Overlays (dialogs, consent cards) sit on `--surface-2/3` with `--elev-*` so they read as lifted.
- State is encoded in **form as well as words** — a pill/badge/dot, not just text.

---

## 4. The trust-critical moments (most design attention)

- **Consent** — verifiable identity, plain-language stake, real default (silence denies). No dark patterns.
- **The live indicator** (Inv 7) — always on top of the shared display, always paired with Stop; teal.
- **Emergency Stop** — the most important control; unmissable, instant (≤250 ms), overrides everything.
- **Connection health HUD** — the diagnostics we already compute (latency/fps/drops/path/`q` decode-queue),
  surfaced as a quiet tabular readout.

---

## 5. UX P0 backlog (from the multi-agent audit) — live status

| # | P0 | Status |
|---|---|---|
| 1 | Bound the relay wait (~10s) + spinner + P2P/retry fallback (no hang) | ☑ `d57c3a0` |
| 2 | Semantic share-status state machine (typed states, not raw strings) + permission-denied recovery | ☑ `d57c3a0` |
| 3 | Permission orchestrator (macOS `CGPreflightScreenCaptureAccess`, honest error + Open Settings) | ☑ `d57c3a0` |
| 4 | Take-control **terminal outcome** (Granted/Denied/Timed-out; no silent revert) | ☑ `d57c3a0` |
| 5 | Style the reconnect/live banner (teal border + glow, legible on dark) | ☑ `94666f2` |
| 6 | File-transfer **sender** countdown mirror + typed rejections | ☑ `d57c3a0` |
| 7 | **Fix inverted color semantics** (view/control → teal; red = stop; amber = pending) | ☑ `6818e26` |
| 8 | Presence **three-state** dot (online / offline / checking) | ☑ `d57c3a0` |

Legend: ☑ shipped (off-device; on-device-verify-pending) · ◐ in progress · ☐ not started. **All P0s
shipped at the code level.** Full feature-by-feature table + the two annotated user journeys ("Share my
screen", "Connect + take control") live in the design Artifact.

---

## 6. Phased plan

- **Phase 0 — token layer** (color/type/space/elevation/motion; migrate inline hexes). **☑ shipped** (`6818e26`).
- **Phase 1 — components** (one of each, all states, off tokens). **☑ CSS shipped** (`94666f2`); JS behaviors (loading/empty/error states) land with the P0s.
- **Phase 2 — screens** recomposed on the system (home, session, contacts, messaging).
- **Phase 3 — trust moments** (consent, indicator, stop, health) with full attention.
- **Phase 4 — motion & polish + accessibility pass** (contrast, focus order, keyboard paths, reduced-motion).

---

## 7. Accessibility (baseline, not optional)

Visible `:focus-visible` on every focusable (global `--signal` ring, shipped); keyboard reachability for
every action; contrast held for `--ink`/`--ink-dim` on the dark ground; `prefers-reduced-motion` honored;
state conveyed by more than color alone (icon/shape/label alongside the semantic hue).
