# Phase S Design Note — Risk-validation spike (→ M1)

> Design gate for the throwaway spike (`docs/17` Phase S). Its job: convert the biggest unvalidated
> bets from "assumed" to "measured" **before** real architecture. Code lives in `spike/` (its own
> workspace, excluded from the main build) and is disposable.

## 1. What we are de-risking (and the target numbers)

| Bet | Risk | Target (`docs/01 §11`) | Spike that measures it |
|-----|------|------------------------|------------------------|
| Iroh direct/relay works on hostile networks | D7 | session setup > 95% on supported nets | `spike/iroh-probe` |
| Transport latency overhead | D1 | internet direct overhead < 80 ms beyond RTT | `spike/iroh-probe` (per-frame RTT) |
| WebCodecs decode+render is low-latency in WebView2 | D1/D4 | glass-to-glass < 120 ms LAN | `spike/latency-probe/web` (WebCodecs loopback) |
| Compositor-frame penalty is tolerable | D4 | ≤ ~1 frame (~16 ms @60 Hz) | same harness (rVFC vs immediate draw) |
| DXGI capture → HW H.264 works + recovers | C2 | 30 FPS, ACCESS_LOST recovery | `spike/latency-probe` capture skeleton (Windows) |

## 2. Decomposition (why two, decoupled, probes)

We deliberately split the pipeline so each half is measurable independently:

- **`iroh-probe`** (transport only): a server/client Rust binary that connects two endpoints over
  Iroh 1.x, reports **direct vs relayed** + home relay, measures handshake time + per-frame RTT while
  streaming fixed-size dummy "frames". Cross-platform; run it across the network matrix.
- **`latency-probe/web`** (controller half only): a **self-contained WebCodecs encode→decode→canvas
  loopback** — generates an animated frame-counter, H.264-encodes it (no B-frames, realtime),
  decodes it, renders it, and measures encode/decode/present latency by carrying `performance.now()`
  in each frame's timestamp. Runs **turnkey in Edge / WebView2** with zero build — validates the
  WebCodecs path, avcC-vs-annexB handling, `VideoFrame.close()` discipline, and the compositor frame.
- **`latency-probe` (Rust, Windows)**: the DXGI→Media-Foundation capture/encode half, scaffolded
  behind a `FrameSource` trait with a working **synthetic** source now and a documented **Windows
  DXGI+MF** source to implement (exact API sequence in `docs/10`/`docs/11`). Its Annex-B output plugs
  into the web harness to measure true glass-to-glass.

Rationale: if we measured only end-to-end and it missed target, we couldn't tell which stage to
blame. Decoupled probes localize the cost.

## 3. Measurement methodology (per `SKILLS.md` P4)
- Fix the **workload** (static doc / IDE / scrolling / video) and **network profile** (LAN / RTT /
  loss) before each run; report numbers *with* that context — never a bare "X ms".
- Report **per-stage**: encode · network/RTT · decode · present, plus end-to-end.
- Run the iroh probe across the full **network matrix** (`docs/08 §3`): same-LAN, different NATs,
  **symmetric NAT**, **UDP-blocked/443-only**, relay-only, and a Wi-Fi↔hotspot migration.

## 4. Go / No-Go (recorded as an ADR after the run)
- **GO** if: latency targets look achievable, direct+relay both work across the matrix, and WebCodecs
  in WebView2 meets the budget → proceed to real Phase 1 on the WebCodecs path.
- **PIVOT** if: the compositor frame or WebView2 IPC blows the budget → switch the MVP to the
  native-surface render path (`docs/12 §5`); if a codec/encoder issue → revisit `docs/10 §3`.
- **NO-GO / re-plan** if: Iroh can't achieve acceptable direct/relay success on the target networks →
  reconsider the transport bet.

## 5. Caveats for the implementer
- **Iroh 1.x API is young** — `iroh-probe` is written against the documented 1.x surface
  (`EndpointId`, `Endpoint::builder()/connect()/accept()`, `conn_type`); `cargo build` on your pinned
  version and reconcile any drift against `cargo doc -p iroh`. Marked with `// VERIFY:` at the
  uncertain calls.
- The **web harness's encoder** stands in for the host encoder to make the controller half runnable
  now; real numbers for the *capture→encode* stage come from the Windows `FrameSource`.
- Everything here is **throwaway** — do not carry spike code into Phase 1; carry the *numbers* and
  the go/no-go ADR.
