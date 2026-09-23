//! Runs a [`Scheduler`] against a [`ChunkSource`].

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use tidefell_proto::{ChunkHash, Manifest};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::scheduler::{Config, Request, Scheduler};

/// Where chunks come from: typically blobs/1 peers.
pub trait ChunkSource: Send + Sync + 'static {
    /// Identifies a peer.
    type Peer: Clone + Eq + Hash + Ord + Debug + Send + Sync + 'static;

    /// The chunks of `manifest` that `peer` holds.
    fn availability(
        &self,
        peer: &Self::Peer,
        manifest: &Manifest,
    ) -> impl Future<Output = Result<Vec<ChunkHash>, String>> + Send;

    /// Requests `chunks` from `peer`. Returns data for each chunk the peer has; chunks absent
    /// from the result are treated as missing. Implementations must verify each chunk's hash.
    fn fetch(
        &self,
        peer: &Self::Peer,
        chunks: &[ChunkHash],
    ) -> impl Future<Output = Result<Vec<(ChunkHash, Vec<u8>)>, String>> + Send;
}

/// Why a fetch did not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// No remaining peer holds these chunks.
    #[error("{} chunk(s) unavailable from any peer", .0.len())]
    Unobtainable(Vec<ChunkHash>),
    /// The chunk sink failed.
    #[error("storing a chunk failed: {0}")]
    Sink(String),
}

/// What a completed fetch transferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport<P: Ord> {
    /// Chunks received.
    pub chunks: usize,
    /// Bytes received.
    pub bytes: u64,
    /// Chunks received from each peer.
    pub per_peer: BTreeMap<P, usize>,
}

/// Fetches every chunk of `manifest` for which `have` returns false from `peers`, passing each
/// chunk to `sink` as it arrives.
pub async fn fetch<S: ChunkSource>(
    source: Arc<S>,
    manifest: &Manifest,
    peers: &[S::Peer],
    have: impl Fn(&ChunkHash) -> bool,
    config: Config,
    mut sink: impl FnMut(ChunkHash, Vec<u8>) -> Result<(), String>,
) -> Result<FetchReport<S::Peer>, FetchError> {
    let mut scheduler = Scheduler::new(manifest, have, config);
    let mut report = FetchReport {
        chunks: 0,
        bytes: 0,
        per_peer: BTreeMap::new(),
    };
    if scheduler.is_complete() {
        return Ok(report);
    }

    let mut queries = JoinSet::new();
    for peer in peers {
        let (source, peer, manifest) = (source.clone(), peer.clone(), manifest.clone());
        queries.spawn(async move {
            let has = source.availability(&peer, &manifest).await;
            (peer, has)
        });
    }
    while let Some(joined) = queries.join_next().await {
        if let Ok((peer, Ok(has))) = joined {
            scheduler.set_availability(peer, has);
        }
    }

    type Outcome<P> = (Request<P>, Result<Vec<(ChunkHash, Vec<u8>)>, String>);
    let mut requests: JoinSet<Outcome<S::Peer>> = JoinSet::new();
    loop {
        if scheduler.is_complete() {
            requests.abort_all();
            return Ok(report);
        }
        for request in scheduler.next_requests(Instant::now().into_std()) {
            let source = source.clone();
            requests.spawn(async move {
                let result = source.fetch(&request.peer, &request.chunks).await;
                (request, result)
            });
        }

        let wakeup = scheduler
            .next_wakeup(Instant::now().into_std())
            .map(Instant::from_std);
        if requests.is_empty() {
            match wakeup {
                Some(at) => {
                    tokio::time::sleep_until(at).await;
                    continue;
                }
                None => return Err(FetchError::Unobtainable(scheduler.unobtainable())),
            }
        }

        let joined = match wakeup {
            Some(at) => tokio::select! {
                joined = requests.join_next() => joined,
                () = tokio::time::sleep_until(at) => continue,
            },
            None => requests.join_next().await,
        };
        let Some(Ok((request, result))) = joined else {
            continue;
        };
        match result {
            Ok(chunks) => {
                let mut delivered: BTreeMap<ChunkHash, Vec<u8>> = chunks.into_iter().collect();
                for hash in &request.chunks {
                    match delivered.remove(hash) {
                        Some(data) => {
                            report.chunks += 1;
                            report.bytes += data.len() as u64;
                            *report.per_peer.entry(request.peer.clone()).or_default() += 1;
                            sink(*hash, data).map_err(FetchError::Sink)?;
                            scheduler.received(&request.peer, hash);
                        }
                        None => scheduler.missing(&request.peer, hash),
                    }
                }
            }
            Err(_) => scheduler.failed(&request.peer, &request.chunks, Instant::now().into_std()),
        }
    }
}
