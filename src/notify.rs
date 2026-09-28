//! Notification delivery (DESIGN.md §3.5): durable queue, best-effort
//! transport, late beats lost.
//!
//! Enqueue (a store write riding the step-close transaction) is what makes a
//! Notify step "succeed"; this module is only the delivery half. It resolves
//! the session bus at *delivery* time — never from the captured env (§2.1):
//!   1. DBUS_SESSION_BUS_ADDRESS from the daemon's own environment
//!   2. $XDG_RUNTIME_DIR/bus
//!   3. /run/user/<uid>/bus
//!
//! Step 3 is what keeps reminders alive on the cron-@reboot persistence path
//! (§8), where neither env var exists — and why this dials zbus at an
//! explicit address instead of using its default session lookup.
//!
//! Unreachable bus → the row stays undelivered; the daemon retries on a slow
//! tick, so a reminder fired while logged out arrives on next login.
//!
//! At-least-once, not exactly-once: the protocol cannot say whether a popup
//! whose acknowledgement was lost is up. An overdue call is kept and waited
//! on rather than repeated; what remains is documented in §3.5.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use zbus::zvariant::Value;

use crate::model::{DeliveryReceipt, NotifySpec};

/// A wedged bus must not stall the delivery loop (§2.3 spirit). Past this the
/// call is *not* abandoned — the server may already be showing it — only no
/// longer waited for inline.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an unacknowledged call keeps its row from being re-sent. A call
/// normally settles long before this: with its reply, or with an error when
/// the server or bus goes away. Past it, re-sending is the at-least-once
/// choice rather than holding the row forever. Provisional (§12).
const ABANDON_AFTER: Duration = Duration::from_secs(300);

/// What one keyed delivery attempt established (§3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The server acknowledged the display; its receipt when the transport
    /// has one.
    Shown(Option<DeliveryReceipt>),
    /// Nothing to deliver to right now; the rest of the queue waits too.
    Unavailable,
    /// This attempt's acknowledgement did not arrive in time. The call is
    /// still pending and the popup may already be up, so the row must not be
    /// re-sent; a wedged server is not worth more calls this pass.
    TimedOut,
    /// An earlier call for this row is still awaiting its acknowledgement.
    Awaiting,
}

/// Delivery behind a trait (§11, like Clock/Spawner) so tests script
/// outcomes — and so `cargo test` daemons can't pop real desktop
/// notifications.
///
/// `Ok(true)` = delivered; `Ok(false)` = no bus reachable right now (leave
/// the row queued); `Err` = a real failure worth logging. Delivery problems
/// never fail a run (§3.2). Spelled as an explicit `impl Future + Send`
/// (not `async fn`) because the delivery task is `tokio::spawn`ed generically.
pub trait Notifier: Send + Sync {
    fn deliver(
        &self,
        spec: &NotifySpec,
    ) -> impl std::future::Future<Output = Result<bool>> + Send;

    /// Deliver queue row `key` (never reused). A transport that can tell
    /// "no answer yet" from "failed" keeps the call and answers `Awaiting`
    /// for that key instead of sending again; the default has no such state.
    fn attempt(
        &self,
        _key: i64,
        spec: &NotifySpec,
    ) -> impl std::future::Future<Output = Result<Delivery>> + Send {
        let delivered = self.deliver(spec);
        async move {
            Ok(if delivered.await? {
                Delivery::Shown(None)
            } else {
                Delivery::Unavailable
            })
        }
    }

    /// Release calls whose queue rows were pruned or recorded elsewhere.
    /// Called even on an empty queue, so cleanup never needs another timeout.
    fn retain_pending(&self, _keys: &HashSet<i64>) {}

    /// Resolves when an earlier `TimedOut`/`Awaiting` call has settled, so
    /// the late acknowledgement is recorded without waiting for the slow
    /// tick. The default never has one.
    fn settled(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::pending()
    }
}

