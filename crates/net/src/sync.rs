//! `tidefell/sync/1` sessions. See `specs/sync-v1.md`.
//!
//! The dialing side opens the main stream and sends `Hello` first. After both `Hello`s, each
//! side requests the other's index for every shared folder, answers requests, and pushes new
//! local versions as they appear. Manifest requests use one extra stream each.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::{ErrorCode, SyncMessage};
use tidefell_proto::{ContentHash, DeviceId, FolderId, Manifest, Signed};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::backend::Backend;
use crate::limits::RateLimiter;
use crate::locks::{Locked, ReadWrite};
use crate::{frame_io, now_ms, Error, Result};

/// Most versions sent in one `IndexBatch` (ADR 0024).
const BATCH: usize = 512;

/// Most received-version hashes a session remembers to avoid echoing them.
const MAX_RECEIVED: usize = 1 << 18;

/// Checks a peer's certificate against the authenticated connection.
pub(crate) fn verify_peer(
    certificate: &Signed<DeviceCertificate>,
    conn: &Connection,
) -> Result<DeviceCertificate> {
    let certificate = certificate
        .verify_certificate()
        .map_err(|_| Error::Auth("invalid device certificate"))?;
    if *certificate.device.as_bytes() != *conn.remote_id().as_bytes() {
        return Err(Error::Auth("certificate is for another device"));
    }
    Ok(certificate)
}

/// The folders currently shared with a peer. Changes when either side re-sends `Hello`.
pub(crate) type Shared = Arc<RwLock<BTreeSet<FolderId>>>;

/// A verified peer and the folders shared with it.
#[derive(Debug, Clone)]
pub(crate) struct Peer {
    pub(crate) certificate: DeviceCertificate,
    pub(crate) shared: Shared,
    /// How far the peer's clock was ahead of ours at the handshake, if its `Hello` said
    /// (ADR 0030).
    pub(crate) clock_offset_ms: Option<i64>,
}

impl Peer {
    pub(crate) fn shares(&self, folder: &FolderId) -> bool {
        self.shared.read_locked().contains(folder)
    }
}

/// Folders this device offers to `peer`: those it syncs and the peer is a member of.
fn offer_for(backend: &dyn Backend, peer: &DeviceCertificate) -> BTreeSet<FolderId> {
    backend
        .folders()
        .into_iter()
        .filter(|f| backend.is_member(f, peer))
        .collect()
}

struct Handshake {
    peer: Peer,
    theirs: BTreeSet<FolderId>,
    /// The folders our `Hello` named: what the peer believes we offer.
    offered: BTreeSet<FolderId>,
    send: SendStream,
    recv: RecvStream,
}

/// Performs the `Hello` exchange on the main stream.
async fn handshake(conn: &Connection, backend: &dyn Backend, initiator: bool) -> Result<Handshake> {
    let (mut send, mut recv) = if initiator {
        conn.open_bi().await
    } else {
        conn.accept_bi().await
    }
    .map_err(|e| Error::Stream(e.to_string()))?;

    let (theirs, peer, offered, clock) = if initiator {
        // The dialer offers everything before it knows the peer, then narrows its offer with a
        // second `Hello` as soon as the session starts (see `run`).
        let offered: BTreeSet<FolderId> = backend.folders().into_iter().collect();
        frame_io::write(
            &mut send,
            &SyncMessage::Hello {
                certificate: backend.certificate(),
                folders: offered.iter().copied().collect(),
                clock_ms: Some(now_ms()),
            },
        )
        .await?;
        let SyncMessage::Hello {
            certificate,
            folders,
            clock_ms,
        } = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        (folders, verify_peer(&certificate, conn)?, offered, clock_ms)
    } else {
        let SyncMessage::Hello {
            certificate,
            folders,
            clock_ms,
        } = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        let peer = verify_peer(&certificate, conn)?;
        let offered = offer_for(backend, &peer);
        frame_io::write(
            &mut send,
            &SyncMessage::Hello {
                certificate: backend.certificate(),
                folders: offered.iter().copied().collect(),
                clock_ms: Some(now_ms()),
            },
        )
        .await?;
        (folders, peer, offered, clock_ms)
    };
    // Measured when the peer's Hello arrived; the time it took on the way is small next to the
    // two minutes that matter.
    // The peer's clock is its to set: saturate rather than overflow on nonsense, and keep the
    // offset's magnitude representable.
    let clock_offset_ms = clock.map(|theirs| theirs.saturating_sub(now_ms()).max(-i64::MAX));

    // Share what both sides named, and only folders the peer may sync now. Folders added or
    // removed since our `Hello` are announced by `reoffer` once the session runs.
    let theirs: BTreeSet<FolderId> = theirs.into_iter().collect();
    let allowed = offer_for(backend, &peer);
    let shared = theirs
        .iter()
        .filter(|f| offered.contains(f) && allowed.contains(f))
        .copied()
        .collect();
    Ok(Handshake {
        peer: Peer {
            certificate: peer,
            shared: Arc::new(RwLock::new(shared)),
            clock_offset_ms,
        },
        theirs,
        offered,
        send,
        recv,
    })
}

