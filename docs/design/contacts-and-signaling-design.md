# Design Plan — Contacts, Direct Messaging, Presence & Ticketless Connect

> Status: **DESIGN / PROPOSED — not yet approved for implementation.** Per `CLAUDE.md §9`
> (design before code), this document is the gate. Nothing here is built until the decisions in §7
> are made and an execution go-ahead is given. ADRs proposed in §8 are **Proposed**, not Accepted.

## 0. What the user asked for

> "can we plan for contact creation and sending messages … basically create a contact, instead of
> copying and forwarding a link every time … and message him, or request him for remote access."
> "and also let's create a plan for iroh-gossip as well."

Two asks, one system. Today every connection needs a **fresh ticket copy-pasted out of band** every
time. The goal: **save a peer once as a contact, then reach them by name** — view/share, **message**
them, or **request remote access** — with no ticket re-exchange. Presence ("is my contact online?")
and live request/message delivery ride **iroh-gossip**. This removes the single biggest friction in
the product without weakening any invariant.

## 1. Foundation that already exists (this is not greenfield)

- **Identity = the durable anchor (already the stated intent).** `docs/16` §: *"from then on that
  identity — not the ticket — is the durable trust anchor."* Exactly this feature.
- **`ras-identity::PairingRegistry` + `PairedController` + `pairing_code`** (ADR-084): a pure model
  for saving a peer identity (Ed25519 pubkey) with a label, timestamps, and a revoke kill-switch, plus
  a Crockford-base32 human verification code. In-memory MVP; **SQLite durable store is the named
  follow-up.** This is the contacts spine.
- **`EndpointId` = the peer's Ed25519 public key** (iroh 1.x rename of `NodeId`); `EndpointAddr` /
  `to_ticket`/`from_ticket` is the ticket; iroh authenticates the peer by `EndpointId` regardless of
  how it was reached (direct / relay / **discovery-by-id**).
- **Signed `AccessRequest`** (`ras-grant`) and **`ChatMessage`** (`ras-protocol`, already a `Redacted`
  payload, Inv 8) — the message/request bodies a signaling layer reuses.
- **Two-phase connect** (bootstrap ALPN `casual-ras/bootstrap/1` → session ALPN `casual-ras/1`),
  **local Allow/Deny consent** (Inv 1), signed **grants/leases**, the **per-message capability gate**
  (Inv 15) — all unchanged and all still apply. Contacts change *how you find a peer*, never *how you
  authorize one*.
- **Unattended-access decision model** (ADR-085, `docs/20 §3.4`) — a paired identity can (Tier-gated)
  pre-authorize; contacts is the identity layer that unlocks it.

## 2. iroh-gossip facts that shape the design (researched against `iroh 1.0.2`)

| Fact | Consequence for the design |
|---|---|
| **`iroh-gossip 0.101.0` depends on `iroh ^1`** → compatible with our pinned `1.0.2`. | **Buildable now.** (A future iroh `2.0` would need a new gossip release; `1.x` is fine.) |
| Best-effort, **unordered, no reliability, NO persistence.** | Gossip is a **live presence/signaling** channel, never a data or guaranteed-delivery channel. |
| **Offline peer gets nothing** (no store-and-forward). | "Message an offline contact" is a *separate* feature that needs a mailbox = an always-on node (backend). Out of the P2P MVP (§3.7). |
| To **join a topic you must supply known bootstrap `EndpointId`s**; no global directory. | The first identity exchange (ticket/QR) is still needed **once** per contact; after that the saved id bootstraps. |
| **Message payload is NOT author-authenticated** — `delivered_from` is the *forwarding neighbor*, not the origin (multi-hop Plumtree). | **Every signaling payload MUST be app-signed with the sender's identity key and verified.** Never trust `delivered_from` as the author. Load-bearing. |
| **`TopicId` (32 bytes) is a bearer capability** — anyone who knows it can join/read/inject. No ACL. | Private topics must use a **high-entropy secret topic id** derived at pairing, **plus** payload signing, **plus** app-layer sender authorization. |
| **Max message ≈ 4 KiB** (`DEFAULT_MAX_MESSAGE_SIZE = 4096`). | Signaling only. Anything larger → gossip the pointer, open a dedicated iroh stream. |
| **No built-in presence**; build from signed heartbeats + `NeighborUp`/`NeighborDown`. | Presence = periodic **signed** "online" beacons + neighbor events + freshness. |
| Open topics are exposed to **Sybil/eclipse**; the crate has no Sybil resistance. | Prefer **pairwise / small known-membership** topics bootstrapped by the contact's id; do not join open discovery topics. |
| **BONUS: iroh 1.0.2 can dial a saved peer by `EndpointId` alone** (no ticket) via DNS/pkarr discovery, *if* the peer is online + discoverable + discovery enabled (`presets::N0`, which we already use). | **Ticketless connect to a saved contact is supported today** — this is the core enabler, and it needs *no gossip at all* for the online case. |