/// The real thing: org.freedesktop.Notifications over the session bus.
///
/// It remembers, per queue row, a call whose acknowledgement is overdue:
/// the spec gives no way to ask a server whether a popup exists, so the only
/// honest way not to show it twice is to wait for that call's own answer.
/// This lives in memory — a daemon restart forgets it and re-sends
/// (at-least-once, DESIGN.md §3.5).
#[derive(Debug, Clone)]
pub struct DesktopNotifier {
    address: Option<String>,
    ack_wait: Duration,
    abandon_after: Duration,
    overdue: Arc<Mutex<HashMap<i64, Overdue>>>,
    settled: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct Overdue {
    since: Instant,
    call: JoinHandle<Result<DeliveryReceipt>>,
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self::with_limits(None, DELIVERY_TIMEOUT, ABANDON_AFTER)
    }
}

impl DesktopNotifier {
    /// `address` pins the bus (tests: a private one) instead of the §3.5
    /// resolution; the limits are the inline acknowledgement wait and how
    /// long an overdue call holds its row.
    pub fn with_limits(address: Option<String>, ack_wait: Duration, abandon_after: Duration) -> Self {
        Self {
            address,
            ack_wait,
            abandon_after,
            overdue: Arc::default(),
            settled: Arc::default(),
        }
    }

    fn address(&self) -> Option<String> {
        self.address.clone().or_else(session_bus_address)
    }

    fn overdue(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Overdue>> {
        self.overdue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Settle what an earlier call for `key` left behind. `Some` answers the
    /// attempt; `None` means send (again) now.
    async fn resume(&self, key: i64) -> Option<Result<Delivery>> {
        let previous = self.overdue().remove(&key)?;
        if !previous.call.is_finished() {
            if previous.since.elapsed() < self.abandon_after {
                self.overdue().insert(key, previous);
                return Some(Ok(Delivery::Awaiting));
            }
            // At-least-once: the old call may yet show; holding the row
            // forever could lose it.
            previous.call.abort();
            eprintln!(
                "cued: notification {key} unacknowledged after {:?}; sending again",
                self.abandon_after
            );
            return None;
        }
        match previous.call.await {
            Ok(Ok(receipt)) => Some(Ok(Delivery::Shown(Some(receipt)))),
            // Failed without acknowledging — typically the server or bus went
            // away mid-call. Whether it displayed first is unknowable; the
            // popup died with that server either way, so send again now.
            Ok(Err(error)) => {
                eprintln!("cued: overdue notification {key} failed: {error:#}; sending again");
                None
            }
            Err(join) => {
                eprintln!("cued: overdue notification {key} ended: {join}; sending again");
                None
            }
        }
    }
}

impl Notifier for DesktopNotifier {
    async fn deliver(&self, spec: &NotifySpec) -> Result<bool> {
        let Some(address) = self.address() else {
            return Ok(false);
        };
        match tokio::time::timeout(self.ack_wait, send(address.clone(), spec.clone())).await {
            Ok(result) => result.map(|_| true),
            Err(_) => anyhow::bail!("notification delivery timed out against {address}"),
        }
    }

    async fn attempt(&self, key: i64, spec: &NotifySpec) -> Result<Delivery> {
        if let Some(settled) = self.resume(key).await {
            return settled;
        }
        let Some(address) = self.address() else {
            return Ok(Delivery::Unavailable);
        };
        // Spawned, so the call outlives this wait: dropping it here is what
        // used to turn a slow acknowledgement into a second popup. Only an
        // overdue call signals `settled` — one answered inline must not wake
        // the loop into an immediate retry (a fast failure would spin).
        let overdue = Arc::new(AtomicBool::new(false));
        let mut call = tokio::spawn({
            let spec = spec.clone();
            let overdue = Arc::clone(&overdue);
            let settled = Arc::clone(&self.settled);
            async move {
                let result = send(address, spec).await;
                if overdue.load(Ordering::SeqCst) {
                    settled.notify_one();
                }
                result
            }
        });
        match tokio::time::timeout(self.ack_wait, &mut call).await {
            Ok(Ok(result)) => result.map(|receipt| Delivery::Shown(Some(receipt))),
            Ok(Err(join)) => Err(join).context("notification call task"),
            Err(_) => {
                overdue.store(true, Ordering::SeqCst);
                if call.is_finished() {
                    // Answered between the timeout and the flag: signal here.
                    self.settled.notify_one();
                }
                let mut overdue = self.overdue();
                // Rows deleted while overdue are never attempted again.
                let abandon_after = self.abandon_after;
                overdue.retain(|_, stale| {
                    let keep = stale.since.elapsed() < abandon_after;
                    if !keep {
                        stale.call.abort();
                    }
                    keep
                });
                overdue.insert(key, Overdue { since: Instant::now(), call });
                Ok(Delivery::TimedOut)
            }
        }
    }

    fn retain_pending(&self, keys: &HashSet<i64>) {
        self.overdue().retain(|key, pending| {
            if keys.contains(key) { return true; }
            pending.call.abort();
            false
        });
    }

    async fn settled(&self) {
        self.settled.notified().await
    }
}

/// The §3.5 resolution, re-run on every attempt — buses appear at login.
fn session_bus_address() -> Option<String> {
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        return Some(address.to_string_lossy().into_owned());
    }
    let candidates = [
        std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("bus")),
        Some(PathBuf::from(format!("/run/user/{}/bus", unsafe { libc::getuid() }))),
    ];
    for socket in candidates.into_iter().flatten() {
        if socket.exists() {
            return Some(format!("unix:path={}", socket.display()));
        }
    }
    None
}

