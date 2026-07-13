//! Casual RAS — Phase-S transport spike (throwaway).
//!
//! Measures Iroh 1.x connectivity: direct-vs-relay, handshake time, and per-frame RTT while
//! streaming fixed-size dummy "frames" (stand-ins for encoded video chunks).
//!
//! Usage:
//!   server:  cargo run -p iroh-probe -- server
//!   client:  cargo run -p iroh-probe -- client <ENDPOINT_ID printed by the server>
//!
//! NOTE: the Iroh 1.x API is young; `// VERIFY:` marks calls most likely to have drifted between
//! patch releases. Build against your pinned version and reconcile with `cargo doc -p iroh --open`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::{Endpoint, RelayMode};

const ALPN: &[u8] = b"casual-ras/spike/1";
const FRAMES: usize = 300; // ~10 s at 30 fps
const FRAME_BYTES: usize = 12_000; // a modest encoded-frame stand-in
const HDR: usize = 8; // seq (u64 LE) echoed back for RTT

#[tokio::main]
async fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "server" => server().await,
        "client" => {
            let peer = std::env::args()
                .nth(2)
                .context("usage: iroh-probe client <ENDPOINT_ID>")?;
            client(&peer).await
        }
        _ => {
            eprintln!("usage: iroh-probe <server | client ENDPOINT_ID>");
            Ok(())
        }
    }
}

/// Build an endpoint bound to our spike ALPN, using the default n0 relays + n0 discovery.
async fn endpoint() -> Result<Endpoint> {
    let ep = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .discovery_n0() // VERIFY: discovery method name on your version
        .bind()
        .await?;
    Ok(ep)
}

async fn server() -> Result<()> {
    let ep = endpoint().await?;
    println!("ENDPOINT_ID: {}", ep.id()); // VERIFY: Endpoint::id()
    println!("waiting for a client (run: iroh-probe client <ENDPOINT_ID>) ...");

    while let Some(incoming) = ep.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_conn(incoming).await {
                eprintln!("connection ended: {e:#}");
            }
        });
    }
    Ok(())
}

// VERIFY: the accept item type + `.await` into a Connection.
async fn handle_conn(incoming: iroh::endpoint::Incoming) -> Result<()> {
    let conn = incoming.await?;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let mut buf = vec![0u8; FRAME_BYTES];
    let mut n = 0usize;
    // Echo the 8-byte header of each fixed-size frame back to the client.
    loop {
        match recv.read_exact(&mut buf).await {
            Ok(()) => {
                send.write_all(&buf[0..HDR]).await?;
                n += 1;
            }
            Err(_) => break, // stream finished/closed
        }
    }
    println!("server: echoed {n} frames");
    Ok(())
}

async fn client(peer: &str) -> Result<()> {
    let ep = endpoint().await?;
    let peer_id: iroh::EndpointId = peer.parse().context("bad ENDPOINT_ID")?; // VERIFY: FromStr

    let t0 = Instant::now();
    let conn = ep.connect(peer_id, ALPN).await?; // VERIFY: connect(impl Into<EndpointAddr>, alpn)
    let handshake = t0.elapsed();
    println!("connected in {:.1} ms", handshake.as_secs_f64() * 1000.0);
    report_path(&ep, peer_id).await;

    let (mut send, mut recv) = conn.open_bi().await?;
    let payload = vec![0u8; FRAME_BYTES];
    let mut rtts: Vec<Duration> = Vec::with_capacity(FRAMES);
    let mut hdr = [0u8; HDR];

    for seq in 0..FRAMES {
        let mut frame = payload.clone();
        frame[0..HDR].copy_from_slice(&(seq as u64).to_le_bytes());
        let t = Instant::now();
        send.write_all(&frame).await?;
        recv.read_exact(&mut hdr).await?;
        rtts.push(t.elapsed());
        tokio::time::sleep(Duration::from_millis(33)).await; // ~30 fps pacing
    }
    send.finish().ok(); // VERIFY: finish() signature

    print_stats(&rtts);
    Ok(())
}

/// Best-effort direct-vs-relay report. VERIFY: `conn_type` API on your version.
async fn report_path(ep: &Endpoint, peer: iroh::EndpointId) {
    match ep.conn_type(peer) {
        Ok(watcher) => match watcher.get() {
            Ok(ct) => println!("connection type: {ct:?}  (Direct = hole-punched, Relay = via relay)"),
            Err(_) => println!("connection type: <unavailable>"),
        },
        Err(_) => println!("connection type: <conn_type API differs — check cargo doc -p iroh>"),
    }
}

fn print_stats(rtts: &[Duration]) {
    if rtts.is_empty() {
        println!("no RTT samples");
        return;
    }
    let mut ms: Vec<f64> = rtts.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f64| ms[((ms.len() as f64 - 1.0) * p).round() as usize];
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    println!(
        "frames: {}  RTT ms  min {:.1}  median {:.1}  p95 {:.1}  max {:.1}  mean {:.1}",
        ms.len(),
        ms[0],
        pct(0.50),
        pct(0.95),
        ms[ms.len() - 1],
        mean
    );
    println!("(record connection type + these RTTs per network profile — see spike/README.md)");
}
