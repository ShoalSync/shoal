//! Chunk requests from downloads running at once, sent to each peer together (ADR 0032).
//!
//! The first request for a peer waits a moment for others, then they go as one blobs/1 request
//! of up to 64 chunks, on one stream, and each download gets the answers for its own chunks.
//! Requests of one download are never joined: it keeps several under way at once, so it waits
//! out a round trip once per many requests rather than once per request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_net::{ChunkResult, EndpointId, Node};
use tidefell_proto::blobs::MAX_CHUNKS_PER_REQUEST;
use tidefell_proto::{ChunkHash, DeviceId, FolderId};
use tokio::sync::oneshot;

/// How long the first request for a peer waits for others to join it.
const WINDOW: Duration = Duration::from_millis(2);

type Answer = std::result::Result<Vec<ChunkResult>, String>;

/// A new download's ID, telling its requests apart from other downloads'.
pub(crate) fn download_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Waiting {
    download: u64,
    chunks: Vec<ChunkHash>,
    reply: oneshot::Sender<Answer>,
}

/// Requests waiting to be sent, by peer and folder.
#[derive(Default)]
pub(crate) struct Batches {
    queues: Mutex<HashMap<(DeviceId, FolderId), Vec<Waiting>>>,
}

impl Batches {
    /// Requests `chunks` of `folder` from `peer`, together with other downloads' requests to it.
    /// Answers as [`Node::fetch_chunks`] would for these chunks alone.
    pub(crate) async fn fetch(
        self: &Arc<Self>,
        node: &Arc<Node>,
        peer: DeviceId,
        folder: FolderId,
        download: u64,
        chunks: Vec<ChunkHash>,
    ) -> Answer {
        let (reply, answer) = oneshot::channel();
        let first = {
            let mut queues = self.queues.lock().unwrap();
            let queue = queues.entry((peer, folder)).or_default();
            queue.push(Waiting {
                download,
                chunks,
                reply,
            });
            queue.len() == 1
        };
        // The first request sends them all, in a task of its own, so a download that stops
        // waiting does not stop the others' requests.
        if first {
            tokio::spawn(self.clone().send(node.clone(), peer, folder));
        }
        answer
            .await
            .unwrap_or_else(|_| Err("the request was dropped".to_owned()))
    }

    async fn send(self: Arc<Self>, node: Arc<Node>, peer: DeviceId, folder: FolderId) {
        tokio::time::sleep(WINDOW).await;
        loop {
            let batch = {
                let mut queues = self.queues.lock().unwrap();
                let Some(queue) = queues.get_mut(&(peer, folder)) else {
                    return;
                };
                let batch = take_batch(queue);
                if queue.is_empty() {
                    queues.remove(&(peer, folder));
                }
                batch
            };
            if batch.is_empty() {
                return;
            }
            let more = self.queues.lock().unwrap().contains_key(&(peer, folder));
            if more {
                tokio::spawn(request(node.clone(), peer, folder, batch));
            } else {
                request(node, peer, folder, batch).await;
                return;
            }
        }
    }
}

/// Takes waiting requests from the front of `queue` while their distinct chunks fit in one
/// request, at most one of each download; always at least one. Those passed over keep their
/// place for the next request, sent alongside.
fn take_batch(queue: &mut Vec<Waiting>) -> Vec<Waiting> {
    let mut chunks: Vec<ChunkHash> = Vec::new();
    let mut downloads: Vec<u64> = Vec::new();
    let mut take = vec![false; queue.len()];
    for (i, waiting) in queue.iter().enumerate() {
        if downloads.contains(&waiting.download) {
            continue;
        }
        let new: Vec<ChunkHash> = waiting
            .chunks
            .iter()
            .filter(|c| !chunks.contains(c))
            .copied()
            .collect();
        if !downloads.is_empty() && chunks.len().saturating_add(new.len()) > MAX_CHUNKS_PER_REQUEST
        {
            break;
        }
        chunks.extend(new);
        downloads.push(waiting.download);
        take[i] = true;
    }
    let mut batch = Vec::new();
    let mut kept = Vec::new();
    for (waiting, take) in queue.drain(..).zip(take) {
        if take {
            batch.push(waiting);
        } else {
            kept.push(waiting);
        }
    }
    *queue = kept;
    batch
}

