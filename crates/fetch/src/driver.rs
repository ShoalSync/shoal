//! Runs a [`Scheduler`] against a [`ChunkSource`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use shoal_proto::{ChunkHash, Manifest};
use tokio::task::{JoinError, JoinSet};
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

    /// True if `peer` is known to hold every chunk of every current file, as a full replica
    /// does (ADR 0032). Its chunks are then requested without asking for its availability
    /// first, unless it answers that one is missing.
    fn holds_everything(&self, peer: &Self::Peer) -> bool {
        let _ = peer;
        false
    }

    /// Requests `chunks` from `peer`. Returns data for each chunk the peer has; chunks absent
    /// from the result are treated as missing, unless the peer said it is busy. Implementations
    /// must verify each chunk's hash.
    fn fetch(
        &self,
        peer: &Self::Peer,
        chunks: &[ChunkHash],
    ) -> impl Future<Output = Result<Fetched, String>> + Send;
}

/// What a peer sent for a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The chunks it sent, verified.
    pub chunks: Vec<(ChunkHash, Vec<u8>)>,
    /// Set if the peer was at its upload limit and stopped before the rest (ADR 0029): the rest
    /// are not missing, and the peer may be asked again after this long.
    pub busy_for: Option<Duration>,
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
/// chunk to `sink` as it arrives. Peers are asked which chunks they hold first, except a peer
/// that holds everything and a lone peer, which are assumed to hold them all until one is
/// missing (ADR 0032).
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

    // Peers assumed to hold every chunk; asked for their availability once one is missing.
    let mut assumed: BTreeSet<S::Peer> = BTreeSet::new();
    let mut queries = JoinSet::new();
    for peer in peers {
        if peers.len() == 1 || source.holds_everything(peer) {
            scheduler.set_availability(peer.clone(), manifest.chunks.iter().map(|c| c.hash));
            assumed.insert(peer.clone());
            continue;
        }
        let (source, peer, manifest) = (source.clone(), peer.clone(), manifest.clone());
        queries.spawn(async move {
            let has = source.availability(&peer, &manifest).await;
            (peer, has)
        });
    }
    while let Some(joined) = queries.join_next().await {
        if let Ok((peer, Ok(has))) = rethrow(joined) {
            scheduler.set_availability(peer, has);
        }
    }

    type Outcome<P> = (Request<P>, Result<Fetched, String>);
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
        let Some(Ok((request, result))) = joined.map(rethrow) else {
            continue;
        };
        match result {
            Ok(fetched) => {
                let mut delivered: BTreeMap<ChunkHash, Vec<u8>> =
                    fetched.chunks.into_iter().collect();
                if let Some(wait) = fetched.busy_for {
                    let rest: Vec<ChunkHash> = request
                        .chunks
                        .iter()
                        .filter(|h| !delivered.contains_key(*h))
                        .copied()
                        .collect();
                    scheduler.busy(&request.peer, &rest, (Instant::now() + wait).into_std());
                }
                let mut missing = false;
                for hash in &request.chunks {
                    match delivered.remove(hash) {
                        // A hedged chunk's second copy: the first is in already.
                        Some(_) if !scheduler.wanted(hash) => {
                            scheduler.received(&request.peer, hash);
                        }
                        Some(data) => {
                            report.chunks += 1;
                            report.bytes += data.len() as u64;
                            *report.per_peer.entry(request.peer.clone()).or_default() += 1;
                            sink(*hash, data).map_err(FetchError::Sink)?;
                            scheduler.received(&request.peer, hash);
                        }
                        None if fetched.busy_for.is_some() => {}
                        None => {
                            scheduler.missing(&request.peer, hash);
                            missing = true;
                        }
                    }
                }
                // A peer assumed to hold everything does not: ask what it holds after all.
                if missing && assumed.remove(&request.peer) {
                    if let Ok(has) = source.availability(&request.peer, manifest).await {
                        // Not the ones it just said it lacks, whatever the answer says.
                        let has: Vec<ChunkHash> = has
                            .into_iter()
                            .filter(|h| !request.chunks.contains(h))
                            .collect();
                        scheduler.set_availability(request.peer.clone(), has);
                    }
                }
            }
            Err(_) => scheduler.failed(&request.peer, &request.chunks, Instant::now().into_std()),
        }
    }
}
/// Passes a panic in a spawned request on to the caller, so it is not mistaken for a failed
/// request (ADR 0030).
fn rethrow<T>(joined: Result<T, JoinError>) -> Result<T, JoinError> {
    match joined {
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        other => other,
    }
}
