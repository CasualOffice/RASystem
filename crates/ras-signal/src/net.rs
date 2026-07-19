//! The live signaling layer: the `casual-ras/signal/1` ALPN for direct signed messages +
//! access-request intents between contacts (ADR-095), and the gossip presence runtime (ADR-094).
//!
//! Structure (so the risky part is testable and the untestable part is thin): the **wire codec** —
//! read one signal off a stream and verify it contacts-only — is generic over any async stream and is
//! unit-tested here over an in-memory duplex, with no network. The **iroh connection glue**
//! (`open_bi`/`accept_bi` + the delivery ACK) and the **gossip runtime** (`subscribe` + beacon loop)
//! are thin wrappers over that codec + the pure presence tracker; per the repo's convention for
//! concrete iroh (e.g. ADR-091's re-dial), their live two-endpoint behaviour is an on-device
//! verification step, not a hermetic test.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use ras_identity::{ContactBook, KeyStore};
use ras_protocol::{ErrorCode, RasError};

use crate::presence::PresenceTracker;
use crate::{
    encode_signed, verify_signed, SignalError, SignalPayload, VerifiedSignal, MAX_SIGNAL_FRAME,
};

/// ALPN for direct contact signaling. Distinct from the session/bootstrap ALPNs so an accept loop
/// routes a signal connection to [`recv_signal`], never to a media session (Inv 9 — separate planes).
pub const SIGNAL_ALPN: &[u8] = b"casual-ras/signal/1";

/// One-byte application ACK the receiver returns once a signal is received AND accepted (verified
/// contacts-only). The sender waits for it before closing — the delivery guarantee learned from the
/// bootstrap grant-drain fix (never drop a connection with data the peer has not acknowledged).
const ACK_OK: u8 = 1;

fn net_err(ctx: &'static str) -> SignalError {
    RasError::recoverable(ErrorCode::TransportError, ctx)
}

/// Wall-clock ms since the Unix epoch. A pre-epoch clock saturates to 0 (fail-closed: everything reads
/// "not yet valid"). This is the impure network layer, so reading the clock here is fine.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ─── Generic wire codec (hermetically testable, no network) ─────────────────────────────────────

/// Write a signed signal to a stream and flush. Exactly one signal per stream; the caller FINs the
/// send side afterwards so the reader's EOF delimits it.
async fn write_signal_bytes<W: AsyncWrite + Unpin>(
    w: &mut W,
    bytes: &[u8],
) -> Result<(), SignalError> {
    w.write_all(bytes)
        .await
        .map_err(|_| net_err("signal write failed"))?;
    w.flush()
        .await
        .map_err(|_| net_err("signal flush failed"))?;
    Ok(())
}

/// Read one signal off a stream (to EOF, **bounded**) and verify it contacts-only. A hostile peer
/// cannot stream unbounded bytes: the read is capped at `MAX_SIGNAL_FRAME + 1`, and anything larger is
/// refused before verification.
async fn read_and_verify<R: AsyncRead + Unpin>(
    r: &mut R,
    book: &dyn ContactBook,
    now: u64,
    max_age_ms: u64,
) -> Result<VerifiedSignal, SignalError> {
    let mut buf = Vec::with_capacity(256);
    let mut limited = r.take((MAX_SIGNAL_FRAME as u64) + 1);
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|_| net_err("signal read failed"))?;
    if buf.len() > MAX_SIGNAL_FRAME {
        return Err(net_err("signal too large"));
    }
    verify_signed(&buf, book, now, max_age_ms)
}

// ─── iroh connection glue (compile-verified; live behaviour verified on-device) ─────────────────

/// Dial a contact and deliver one signed signal, **waiting for the receiver's ACK before closing** so
/// the signal is never lost to an early connection drop (the grant-drain lesson). `target` carries the
/// contact's `EndpointId` (+ optional addr hints); the QUIC/TLS handshake authenticates it (Inv 9).
pub async fn send_signal(
    endpoint: &iroh::Endpoint,
    target: impl Into<iroh::EndpointAddr>,
    ks: &dyn KeyStore,
    payload: &SignalPayload,
) -> Result<(), SignalError> {
    let bytes = encode_signed(ks, payload)?;
    let conn = endpoint
        .connect(target, SIGNAL_ALPN)
        .await
        .map_err(|_| net_err("signal connect failed"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|_| net_err("signal open_bi failed"))?;
    write_signal_bytes(&mut send, &bytes).await?;
    send.finish().map_err(|_| net_err("signal finish failed"))?;
    let mut ack = [0u8; 1];
    let got = recv.read_exact(&mut ack).await;
    conn.close(0u32.into(), b"done");
    match got {
        Ok(()) if ack[0] == ACK_OK => Ok(()),
        _ => Err(net_err("signal not acknowledged")),
    }
}

/// Accept one signed signal on an inbound `SIGNAL_ALPN` connection, verify it contacts-only, and ACK
/// **only on success**. Called by the app's accept loop when it routes a `SIGNAL_ALPN` connection here.
/// Returns the verified message / access-request intent (the caller surfaces a chat line, or — for an
/// intent — a local consent prompt, Inv 1). On any verification failure it returns `Err` and does NOT
/// ACK, so the sender learns it was refused (no silent drop of a stranger's signal — deny-by-default).
pub async fn recv_signal(
    conn: &iroh::endpoint::Connection,
    book: &dyn ContactBook,
    now: u64,
    max_age_ms: u64,
) -> Result<VerifiedSignal, SignalError> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|_| net_err("signal accept_bi failed"))?;
    let verified = read_and_verify(&mut recv, book, now, max_age_ms).await?;
    send.write_all(&[ACK_OK])
        .await
        .map_err(|_| net_err("ack write failed"))?;
    send.finish().map_err(|_| net_err("ack finish failed"))?;
    Ok(verified)
}

