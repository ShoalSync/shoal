//! This device's time. A simulation skews or jumps it for one engine of several in a process
//! (ADR 0036); otherwise it is the system's.

use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug, Default)]
pub(crate) struct Clock {
    offset_ms: AtomicI64,
}

impl Clock {
    /// Milliseconds since the Unix epoch, as this device tells it.
    pub(crate) fn now_ms(&self) -> i64 {
        crate::now_ms().saturating_add(self.offset_ms.load(Ordering::Relaxed))
    }

    /// Makes this device's time `offset_ms` ahead of the system's, or behind if negative.
    pub(crate) fn set_offset(&self, offset_ms: i64) {
        self.offset_ms.store(offset_ms, Ordering::Relaxed);
    }
}