## 3. The model

### 3.1 Contact = a saved, mutually-verified identity
Generalize `PairedController` → a **`Contact`**: `{ endpoint_id (Ed25519 pubkey), label, added_at,
last_seen_at, verification: Unverified|CodeVerified, blocked: bool, last_known_addr_hints (relay +
recent direct addrs, best-effort) }`. Contacts are **mutual** — pairing exchanges both identities, so
each side stores the other. **Durable store = SQLite** (the ADR-084 follow-up), so contacts survive
restart. Key change detection is free (the id *is* the pubkey; a rotated key is a new, unverified
entry — surfaced, never silently trusted).

### 3.2 First contact (pairing) — the one unavoidable out-of-band step
The **first** time two people connect they still exchange identities out of band — a **QR code** (host
displays; the pairing-code Crockford string is the verbal check) or the existing ticket. Both sides
**consent to add** (Inv 1). After that, the identity is the anchor and the ticket is never needed
again. This is unavoidable: gossip cannot find strangers, and trusting an unverified pubkey blindly
would break the security model.

### 3.3 Ticketless reach of a saved contact (the headline win)
Dial the saved `EndpointId` directly via iroh discovery (`endpoint.connect(id, ALPN)`), no ticket.
Fallbacks, in order: (1) discovery-by-id; (2) stored `last_known_addr_hints`; (3) if both fail →
"**contact appears offline**". **This alone removes the copy-paste for any online contact and needs no
gossip** — it is Phase A.

### 3.4 Presence — "who's online" (gossip)
Per **contact-pair**, derive a **secret `TopicId`** from a shared secret established at pairing (so it
is unguessable and private to the pair — not a hash of the two public keys, which would be guessable).
Each side periodically **broadcasts a signed "online" beacon** and tracks `NeighborUp`/`NeighborDown`
+ beacon freshness. A contact shows **online** when a fresh, signature-verified beacon is seen. Beacons
are tiny (≤4 KiB), signed, and reveal nothing but "this identity is online now" (Inv 8). *(Topic model
is a decision — see §7; pairwise is the privacy-safe default.)*

### 3.5 Messaging + "request remote access" from a contact
- **Direct message a contact:** when presence says online, open a direct iroh connection on a new
  **signaling ALPN `casual-ras/signal/1`** and send a **signed** message (reuse the `Redacted`
  `ChatMessage` body). Direct dial (reliable, ordered, no size cap) is preferred over gossip for the
  actual message; gossip is for presence + a wake-up nudge.
- **Request remote access:** send a **signed access-request *intent*** over the signaling ALPN → the
  contact's app raises an **incoming-request prompt** (Inv 1 consent, with the focus+notification we
  just shipped) → on accept, the normal Share/Connect two-phase flow runs. This literally replaces
  "text me your ticket." **Consent is unchanged** — being a contact only removes the ticket step, it
  **never** grants access; the human still clicks Allow, the grant is still fresh/short-lived/
  endpoint-bound, the per-message gate still runs.

### 3.6 Security model (every invariant preserved — this is the load-bearing section)
- **Inv 1 — contact ≠ authorization.** A saved contact skips only the *ticket exchange*. Every screen
  view / control / file / message that touches the OS still requires the local user's Allow. No contact,
  paired or not, ever self-authorizes.
- **Inv 3 — grants unchanged.** A contact-initiated connection is still `EndpointId`-authenticated by
  iroh's QUIC/TLS; the grant is still signed, short-lived, and endpoint-sender-constrained.
- **Inv 8 — no secret logging.** Presence beacons, message bodies (already `Redacted`), and topic
  secrets never hit logs/traces.
- **Payload signing (new, mandatory).** Every gossip beacon and every signaling message is signed by
  the sender's identity key and **verified before use**; `delivered_from` is never trusted as author.
  Unsigned / bad-signature / unknown-sender payloads are dropped.
- **Topic secrecy.** Pairwise topic ids are derived from a pairing secret (high-entropy), treated as
  secrets, never logged, never guessable from public keys.
- **Deny-by-default against spam/abuse.** Only **saved, non-blocked contacts** can deliver a message
  or an access-request by default. Requests from unknown identities are **refused** unless the user
  explicitly opts into a rate-limited "requests from strangers" inbox (default OFF). Per-contact
  **block/mute**. This is the anti-harassment / anti-scam posture (docs/15).