/// Per-session state the reading side keeps after the handshake.
struct Session {
    peer: Peer,
    /// The peer's latest offer.
    theirs: BTreeSet<FolderId>,
    /// Our latest offer.
    offered: BTreeSet<FolderId>,
    /// Work for the writer task.
    out: mpsc::UnboundedSender<Outgoing>,
    /// Versions received from this peer, not to be pushed back to it.
    received: Received,
}

impl Session {
    /// Recomputes the shared set. Returns folders that became shared.
    fn reshare(&mut self) -> Vec<FolderId> {
        let now: BTreeSet<FolderId> = self.theirs.intersection(&self.offered).copied().collect();
        let mut shared = self.peer.shared.write_locked();
        let added = now.difference(&shared).copied().collect();
        *shared = now;
        added
    }

    fn send(&self, outgoing: Outgoing) -> Result<()> {
        self.out.send(outgoing).map_err(|_| Error::Closed)
    }

    fn request_index(
        &self,
        backend: &dyn Backend,
        folders: impl IntoIterator<Item = FolderId>,
    ) -> Result<()> {
        let device = self.peer.certificate.device;
        for folder in folders {
            let (epoch, since) = backend.peer_progress(&folder, device);
            self.send(Outgoing::Message(SyncMessage::IndexRequest {
                folder,
                epoch,
                since,
            }))?;
        }
        Ok(())
    }
}

/// Work for a session's writer task. Only the writer writes to the main stream, so the reading
/// side never waits on the network (ADR 0024).
enum Outgoing {
    Message(SyncMessage),
    /// Answer an index request, in pages.
    Index {
        folder: FolderId,
        epoch: u64,
        since: u64,
    },
    /// Send the peer new versions of every folder it has been sent the index of.
    Push,
}

/// Hashes of the signed versions a peer sent in this session. The writer skips them when
/// pushing, so versions are not echoed back to where they came from (ADR 0024).
type Received = Arc<std::sync::Mutex<std::collections::HashSet<[u8; 32]>>>;

#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which no message has"
)]
fn version_hash(version: &Signed<tidefell_proto::sync::EntryVersion>) -> [u8; 32] {
    *blake3::hash(&postcard::to_stdvec(version).expect("in-memory encoding cannot fail")).as_bytes()
}

