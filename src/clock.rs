//! The `Clock` trait (DESIGN.md §11): the daemon core never asks the system
//! for "now" directly. Tests drive reconciliation, catch-up, DST cases, and
//! overdue-wait handling by owning this — an architectural requirement from
//! day one, not a retrofit.

use jiff::Timestamp;

pub trait Clock: Send + Sync {
    /// The current instant in the daemon's zone. Wall-clock is the
    /// scheduling contract (§9): every "is it due?" check compares stored
    /// targets against this.
    fn now(&self) -> Timestamp;
}

/// The real thing.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}
