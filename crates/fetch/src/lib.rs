//! Tidefell `fetch` crate: multi-peer scheduler, rarest-first, retry.
//!
//! [`Scheduler`] decides which chunks of a manifest to request from which peers. It is pure
//! (no I/O, time passed in), so its policy is tested directly. [`fetch`] drives it against any
//! [`ChunkSource`], such as the `net` crate's blobs/1 client.
//!
//! Policy:
//! - **Rarest first.** Among missing chunks not in flight, those held by the fewest available
//!   peers are requested first, so rare chunks are secured before their holders leave.
//! - **Spread load.** Each chunk goes to the holder with the fewest chunks in flight, up to a
//!   per-peer limit.
//! - **Retry with backoff.** A failed request returns its chunks to the queue and backs the
//!   peer off exponentially; after too many consecutive failures the peer is dropped.
//! - A peer that reports a chunk missing is no longer considered a holder of it.

mod driver;
mod scheduler;

pub use driver::{fetch, ChunkSource, FetchError, FetchReport};
pub use scheduler::{Config, Request, Scheduler};
