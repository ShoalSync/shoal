//! Taking locks. A lock is poisoned only when a thread panicked while holding it, which already
//! stops the node (ADR 0030), so the panic is passed on rather than handled at every site.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) trait Locked<T> {
    /// The lock, taken.
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Locked<T> for Mutex<T> {
    #[expect(clippy::unwrap_used, reason = "a poisoned lock passes a panic on")]
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap()
    }
}

pub(crate) trait ReadWrite<T> {
    /// The lock, taken to read.
    fn read_locked(&self) -> RwLockReadGuard<'_, T>;
    /// The lock, taken to write.
    fn write_locked(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> ReadWrite<T> for RwLock<T> {
    #[expect(clippy::unwrap_used, reason = "a poisoned lock passes a panic on")]
    fn read_locked(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap()
    }

    #[expect(clippy::unwrap_used, reason = "a poisoned lock passes a panic on")]
    fn write_locked(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap()
    }
}
