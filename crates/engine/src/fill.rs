//! Fills, as of a Windows placeholder (ADR 0042): a file's content fetched from peers into
//! memory and handed over in file order, without placing the file or reading anything from the
//! block store or the folder.
//!
//! The content is fetched a window at a time, and the next window is under way while the
//! current one is finished and handed over, so the download does not stand still at the end of
//! each window waiting for its slowest chunk (ADR 0032). At most two windows are held.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use shoal_fetch::{fetch, ChunkSource, Config, FetchError};
use shoal_proto::{ChunkHash, ChunkRef, ContentHash, DeviceId, Manifest};
use tokio::task::JoinHandle;

/// About how much of a fill one window holds.
pub(crate) const FILL_WINDOW: u64 = 16 << 20;

/// Checks a chunk as it arrives: Err if it is not what was asked for.
pub(crate) type Check = Arc<dyn Fn(&ChunkHash, &[u8]) -> Result<(), String> + Send + Sync>;

/// Why a fill stopped.
#[derive(Debug)]
pub(crate) enum FillError {
    /// The chunks could not be fetched.
    Fetch(FetchError),
    /// Handing one over failed.
    Deliver(String),
}

/// `chunks` in consecutive runs of about [`FILL_WINDOW`] bytes; a larger chunk alone.
fn windows(chunks: &[ChunkRef]) -> Vec<Vec<ChunkRef>> {
    let mut windows = Vec::new();
    let mut rest = chunks;
    while !rest.is_empty() {
        let mut take = 0;
        let mut bytes = 0u64;
        while take < rest.len() && (take == 0 || bytes + u64::from(rest[take].len) <= FILL_WINDOW) {
            bytes += u64::from(rest[take].len);
            take += 1;
        }
        let (window, after) = rest.split_at(take);
        windows.push(window.to_vec());
        rest = after;
    }
    windows
}

/// A window being fetched, stopped if dropped, as when the fill is.
struct Window(JoinHandle<Result<HashMap<ChunkHash, Vec<u8>>, FetchError>>);

impl Drop for Window {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start<S: ChunkSource<Peer = DeviceId>>(
    source: &Arc<S>,
    content: ContentHash,
    window: Vec<ChunkRef>,
    peers: &[DeviceId],
    check: &Check,
) -> Window {
    let (source, peers, check) = (source.clone(), peers.to_vec(), check.clone());
    Window(tokio::spawn(async move {
        let part = Manifest {
            content,
            chunks: window,
        };
        let mut got = HashMap::new();
        fetch(
            source,
            &part,
            &peers,
            |_| false,
            Config::default(),
            |hash, data| {
                check(&hash, &data)?;
                got.insert(hash, data);
                Ok(())
            },
        )
        .await?;
        Ok(got)
    }))
}

/// Fetches `chunks` of `content` from `peers` through `source`, checking each with `check`,
/// and gives each to `deliver` in order, with its data. The next window is fetched while one is
/// handed over.
pub(crate) async fn fill<S: ChunkSource<Peer = DeviceId>>(
    source: Arc<S>,
    content: ContentHash,
    chunks: &[ChunkRef],
    peers: &[DeviceId],
    check: Check,
    mut deliver: impl FnMut(&ChunkRef, &[u8]) -> Result<(), String>,
) -> Result<(), FillError> {
    let mut pending: VecDeque<Vec<ChunkRef>> = windows(chunks).into();
    let mut fetching: VecDeque<(Vec<ChunkRef>, Window)> = VecDeque::new();
    loop {
        // The current window and the next one under way.
        while fetching.len() < 2 {
            let Some(window) = pending.pop_front() else {
                break;
            };
            let started = start(&source, content, window.clone(), peers, &check);
            fetching.push_back((window, started));
        }
        let Some((window, mut current)) = fetching.pop_front() else {
            return Ok(());
        };
        let got = match (&mut current.0).await {
            Ok(got) => got.map_err(FillError::Fetch)?,
            Err(e) => return Err(FillError::Deliver(e.to_string())),
        };
        for chunk in &window {
            let data = got
                .get(&chunk.hash)
                .ok_or_else(|| FillError::Deliver("a chunk did not arrive".to_owned()))?;
            deliver(chunk, data).map_err(FillError::Deliver)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: u8, len: u32) -> ChunkRef {
        ChunkRef {
            hash: ChunkHash([n; 32]),
            len,
        }
    }

    #[test]
    fn windows_hold_about_their_size() {
        let mib = 1 << 20;
        let chunks: Vec<ChunkRef> = (0..40).map(|n| chunk(n, mib)).collect();
        let windows = windows(&chunks);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].len(), 16);
        assert_eq!(windows[2].len(), 8);
        // A chunk larger than a window goes alone.
        assert_eq!(super::windows(&[chunk(1, 64 * mib), chunk(2, 1)]).len(), 2);
    }
}