- **Eclipse/Sybil.** Use pairwise / known-membership topics bootstrapped by the contact's id; never
  join open discovery topics. Accept the residual risk honestly (the crate offers no Sybil resistance).
- **No new unauthenticated endpoint (DoD/Inv 9).** The signaling ALPN is iroh-authenticated +
  app-signed + consent-gated; it exposes no capability on its own.

### 3.7 Offline delivery — the honest limitation
iroh-gossip has **no persistence**, so it cannot deliver to an offline contact, and iroh **relays do
not store messages** (they only relay live traffic for NAT traversal). A true offline mailbox needs an
**always-on store-and-forward node** — a server-shaped component that conflicts with the strict
"no backend until Phase 9" stance (S9). **MVP is live-only:** a message/request to an offline contact
is queued **locally** and delivered on their next presence (best-effort), shown honestly as "will
deliver when they're online." A durable mailbox is a **separate, backend-requiring** feature (Phase 9
or a self-host option) — explicitly out of this plan's MVP.

## 4. Architecture & crates
- **`ras-identity`** — promote `PairingRegistry` → a **`Contacts`** book; add a **SQLite** durable
  impl (the ADR-084 follow-up). Pure model + storage seam; no iroh.
- **`ras-signal`** (new crate, or fold into `ras-bootstrap`) — iroh-gossip presence, the signaling
  ALPN, and the **signed signaling envelope** types (beacon / message / access-request-intent). Owns
  the `iroh-gossip = "0.101"` dependency (kept behind this crate so the Inv-18 license gate is scoped;
  gossip is n0 → expected MIT/Apache, **must be `cargo-deny`-verified at implementation**).
- **`ras-protocol`** — the small (≤4 KiB) signed signaling message enum (additive; fuzz + fail-closed
  codec like every other wire type).
- **`app`** — a **Contacts** view: list with online/offline dots, add-contact (QR / pairing code /
  ticket), one-tap **Connect / Share to / Message / Request access**, and **block/mute**; incoming
  request/message prompts reuse the consent-card + focus/notification UX just shipped.

## 5. Proposed phasing (each phase is independently shippable)
- **Phase A — Durable contacts + ticketless connect (biggest UX win, lowest risk, NO gossip).**
  SQLite contacts book + the app Contacts UI + **dial-a-saved-contact by `EndpointId`** via existing
  discovery. Removes the copy-paste for every online contact using only iroh we already have.
- **Phase B — Presence (gossip).** Pairwise secret topics + signed beacons → online/offline dots.
- **Phase C — Messaging + request-access (signaling ALPN).** Signed, consent-gated, deny-by-default.
- **Phase D — (Deferred) Offline mailbox.** Backend-requiring; Phase 9 / self-host only.

## 6. What does NOT change (guardrails)
Consent (Inv 1), grant/lease shape + expiry + endpoint-binding (Inv 3), per-message capability gate
(Inv 15), emergency stop (Inv 4), secret hygiene (Inv 8), the license gate (Inv 18), and the wire
protocol source-of-truth in `proto/`. Contacts is an **identity-and-discovery convenience layer on top
of the unchanged authorization core.**

## 7. Decisions needed before implementation
1. **Offline delivery:** accept the **live-only MVP** (recommended — stays true to no-backend) vs
   invest now in an always-on mailbox node (a backend, larger scope, availability/trust burden)?
2. **Presence topic model:** **pairwise secret topics** (recommended — private, simple auth, no shared
   roster leak) vs one per-user contact-group topic (fewer topics but needs payload encryption + leaks
   the membership set to all joiners)?
3. **Requests from strangers (non-contacts):** **contacts-only, refuse strangers** (recommended — best
   anti-abuse) vs an opt-in, rate-limited "requests" inbox (more discoverable, more abuse surface)?
4. **Priority & scope now:** ship **Phase A only** first (ticketless connect — the headline win, no
   gossip, low risk) and stage B/C, vs design/build A–C together?

## 8. Proposed ADRs (Proposed — pending §7 + go-ahead)
- **ADR-092** — Durable mutual **Contacts** book (extends `PairingRegistry`; SQLite; mutual; key-change
  surfaced, never auto-trusted).
- **ADR-093** — **Ticketless contact connect** via dial-by-`EndpointId` + iroh discovery, with
  addr-hint + offline fallbacks.
- **ADR-094** — **Presence + signaling over iroh-gossip 0.101** (pairwise secret topics, mandatory
  payload signing, deny-by-default, live-only; `delivered_from` never trusted as author).
- **ADR-095** — **Contact messaging + request-access** over a signaling ALPN, consent-gated, Inv 1/3/15
  preserved.
- **(Deferred)** offline mailbox — explicitly Phase 9 / self-host; not an MVP ADR.