// ─── Gossip presence runtime (compile-verified; live behaviour verified on-device) ──────────────

/// Timing for the presence loop.
#[derive(Debug, Clone, Copy)]
pub struct PresenceParams {
    /// How often to broadcast a signed "online" beacon.
    pub beacon_every: Duration,
    /// How long a beacon counts as "online" before a contact is treated as offline (should be a small
    /// multiple of `beacon_every` so a couple of dropped beacons don't flap the state).
    pub freshness_ms: u64,
}

/// A running pairwise-presence task. Dropping it aborts the beacon loop and leaves the topic.
pub struct PresenceHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PresenceHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the pairwise presence loop for one contact: subscribe to `topic` (bootstrapped by the
/// contact's `peer` endpoint), broadcast a **signed** [`SignalPayload::PresenceBeacon`] every
/// `beacon_every`, and feed **verified** inbound beacons (contacts-only) into `tracker`. Returns once
/// subscribed; the loop runs until the handle is dropped or the topic closes.
///
/// Gossip carries only these tiny signed beacons — never message bodies or anything an unverified
/// forwarder could forge into meaning (ADR-094): a beacon that fails [`verify_signed`] (bad signature,
/// not a saved contact, stale) is dropped, so a topic-guesser learns nothing and can inject nothing.
pub async fn spawn_presence(
    gossip: &iroh_gossip::net::Gossip,
    topic: iroh_gossip::TopicId,
    peer: iroh::EndpointId,
    ks: Arc<dyn KeyStore>,
    book: Arc<dyn ContactBook>,
    tracker: Arc<Mutex<PresenceTracker>>,
    params: PresenceParams,
) -> Result<PresenceHandle, SignalError> {
    use iroh_gossip::api::Event;
    use tokio_stream::StreamExt;

    let PresenceParams {
        beacon_every,
        freshness_ms,
    } = params;
    let sub = gossip
        .subscribe(topic, vec![peer])
        .await
        .map_err(|_| net_err("gossip subscribe failed"))?;
    let (sender, mut receiver) = sub.split();

    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(beacon_every);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Ok(bytes) = encode_signed(
                        &*ks,
                        &SignalPayload::PresenceBeacon { issued_at: now_ms() },
                    ) {
                        // Best-effort broadcast (gossip is lossy by design); a failure just means this
                        // heartbeat did not go out — the next one will.
                        let _ = sender.broadcast(bytes.into()).await;
                    }
                }
                ev = receiver.next() => match ev {
                    Some(Ok(Event::Received(msg))) => {
                        // Verify contacts-only before trusting a beacon; `delivered_from` is never the
                        // author (multi-hop gossip), so only the signature + contact check count.
                        if let Ok(v) = verify_signed(&msg.content, &*book, now_ms(), freshness_ms) {
                            if matches!(v.payload, SignalPayload::PresenceBeacon { .. }) {
                                if let Ok(mut t) = tracker.lock() {
                                    t.observe(v.sender, now_ms());
                                }
                            }
                        }
                    }
                    // NeighborUp/Down/Lagged and transient errors: nothing to do for MVP presence
                    // (staleness handles departures; a dropped beacon reads as offline).
                    Some(_) => {}
                    // The topic closed (both handles dropped elsewhere / gossip shut down).
                    None => break,
                }
            }
        }
    });
    Ok(PresenceHandle { task })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ras_identity::{Contact, ContactId, InMemoryContactBook, SoftwareKeyStore};

    const MAX_AGE: u64 = 60_000;

    fn book_with(ks: &SoftwareKeyStore, blocked: bool) -> InMemoryContactBook {
        let book = InMemoryContactBook::new();
        book.upsert(Contact {
            id: ContactId::from_bytes(ks.public_key()),
            label: "peer".into(),
            added_at: 0,
            last_seen_at: 0,
            blocked,
        });
        book
    }

    // The wire codec: a signed signal written to one end of a duplex is read + verified at the other.
    #[tokio::test]
    async fn a_signal_reads_and_verifies_over_a_stream() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let bytes = encode_signed(
            &ks,
            &SignalPayload::AccessRequestIntent {
                issued_at: 1000,
                reason: "remote support".into(),
            },
        )
        .unwrap();

        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_signal_bytes(&mut writer, &bytes).await.unwrap();
        writer.shutdown().await.unwrap(); // FIN so the reader's read_to_end sees EOF

        let v = read_and_verify(&mut reader, &book, 1000, MAX_AGE)
            .await
            .unwrap();
        assert_eq!(v.sender, ContactId::from_bytes(ks.public_key()));
        assert!(matches!(
            v.payload,
            SignalPayload::AccessRequestIntent { .. }
        ));
    }

    // A signal from a non-contact is refused at the stream boundary (contacts-only, deny-by-default).
    #[tokio::test]
    async fn a_stream_signal_from_a_non_contact_is_refused() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let empty = InMemoryContactBook::new(); // sender not saved
        let bytes = encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 1 }).unwrap();

        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_signal_bytes(&mut writer, &bytes).await.unwrap();
        writer.shutdown().await.unwrap();

        let err = read_and_verify(&mut reader, &empty, 1, MAX_AGE)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CapabilityDenied);
    }

    // An over-large stream is refused before verification (bounded read).
    #[tokio::test]
    async fn an_oversize_stream_is_refused() {
        let book = InMemoryContactBook::new();
        let (mut writer, mut reader) = tokio::io::duplex(MAX_SIGNAL_FRAME * 2);
        writer
            .write_all(&vec![0u8; MAX_SIGNAL_FRAME + 100])
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let err = read_and_verify(&mut reader, &book, 0, MAX_AGE)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::TransportError); // "signal too large"
    }
}