/// Runs a session until the peer disconnects. `on_ready` is called once the handshake
/// succeeds.
pub(crate) async fn run(
    conn: Connection,
    backend: Arc<dyn Backend>,
    initiator: bool,
    on_ready: impl FnOnce(Peer),
) -> Result<()> {
    let Handshake {
        peer,
        theirs,
        offered,
        send,
        recv,
    } = crate::within(
        crate::HANDSHAKE_TIMEOUT,
        handshake(&conn, backend.as_ref(), initiator),
    )
    .await?;
    let device = peer.certificate.device;
    let initial: Vec<FolderId> = peer.shared.read_locked().iter().copied().collect();
    debug!(%device, shared = initial.len(), "sync session started");
    on_ready(peer.clone());

    tokio::spawn(serve_manifests(
        conn.clone(),
        backend.clone(),
        peer.shared.clone(),
    ));

    // Reading frames is not cancel-safe, so a dedicated task feeds them through a channel.
    let (tx, mut incoming) = mpsc::channel::<Result<SyncMessage>>(16);
    tokio::spawn(read_loop(recv, tx));
    let (out, queue) = mpsc::unbounded_channel();
    let received = Received::default();
    let mut writer = tokio::spawn(write_loop(
        send,
        backend.clone(),
        peer.clone(),
        queue,
        received.clone(),
    ));

    let mut session = Session {
        peer,
        theirs,
        offered,
        out,
        received,
    };
    session.request_index(backend.as_ref(), initial)?;
    // Subscribe before re-checking the offer: a folder added after the handshake computed it but
    // before this point would otherwise never be offered.
    let mut changes = backend.changes();
    reoffer(&mut session, backend.as_ref())?;
    let result = loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break Ok(()) };
                if let Err(e) = message.and_then(|m| handle(m, &conn, &mut session, backend.as_ref())) {
                    break Err(e);
                }
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                if let Err(e) = reoffer(&mut session, backend.as_ref())
                    .and_then(|()| session.send(Outgoing::Push))
                {
                    break Err(e);
                }
            }
            written = &mut writer => {
                break match written {
                    Ok(result) => result,
                    Err(e) => Err(Error::Stream(e.to_string())),
                };
            }
        }
    };
    writer.abort();
    conn.close(0u32.into(), b"done");
    result
}

async fn read_loop(mut recv: RecvStream, tx: mpsc::Sender<Result<SyncMessage>>) {
    loop {
        let message = match frame_io::read(&mut recv).await {
            Ok(Some(message)) => Ok(message),
            Ok(None) => return,
            Err(e) => Err(e),
        };
        let failed = message.is_err();
        if tx.send(message).await.is_err() || failed {
            return;
        }
    }
}

