//! Casual RAS **host** — alpha CLI (M2 view-only).
//!
//! Binds an iroh endpoint, captures this machine's screen, and serves it to a remote controller over
//! the real iroh transport (`IrohSessionTransport` behind the `SessionTransport` seam — the exact
//! path the loopback e2e tests exercise, now over the network). It prints a **connection ticket**;
//! paste that into the controller to view this screen from another machine.
//!
//! Alpha scope + honesty:
//! - **View-only.** No input injection, no support actions yet.
//! - **Consent is a no-op seam** (`AllowAllValidator`, Phase-1): anyone with the ticket who reaches
//!   this endpoint is served. The host **consent window** (Invariant 1/7 — approve/deny + a real
//!   overlay indicator) lands with the host GUI. For now the indicator is this process's own,
//!   always-on terminal banner and connect/disconnect log, and **Ctrl-C is the stop control**
//!   (always present). Do not run this on a machine you would not hand to a stranger.
//! - **macOS only** so far (`ras-media-macos`). On other platforms the binary compiles but prints an
//!   "unsupported" notice — the Linux/Windows capture backends are the next port.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "ras-host: no screen-capture backend for this platform yet (macOS only in the alpha).\n\
         The Linux (PipeWire/VAAPI) and Windows (DXGI/Media Foundation) host backends are the next port."
    );
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    mac::run().await
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::Arc;

    use ras_core::{
        AllowAllValidator, HostSession, HostSessionConfig, IrohSessionTransport, LifecycleEvent,
        StopReason,
    };
    use ras_media::MonitorId;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};
    use ras_transport_iroh::{Endpoint, EndpointId, Session};
    use tokio::sync::watch;

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint = Arc::new(Endpoint::bind().await?);
        eprintln!("ras-host: contacting a relay for a reachable address…");
        endpoint.online().await;
        let ticket = endpoint.addr().to_ticket();
        print_banner(&ticket);

        // One shutdown signal shared by every loop; the first Ctrl-C exits the whole host cleanly.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown_tx.send(true);
        });

        // Serve one viewer at a time; loop so a viewer can reconnect without restarting the host.
        loop {
            let mut sd = shutdown_rx.clone();
            tokio::select! {
                _ = sd.changed() => break,
                accepted = endpoint.accept() => match accepted {
                    Ok(Some(session)) => serve_one(&endpoint, session, shutdown_rx.clone()).await,
                    Ok(None) => break, // endpoint closed
                    Err(_) => continue, // transient accept error; keep listening
                },
            }
        }

        println!("\nras-host: stopping.");
        endpoint.close().await;
        Ok(())
    }

    /// Serve exactly one connected controller until it disconnects or the host is shut down.
    async fn serve_one(
        endpoint: &Arc<Endpoint>,
        session: Session,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let peer = session.remote();
        println!(
            "\n🔴 VIEWER CONNECTED [{}] — REMOTE VIEWING IS ACTIVE. Press Ctrl-C to stop.",
            short_id(&peer)
        );

        let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            transport,
            MacScreenCapture::new(),
            VideoToolboxEncoder::new(),
            Arc::new(AllowAllValidator),
        );

        let mut events = match host.start().await {
            Ok(events) => events,
            Err(e) => {
                eprintln!("ras-host: session failed to start: {e}");
                return;
            }
        };

        // Run until the viewer's session ends (transport drop → terminal event / closed stream) or a
        // Ctrl-C shuts the host down. Remote-pointer events are logged (throttled) so a two-machine
        // test can confirm the "look here" pointer arrives over the network — the on-screen overlay
        // that draws it lands with the host GUI.
        let mut ptr_count = 0u64;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    host.stop(StopReason::UserRequested).await;
                    break;
                }
                ev = events.recv() => match ev {
                    Some(LifecycleEvent::SessionEnded { .. })
                    | Some(LifecycleEvent::Revoked { .. })
                    | Some(LifecycleEvent::Disconnected { .. })
                    | None => break,
                    Some(LifecycleEvent::RemotePointer { x, y, visible }) => {
                        ptr_count += 1;
                        if visible && ptr_count.is_multiple_of(12) {
                            println!(
                                "   👉 viewer pointing at {}% , {}%",
                                u32::from(x) * 100 / 65535,
                                u32::from(y) * 100 / 65535
                            );
                        }
                    }
                    _ => {}
                },
            }
        }

        println!(
            "⚪ Viewer [{}] disconnected. Waiting for the next viewer…",
            short_id(&peer)
        );
    }

    fn print_banner(ticket: &str) {
        println!("\n╔══════════════════════════════════════════════════════════════╗");
        println!("║  Casual RAS host — alpha (view-only)                          ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!("\nShare this connection ticket with the person who will view this screen:\n");
        println!("  {ticket}\n");
        println!("They paste it into the Casual RAS controller to connect.");
        println!("This window stays open while sharing. Press Ctrl-C to stop at any time.\n");
        println!("Waiting for a viewer to connect…");
    }

    /// A short, log-safe rendering of a peer identity (first 8 hex of the Ed25519 key). Never logs
    /// the full key material verbatim as a secret would be — this is a public identity, but keep it
    /// terse for the terminal.
    fn short_id(id: &EndpointId) -> String {
        let mut s = String::with_capacity(8);
        for b in id.0.iter().take(4) {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
        }
        s
    }
}
