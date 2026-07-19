//! Async runtime + session event-callback ABI for the session SDK (ADR-096, async follow-up).
//!
//! This lands the **callback model** — the hard, reusable part of an async C SDK: a `Send`-safe C
//! event callback + an opaque tokio-runtime handle + a proven event-delivery path from a worker
//! thread. The session/host/controller wiring (connect over iroh → `LifecycleEvent` → these events)
//! is the next increment and builds directly on this.

use std::ffi::c_void;

use crate::{guard, CasualRasStatus};

/// A session lifecycle event delivered to the C callback. Mirrors the core `LifecycleEvent` kinds a
/// consumer acts on. **Content-free** (Inv 8): an enum tag only — never pixels/keystrokes/secrets.
///
/// `dead_code` is allowed: this is a public C ABI enum — a C consumer `switch`es on every variant, and
/// the session emitter (next increment) constructs the ones the test driver does not yet.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasualRasEvent {
    /// The session is live.
    Connected = 0,
    /// Transport lost; resuming within the reconnect window (ADR-091).
    Suspended = 1,
    /// Resumed after a transport loss.
    Resumed = 2,
    /// The session ended (disconnect / revoke / teardown).
    Ended = 3,
}

/// The C session-event callback. `user_data` is the opaque context the caller registered.
///
/// **Invoked from an internal worker thread**, so it must be thread-safe; it must not block or re-enter
/// the SDK in a way that deadlocks. Only content-free events cross it (Inv 8).
pub type CasualRasEventCallback = extern "C" fn(user_data: *mut c_void, event: CasualRasEvent);

/// A callback bound to its `user_data`, made `Send` so it can move into an async task.
pub(crate) struct SendCallback {
    cb: CasualRasEventCallback,
    user_data: *mut c_void,
}
// SAFETY: the C caller owns `user_data` and guarantees (per the ABI contract on the callback type)
// that it is valid to use from another thread for the callback's lifetime. The `cb` is a plain fn
// pointer (already `Send`); only the raw `user_data` needs this assertion.
unsafe impl Send for SendCallback {}

impl SendCallback {
    fn fire(&self, event: CasualRasEvent) {
        (self.cb)(self.user_data, event);
    }
}

/// Opaque handle owning a multi-threaded async runtime for the SDK's sessions. Free with
/// `casual_ras_runtime_free`.
pub struct CasualRasRuntime {
    rt: tokio::runtime::Runtime,
}

/// Create a new async runtime; on success writes a handle to `*out_runtime`.
///
/// # Safety
/// `out_runtime` must be a valid, writable pointer to a `CasualRasRuntime*`.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_runtime_new(
    out_runtime: *mut *mut CasualRasRuntime,
) -> CasualRasStatus {
    guard(|| {
        if out_runtime.is_null() {
            return CasualRasStatus::NullArgument;
        }
        match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => {
                // SAFETY: checked non-null above.
                unsafe { *out_runtime = Box::into_raw(Box::new(CasualRasRuntime { rt })) };
                CasualRasStatus::Ok
            }
            Err(_) => CasualRasStatus::Internal,
        }
    })
}

/// Free a runtime handle (a NULL pointer is a no-op). Shuts the runtime down (in-flight tasks are
/// dropped). Never call twice on the same handle.
///
/// # Safety
/// `runtime` must be a handle from `casual_ras_runtime_new`, or NULL.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_runtime_free(runtime: *mut CasualRasRuntime) {
    if !runtime.is_null() {
        // SAFETY: reclaim the Box we leaked in the constructor.
        drop(unsafe { Box::from_raw(runtime) });
    }
}

/// Foundational proof of the callback model: spawn a task on the runtime that delivers one
/// [`CasualRasEvent::Connected`] to `callback` from a worker thread. This validates that the
/// `Send`-safe callback + runtime path works end-to-end; the real session emitter replaces it in the
/// next increment. Returns immediately (the event arrives asynchronously).
///
/// # Safety
/// `runtime` a valid runtime handle; `callback` a valid function pointer; `user_data` valid for the
/// callback for as long as the runtime may invoke it.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_runtime_emit_test_event(
    runtime: *const CasualRasRuntime,
    callback: CasualRasEventCallback,
    user_data: *mut c_void,
) -> CasualRasStatus {
    guard(|| {
        // SAFETY: caller guarantees a live runtime handle.
        let Some(rt) = (unsafe { runtime.as_ref() }) else {
            return CasualRasStatus::NullArgument;
        };
        let sc = SendCallback {
            cb: callback,
            user_data,
        };
        rt.rt.spawn(async move {
            sc.fire(CasualRasEvent::Connected);
        });
        CasualRasStatus::Ok
    })
}