/// Owns the main stream's sending side and does the session's writing.
async fn write_loop(
    mut send: SendStream,
    backend: Arc<dyn Backend>,
    peer: Peer,
    mut queue: mpsc::UnboundedReceiver<Outgoing>,
    received: Received,
) -> Result<()> {
    // What the peer has been sent, per folder: (epoch, up_to).
    let mut sent: HashMap<FolderId, (u64, u64)> = HashMap::new();
    // Work taken off the queue while coalescing pushes, to do before anything newer.
    let mut backlog = std::collections::VecDeque::new();
    loop {
        let outgoing = match backlog.pop_front() {
            Some(outgoing) => outgoing,
            None => match queue.recv().await {
                Some(outgoing) => outgoing,
                None => break,
            },
        };
        match outgoing {
            Outgoing::Message(message) => frame_io::write(&mut send, &message).await?,
            Outgoing::Index {
                folder,
                epoch,
                since,
            } => {
                if !peer.shares(&folder) {
                    continue;
                }
                let Some(progress) = send_index(
                    &mut send,
                    backend.as_ref(),
                    &received,
                    folder,
                    epoch,
                    since,
                    false,
                )
                .await?
                else {
                    frame_io::write(
                        &mut send,
                        &SyncMessage::Error {
                            code: ErrorCode::UnknownFolder,
                        },
                    )
                    .await?;
                    continue;
                };
                sent.insert(folder, progress);
            }
            Outgoing::Push => {
                // Pushes queued behind this one would find nothing new.
                drain_pushes(&mut queue, &mut backlog);
                sent.retain(|folder, _| peer.shares(folder));
                let folders: Vec<(FolderId, (u64, u64))> =
                    sent.iter().map(|(f, p)| (*f, *p)).collect();
                for (folder, (epoch, up_to)) in folders {
                    if let Some(progress) = send_index(
                        &mut send,
                        backend.as_ref(),
                        &received,
                        folder,
                        epoch,
                        up_to,
                        true,
                    )
                    .await?
                    {
                        sent.insert(folder, progress);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Takes everything already queued, dropping pushes and keeping the rest, in order, in
/// `backlog`.
fn drain_pushes(
    queue: &mut mpsc::UnboundedReceiver<Outgoing>,
    backlog: &mut std::collections::VecDeque<Outgoing>,
) {
    while let Ok(next) = queue.try_recv() {
        if !matches!(next, Outgoing::Push) {
            backlog.push_back(next);
        }
    }
}

/// Sends `folder`'s index after `since` in pages of [`BATCH`], skipping versions the peer sent
/// us. Returns the `(epoch, up_to)` sent, or `None` if the folder is unknown. A push with
/// nothing new sends nothing.
async fn send_index(
    send: &mut SendStream,
    backend: &dyn Backend,
    received: &Received,
    folder: FolderId,
    epoch: u64,
    since: u64,
    push: bool,
) -> Result<Option<(u64, u64)>> {
    let (mut epoch_asked, mut since) = (epoch, since);
    let mut first = true;
    loop {
        let Some(page) = backend.index_since(&folder, epoch_asked, since, BATCH) else {
            return Ok(None);
        };
        let last = page.versions.len() < BATCH;
        let unchanged = page.epoch == epoch && page.up_to == since;
        if push && first && last && page.versions.is_empty() && unchanged {
            return Ok(Some((page.epoch, page.up_to)));
        }
        first = false;
        let versions: Vec<_> = {
            let mut received = received.locked();
            page.versions
                .into_iter()
                .filter(|v| !received.remove(&version_hash(v)))
                .collect()
        };
        // Pages in the middle only matter for their versions; the last one also carries the
        // position the peer records.
        if last || !versions.is_empty() {
            frame_io::write(
                send,
                &SyncMessage::IndexBatch {
                    folder,
                    epoch: page.epoch,
                    up_to: page.up_to,
                    versions,
                    last,
                },
            )
            .await?;
        }
        if last {
            return Ok(Some((page.epoch, page.up_to)));
        }
        (epoch_asked, since) = (page.epoch, page.up_to);
    }
}

/// Re-sends `Hello` if the folders we offer the peer changed (a folder was added or removed,
/// or membership changed).
fn reoffer(session: &mut Session, backend: &dyn Backend) -> Result<()> {
    let offer = offer_for(backend, &session.peer.certificate);
    if offer == session.offered {
        return Ok(());
    }
    session.send(Outgoing::Message(SyncMessage::Hello {
        certificate: backend.certificate(),
        folders: offer.iter().copied().collect(),
        clock_ms: Some(now_ms()),
    }))?;
    session.offered = offer;
    let added = session.reshare();
    debug!(device = %session.peer.certificate.device, newly_shared = added.len(), "offer updated");
    session.request_index(backend, added)
}

fn handle(
    message: SyncMessage,
    conn: &Connection,
    session: &mut Session,
    backend: &dyn Backend,
) -> Result<()> {
    let device = session.peer.certificate.device;
    match message {
        SyncMessage::Hello {
            certificate,
            folders,
            ..
        } => {
            // An updated offer. The certificate must still be for this connection's device.
            let certificate = verify_peer(&certificate, conn)?;
            if certificate.user != session.peer.certificate.user {
                return Err(Error::Protocol(
                    "certificate changed user mid-session".into(),
                ));
            }
            session.theirs = folders.into_iter().collect();
            let added = session.reshare();
            debug!(%device, offered = session.theirs.len(), newly_shared = added.len(), "peer updated its offer");
            session.request_index(backend, added)
        }
        SyncMessage::IndexRequest {
            folder,
            epoch,
            since,
        } => {
            if !session.peer.shares(&folder) {
                debug!(%device, %folder, "index requested for a folder not shared");
                return session.send(Outgoing::Message(SyncMessage::Error {
                    code: ErrorCode::NotAMember,
                }));
            }
            debug!(%device, %folder, epoch, since, "index requested");
            session.send(Outgoing::Index {
                folder,
                epoch,
                since,
            })
        }
        SyncMessage::IndexBatch {
            folder,
            epoch,
            up_to,
            versions,
            last,
        } => {
            if !session.peer.shares(&folder) {
                // Possible briefly after the folder stopped being shared; ignore it.
                debug!(%device, %folder, "ignoring index for a folder not shared");
                return Ok(());
            }
            debug!(%device, %folder, versions = versions.len(), last, "index received");
            {
                let mut received = session.received.locked();
                // Versions we already had are never pushed, so their hashes would stay; bound the
                // set. Forgetting only means some versions may be echoed.
                if received.len() > MAX_RECEIVED {
                    received.clear();
                }
                received.extend(versions.iter().map(version_hash));
            }
            backend.apply_versions(&folder, device, versions);
            if last {
                backend.set_peer_progress(&folder, device, epoch, up_to);
            }
            Ok(())
        }
        SyncMessage::Error { code } => {
            warn!(%device, ?code, "peer reported an error");
            Ok(())
        }
        other => Err(Error::Protocol(format!(
            "unexpected message on main stream: {}",
            variant_name(&other)
        ))),
    }
}

fn variant_name(message: &SyncMessage) -> &'static str {
    match message {
        SyncMessage::Hello { .. } => "Hello",
        SyncMessage::IndexRequest { .. } => "IndexRequest",
        SyncMessage::IndexBatch { .. } => "IndexBatch",
        SyncMessage::ManifestRequest { .. } => "ManifestRequest",
        SyncMessage::ManifestResponse { .. } => "ManifestResponse",
        SyncMessage::Error { .. } => "Error",
    }
}

/// Answers manifest requests, one per extra stream.
async fn serve_manifests(conn: Connection, backend: Arc<dyn Backend>, shared: Shared) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let backend = backend.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            let request = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv));
            let response = match request.await {
                Ok(SyncMessage::ManifestRequest { folder, content }) => {
                    if !shared.read_locked().contains(&folder) {
                        SyncMessage::Error {
                            code: ErrorCode::NotAMember,
                        }
                    } else {
                        match backend.manifest(&folder, &content) {
                            Some(manifest) => SyncMessage::ManifestResponse { manifest },
                            None => SyncMessage::Error {
                                code: ErrorCode::UnknownManifest,
                            },
                        }
                    }
                }
                _ => SyncMessage::Error {
                    code: ErrorCode::ProtocolViolation,
                },
            };
            if frame_io::write(&mut send, &response).await.is_ok() {
                let _ = send.finish();
            }
        });
    }
}