/// The Desktop Notifications spec's one method, hand-dialed — to the
/// well-known name, so an on-demand server is still bus-activated. The
/// server's identity comes from the reply's sender: the unique name of the
/// lifetime that issued the ID. A server that dies mid-call fails the call
/// (the bus answers NoReply); a successor never answers for it.
async fn send(address: String, spec: NotifySpec) -> Result<DeliveryReceipt> {
    let connection = zbus::connection::Builder::address(address.as_str())
        .with_context(|| format!("bad bus address {address:?}"))?
        .build()
        .await
        .with_context(|| format!("connecting to session bus at {address}"))?;
    let reply = connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "cued",
                0u32,          // never replaces: IDs don't survive server lifetimes,
                               // and a known-shown row is never re-sent (§3.5)
                "appointment", // themed stock icon; close enough for a scheduler
                spec.title.as_str(),
                spec.body.as_str(),
                Vec::<&str>::new(), // no actions — the socket is the control surface
                HashMap::<&str, Value<'_>>::new(),
                -1i32,         // server-default expiry
            ),
        )
        .await
        .context("Notify call failed")?;
    let id: u32 = reply.body().deserialize().context("Notify reply")?;
    let server = reply
        .header()
        .sender()
        .context("Notify reply without a sender")?
        .to_string();
    Ok(DeliveryReceipt { server, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_prefers_env_then_runtime_dir() {
        // Only the pure logic is testable without a desktop; the env-var
        // branch short-circuits before any filesystem checks.
        // SAFETY: test-local var.
        unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/tmp/test-bus") };
        assert_eq!(
            session_bus_address().as_deref(),
            Some("unix:path=/tmp/test-bus")
        );
        unsafe { std::env::remove_var("DBUS_SESSION_BUS_ADDRESS") };
    }

    /// A deleted queue row will never call attempt(key) again. Maintenance
    /// must release its transport state even when no later call times out.
    #[tokio::test]
    async fn delivery_pass_reaps_transport_calls_for_pruned_rows() -> Result<()> {
        use crate::daemon::{DeliveryLedger, deliver_pending};
        use crate::store::Store;
        use jiff::Timestamp;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Stopped(Arc<AtomicBool>);
        impl Drop for Stopped {
            fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
        }
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db")).await?;
        let notifier = DesktopNotifier::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let (ready, arrived) = tokio::sync::oneshot::channel();
        let flag = Arc::clone(&stopped);
        let call = tokio::spawn(async move {
            let _guard = Stopped(flag);
            let _ = ready.send(());
            std::future::pending::<Result<crate::model::DeliveryReceipt>>().await
        });
        arrived.await?;
        notifier.overdue().insert(123, super::Overdue { since: std::time::Instant::now(), call });
        deliver_pending(&store, &notifier, &mut DeliveryLedger::default(), &Timestamp::now()).await?;
        assert!(notifier.overdue().is_empty(), "GC-deleted rows must release transport handles without another timeout");
        tokio::task::yield_now().await;
        assert!(stopped.load(Ordering::SeqCst), "pending transport task must be aborted");
        Ok(())
    }

}