/// Map a core [`ras_core::LifecycleEvent`] to the C ABI [`CasualRasEvent`]. Returns `None` for events
/// the SDK does not surface yet (connecting / quality / pointer / geometry / in-session chat / …) — the
/// session-connect FFI will grow the mapping as it exposes more.
///
/// `dead_code` allowed: exercised by tests today; the session handle (next increment) is its non-test
/// caller.
#[allow(dead_code)]
fn map_lifecycle(ev: &ras_core::LifecycleEvent) -> Option<CasualRasEvent> {
    use ras_core::LifecycleEvent as L;
    match ev {
        // Control channel up ⇒ the session is usable.
        L::SessionReady { .. } => Some(CasualRasEvent::Connected),
        L::Suspended { .. } => Some(CasualRasEvent::Suspended),
        L::Resumed => Some(CasualRasEvent::Resumed),
        L::SessionEnded { .. } | L::Disconnected { .. } | L::Revoked { .. } => {
            Some(CasualRasEvent::Ended)
        }
        _ => None,
    }
}

/// Pump a session's [`ras_core::LifecycleStream`] to the C callback on the runtime until the stream
/// closes, mapping each core event to a [`CasualRasEvent`]. A final `Ended` is delivered on close if no
/// terminal event was already sent — so a consumer always sees exactly one end. This is the
/// event-delivery core the (next-increment) session handle drives; the connect/transport wiring plugs a
/// real `ControllerSession`'s stream in here.
#[allow(dead_code)]
pub(crate) fn spawn_lifecycle_drain(
    rt: &tokio::runtime::Runtime,
    mut stream: ras_core::LifecycleStream,
    cb: SendCallback,
) {
    rt.spawn(async move {
        let mut ended = false;
        while let Some(ev) = stream.recv().await {
            if let Some(mapped) = map_lifecycle(&ev) {
                ended |= mapped == CasualRasEvent::Ended;
                cb.fire(mapped);
            }
        }
        if !ended {
            cb.fire(CasualRasEvent::Ended); // stream closed without an explicit terminal event
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    extern "C" fn capture_cb(user_data: *mut c_void, event: CasualRasEvent) {
        // `user_data` is a `&Sender<CasualRasEvent>` the test passed in.
        let tx = unsafe { &*(user_data as *const Sender<CasualRasEvent>) };
        let _ = tx.send(event);
    }

    #[test]
    fn runtime_delivers_an_event_to_a_c_callback_from_a_worker_thread() {
        let mut rt: *mut CasualRasRuntime = std::ptr::null_mut();
        assert_eq!(
            unsafe { casual_ras_runtime_new(&mut rt) },
            CasualRasStatus::Ok
        );
        assert!(!rt.is_null());

        let (tx, rx) = std::sync::mpsc::channel::<CasualRasEvent>();
        let tx_ptr = std::ptr::addr_of!(tx) as *mut c_void;
        assert_eq!(
            unsafe { casual_ras_runtime_emit_test_event(rt, capture_cb, tx_ptr) },
            CasualRasStatus::Ok
        );

        // The event is delivered asynchronously from a runtime worker thread.
        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(event, CasualRasEvent::Connected);

        unsafe { casual_ras_runtime_free(rt) };
    }

    #[test]
    fn runtime_new_rejects_null_out() {
        assert_eq!(
            unsafe { casual_ras_runtime_new(std::ptr::null_mut()) },
            CasualRasStatus::NullArgument
        );
        // Freeing NULL is a safe no-op.
        unsafe { casual_ras_runtime_free(std::ptr::null_mut()) };
    }

    // The event-delivery core: a synthetic lifecycle stream is mapped + delivered to the C callback,
    // and a final Ended is synthesized when the stream closes.
    #[test]
    fn lifecycle_drain_maps_core_events_to_the_callback() {
        use ras_core::LifecycleEvent;
        let mut rt: *mut CasualRasRuntime = std::ptr::null_mut();
        assert_eq!(
            unsafe { casual_ras_runtime_new(&mut rt) },
            CasualRasStatus::Ok
        );
        let runtime = unsafe { &*rt };

        let (tx, rx) = std::sync::mpsc::channel::<CasualRasEvent>();
        let (ltx, lstream) = tokio::sync::mpsc::channel::<LifecycleEvent>(8);
        let sc = SendCallback {
            cb: capture_cb,
            user_data: std::ptr::addr_of!(tx) as *mut c_void,
        };
        spawn_lifecycle_drain(&runtime.rt, lstream, sc);

        runtime.rt.block_on(async {
            ltx.send(LifecycleEvent::Suspended { since_ms: 0 })
                .await
                .unwrap();
            ltx.send(LifecycleEvent::Resumed).await.unwrap();
        });
        drop(ltx); // close the stream → a final Ended is synthesized

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            CasualRasEvent::Suspended
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            CasualRasEvent::Resumed
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            CasualRasEvent::Ended
        );

        unsafe { casual_ras_runtime_free(rt) };
    }
}