/// Requests a manifest from a connected peer on a new stream.
pub(crate) async fn request_manifest(
    conn: &Connection,
    limiter: &RateLimiter,
    folder: FolderId,
    content: ContentHash,
) -> Result<Manifest> {
    limiter.request(crate::traffic::Request::Manifest).await;
    crate::within(
        crate::REQUEST_TIMEOUT,
        manifest_exchange(conn, folder, content),
    )
    .await
}

async fn manifest_exchange(
    conn: &Connection,
    folder: FolderId,
    content: ContentHash,
) -> Result<Manifest> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Stream(e.to_string()))?;
    frame_io::write(&mut send, &SyncMessage::ManifestRequest { folder, content }).await?;
    let _ = send.finish();
    match frame_io::expect(&mut recv).await? {
        SyncMessage::ManifestResponse { manifest } if manifest.content == content => Ok(manifest),
        SyncMessage::ManifestResponse { .. } => {
            Err(Error::Protocol("manifest for different content".into()))
        }
        SyncMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        other => Err(Error::Protocol(format!(
            "unexpected manifest response: {}",
            variant_name(&other)
        ))),
    }
}

/// The device ID of a peer's endpoint.
pub(crate) fn device_of(conn: &Connection) -> DeviceId {
    DeviceId(*conn.remote_id().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_pushes_keeps_other_work_in_order() {
        let (tx, mut queue) = mpsc::unbounded_channel();
        let folder = FolderId([1; 32]);
        tx.send(Outgoing::Push).unwrap();
        tx.send(Outgoing::Message(SyncMessage::Error {
            code: ErrorCode::NotAMember,
        }))
        .unwrap();
        tx.send(Outgoing::Push).unwrap();
        tx.send(Outgoing::Index {
            folder,
            epoch: 3,
            since: 4,
        })
        .unwrap();
        let mut backlog = std::collections::VecDeque::new();
        drain_pushes(&mut queue, &mut backlog);
        assert!(queue.try_recv().is_err(), "everything was taken");
        assert_eq!(backlog.len(), 2, "only pushes are dropped");
        assert!(matches!(
            backlog[0],
            Outgoing::Message(SyncMessage::Error { .. })
        ));
        assert!(matches!(
            backlog[1],
            Outgoing::Index {
                epoch: 3,
                since: 4,
                ..
            }
        ));
    }
}