/// Sends one request for every chunk in `batch`, and answers each waiting download.
async fn request(node: Arc<Node>, peer: DeviceId, folder: FolderId, batch: Vec<Waiting>) {
    let mut chunks: Vec<ChunkHash> = Vec::new();
    for waiting in &batch {
        for chunk in &waiting.chunks {
            if !chunks.contains(chunk) {
                chunks.push(*chunk);
            }
        }
    }
    let results = match EndpointId::from_bytes(peer.as_bytes()) {
        Ok(endpoint) => node
            .fetch_chunks(endpoint, folder, &chunks)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    for waiting in batch {
        let answer = match &results {
            Ok(results) => Ok(answer_for(&waiting.chunks, results)),
            Err(e) => Err(e.clone()),
        };
        let _ = waiting.reply.send(answer);
    }
}

/// The answers to a request for `chunks`, from those to a request that included them: each
/// chunk sent or missing, in order, up to a busy answer, which ends them.
fn answer_for(chunks: &[ChunkHash], results: &[ChunkResult]) -> Vec<ChunkResult> {
    let busy = results.iter().find_map(|r| match r {
        ChunkResult::Busy(wait) => Some(*wait),
        _ => None,
    });
    let mut answer = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let found = results.iter().find_map(|r| match r {
            ChunkResult::Found(hash, data) if hash == chunk => {
                Some(ChunkResult::Found(*hash, data.clone()))
            }
            ChunkResult::Missing(hash) if hash == chunk => Some(ChunkResult::Missing(*hash)),
            _ => None,
        });
        match (found, busy) {
            (Some(result), _) => answer.push(result),
            // Not reached before the peer said it was busy.
            (None, Some(wait)) => {
                answer.push(ChunkResult::Busy(wait));
                break;
            }
            // Not answered at all: the peer stopped early, which counts as missing.
            (None, None) => {}
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> ChunkHash {
        ChunkHash([n; 32])
    }

    fn waiting(chunks: &[u8]) -> Waiting {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
        of(
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            chunks,
        )
    }

    fn of(download: u64, chunks: &[u8]) -> Waiting {
        Waiting {
            download,
            chunks: chunks.iter().map(|&n| h(n)).collect(),
            reply: oneshot::channel().0,
        }
    }

    #[test]
    fn batches_fill_one_request() {
        let mut queue = vec![waiting(&[1, 2]), waiting(&[2, 3]), waiting(&[4])];
        assert_eq!(take_batch(&mut queue).len(), 3);
        assert!(queue.is_empty());

        let big: Vec<u8> = (0..60).collect();
        let mut queue = vec![
            waiting(&big),
            waiting(&[100, 101, 102, 103, 104]),
            waiting(&[1]),
        ];
        // 60 + 5 is more than a request holds; the first always goes.
        assert_eq!(take_batch(&mut queue).len(), 1);
        assert_eq!(take_batch(&mut queue).len(), 2);

        let mut queue = vec![waiting(&(0..64).collect::<Vec<u8>>()), waiting(&[200])];
        assert_eq!(take_batch(&mut queue).len(), 1);
    }

    #[test]
    fn a_downloads_requests_go_on_their_own() {
        // Two of download 1's and one of download 2's: one request takes one of each, the next
        // the other of download 1's.
        let mut queue = vec![of(1, &[1]), of(1, &[2]), of(2, &[3])];
        let first = take_batch(&mut queue);
        assert_eq!(first.iter().map(|w| w.download).collect::<Vec<_>>(), [1, 2]);
        let second = take_batch(&mut queue);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].chunks, [h(2)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn each_download_gets_its_own_answers() {
        let results = vec![
            ChunkResult::Found(h(1), vec![1]),
            ChunkResult::Missing(h(2)),
            ChunkResult::Found(h(3), vec![3]),
        ];
        assert_eq!(
            answer_for(&[h(3), h(2)], &results),
            [
                ChunkResult::Found(h(3), vec![3]),
                ChunkResult::Missing(h(2))
            ]
        );
        // The peer was busy after the first chunk.
        let busy = vec![
            ChunkResult::Found(h(1), vec![1]),
            ChunkResult::Busy(Duration::from_secs(2)),
        ];
        assert_eq!(
            answer_for(&[h(1), h(4), h(5)], &busy),
            [
                ChunkResult::Found(h(1), vec![1]),
                ChunkResult::Busy(Duration::from_secs(2))
            ]
        );
        assert_eq!(
            answer_for(&[h(4)], &busy),
            [ChunkResult::Busy(Duration::from_secs(2))]
        );
    }
}
