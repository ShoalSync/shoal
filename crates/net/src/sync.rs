//! `tidefell/sync/1` sessions. See `specs/sync-v1.md`.
//!
//! The dialing side opens the main stream and sends `Hello` first. After both `Hello`s, each
//! side requests the other's index for every shared folder, answers requests, and pushes new
//! local versions as they appear. Manifest requests use one extra stream each.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use crate::transport::{Conn, RecvHalf, SendHalf};
use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::sync::{ErrorCode, SyncMessage};
use tidefell_proto::{ContentHash, DeviceId, FolderId, Manifest, Signed};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::backend::Backend;
use crate::limits::RateLimiter;
use crate::locks::{Locked, ReadWrite};
use crate::{frame_io, Error, Result};

/// Most versions sent in one `IndexBatch` (ADR 0024).
const BATCH: usize = 512;

/// Most received-version hashes a session remembers to avoid echoing them.
const MAX_RECEIVED: usize = 1 << 18;

/// Checks a peer's certificate against the authenticated connection.
pub(crate) fn verify_peer(
    certificate: &Signed<DeviceCertificate>,
    conn: &Conn,
) -> Result<DeviceCertificate> {
    let certificate = certificate
        .verify_certificate()
        .map_err(|_| Error::Auth("invalid device certificate"))?;
    if certificate.device != conn.remote() {
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
    send: SendHalf,
    recv: RecvHalf,
}

/// Most revocations read from one `Hello`: a user revokes few devices, and a peer cannot make
/// this device keep an unbounded list.
const MAX_REVOCATIONS_PER_HELLO: usize = 256;

/// Passes on the revocations in a peer's `Hello` whose signatures hold (ADR 0040).
fn learn_revocations(backend: &dyn Backend, revocations: Option<Vec<Signed<DeviceRevocation>>>) {
    let verified: Vec<_> = revocations
        .unwrap_or_default()
        .into_iter()
        .take(MAX_REVOCATIONS_PER_HELLO)
        .filter(|r| r.verify_revocation().is_ok())
        .collect();
    if !verified.is_empty() {
        backend.learn_revocations(verified);
    }
}

/// Refuses a session with a revoked peer, and any session once this device is revoked
/// (ADR 0040).
fn check_revoked(backend: &dyn Backend, peer: &DeviceCertificate) -> Result<()> {
    if backend.is_revoked(peer) {
        return Err(Error::Auth("the peer's device is revoked"));
    }
    let own = backend.certificate();
    if own
        .verify_certificate()
        .is_ok_and(|own| backend.is_revoked(&own))
    {
        return Err(Error::Auth("this device is revoked"));
    }
    Ok(())
}

/// Performs the `Hello` exchange on the main stream.
async fn handshake(conn: &Conn, backend: &dyn Backend, initiator: bool) -> Result<Handshake> {
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
                clock_ms: Some(backend.now_ms()),
                revocations: Some(backend.revocations()),
            },
        )
        .await?;
        let SyncMessage::Hello {
            certificate,
            folders,
            clock_ms,
            revocations,
        } = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        let peer = verify_peer(&certificate, conn)?;
        learn_revocations(backend, revocations);
        check_revoked(backend, &peer)?;
        backend.learn_certificate(certificate);
        (folders, peer, offered, clock_ms)
    } else {
        let SyncMessage::Hello {
            certificate,
            folders,
            clock_ms,
            revocations,
        } = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        let peer = verify_peer(&certificate, conn)?;
        learn_revocations(backend, revocations);
        // A revoked peer is still told what this side knows, so it learns that it is revoked,
        // and is offered nothing.
        let refused = check_revoked(backend, &peer);
        let offered = if refused.is_ok() {
            backend.learn_certificate(certificate);
            offer_for(backend, &peer)
        } else {
            BTreeSet::new()
        };
        frame_io::write(
            &mut send,
            &SyncMessage::Hello {
                certificate: backend.certificate(),
                folders: offered.iter().copied().collect(),
                clock_ms: Some(backend.now_ms()),
                revocations: Some(backend.revocations()),
            },
        )
        .await?;
        if refused.is_err() {
            let _ = send.finish();
        }
        refused?;
        (folders, peer, offered, clock_ms)
    };
    // Measured when the peer's Hello arrived; the time it took on the way is small next to the
    // two minutes that matter.
    // The peer's clock is its to set: saturate rather than overflow on nonsense, and keep the
    // offset's magnitude representable.
    let clock_offset_ms =
        clock.map(|theirs| theirs.saturating_sub(backend.now_ms()).max(-i64::MAX));

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
    /// Folders whose versions from this peer were held this session, for want of their authors'
    /// certificates: the peer is not told it is caught up with them, so it sends them again next
    /// time (ADR 0024).
    held: BTreeSet<FolderId>,
    /// The revocations this peer has been told of (ADR 0040).
    revocations_told: Vec<Signed<DeviceRevocation>>,
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
    conn: Conn,
    backend: Arc<dyn Backend>,
    limiter: Arc<RateLimiter>,
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

    tokio::spawn(serve_requests(
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
        held: BTreeSet::new(),
        revocations_told: backend.revocations(),
    };
    session.request_index(backend.as_ref(), initial)?;
    // Subscribe before re-checking the offer: a folder added after the handshake computed it but
    // before this point would otherwise never be offered.
    let mut changes = backend.changes();
    reoffer(&mut session, backend.as_ref())?;
    // The peer is asked about the authors of versions stored here that this side cannot vouch
    // for yet: from 0.20, versions were applied before their authors were vouched for.
    tokio::spawn({
        let (conn, limiter, backend) = (conn.clone(), limiter.clone(), backend.clone());
        let shared: Vec<FolderId> = session.peer.shared.read_locked().iter().copied().collect();
        async move {
            for folder in shared {
                let authors = backend.stored_unknown_authors(&folder);
                if !authors.is_empty() {
                    fetch_certificates(&conn, &limiter, &backend, folder, authors).await;
                }
            }
        }
    });
    let result = loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break Ok(()) };
                let message = match message {
                    Ok(message) => message,
                    Err(e) => break Err(e),
                };
                // The certificates of authors this side does not know come before their
                // versions, from the peer that sent them (ADR 0024).
                if let SyncMessage::IndexBatch { folder, versions, .. } = &message {
                    let unknown = backend.unknown_authors(versions);
                    if session.peer.shares(folder)
                        && !unknown.is_empty()
                        && !fetch_certificates(&conn, &limiter, &backend, *folder, unknown).await
                    {
                        session.held.insert(*folder);
                    }
                }
                if let Err(e) = handle(message, &conn, &mut session, backend.as_ref()) {
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
    if matches!(result, Err(Error::Auth(_))) {
        // A peer refused for a revocation has been sent it: let the writer send it, and give
        // the peer a moment to read it and close its side, rather than cut it off unread.
        drop(session);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut writer).await;
        for _ in 0..30 {
            if conn.is_closed() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    writer.abort();
    conn.close(0, b"done");
    result
}

async fn read_loop(mut recv: RecvHalf, tx: mpsc::Sender<Result<SyncMessage>>) {
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
    mut send: SendHalf,
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
    // Everything queued is written: the peer reads it to the end.
    let _ = send.finish();
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
    send: &mut SendHalf,
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
    if let Err(e) = check_revoked(backend, &session.peer.certificate) {
        // Told why before the session ends: the Hello carries the revocations, and offers
        // nothing.
        let _ = session.send(Outgoing::Message(SyncMessage::Hello {
            certificate: backend.certificate(),
            folders: Vec::new(),
            clock_ms: Some(backend.now_ms()),
            revocations: Some(backend.revocations()),
        }));
        return Err(e);
    }
    let offer = offer_for(backend, &session.peer.certificate);
    let revocations = backend.revocations();
    // A revocation can be new, or replace one of the same device (ADR 0040).
    let told = revocations == session.revocations_told;
    if offer == session.offered && told {
        return Ok(());
    }
    session.revocations_told = revocations.clone();
    session.send(Outgoing::Message(SyncMessage::Hello {
        certificate: backend.certificate(),
        folders: offer.iter().copied().collect(),
        clock_ms: Some(backend.now_ms()),
        revocations: Some(revocations),
    }))?;
    if !told {
        // Withdrawals wait for a whole index from a peer since the revocation was learnt
        // (ADR 0040): asked for now, rather than at the folder's next change.
        let shared: Vec<FolderId> = session.peer.shared.read_locked().iter().copied().collect();
        session.request_index(backend, shared)?;
    }
    if offer == session.offered {
        return Ok(());
    }
    session.offered = offer;
    let added = session.reshare();
    debug!(device = %session.peer.certificate.device, newly_shared = added.len(), "offer updated");
    session.request_index(backend, added)
}

fn handle(
    message: SyncMessage,
    conn: &Conn,
    session: &mut Session,
    backend: &dyn Backend,
) -> Result<()> {
    let device = session.peer.certificate.device;
    match message {
        SyncMessage::Hello {
            certificate,
            folders,
            revocations,
            ..
        } => {
            // An updated offer. The certificate must still be for this connection's device.
            let certificate = verify_peer(&certificate, conn)?;
            learn_revocations(backend, revocations);
            check_revoked(backend, &certificate)?;
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
            if last && !session.held.contains(&folder) {
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
        SyncMessage::CertificateRequest { .. } => "CertificateRequest",
        SyncMessage::CertificateResponse { .. } => "CertificateResponse",
    }
}

/// Answers manifest and certificate requests, one per extra stream.
async fn serve_requests(conn: Conn, backend: Arc<dyn Backend>, shared: Shared) {
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
                Ok(SyncMessage::CertificateRequest { folder, device }) => {
                    if !shared.read_locked().contains(&folder) {
                        SyncMessage::Error {
                            code: ErrorCode::NotAMember,
                        }
                    } else {
                        match backend.certificate_of(&folder, device) {
                            Some(certificate) => SyncMessage::CertificateResponse { certificate },
                            None => SyncMessage::Error {
                                code: ErrorCode::UnknownDevice,
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

/// Most certificate requests in flight at once for one batch.
const CERTIFICATES_AT_ONCE: usize = 8;

/// Asks the peer for the certificates of `authors` and keeps those it sends. Returns false if
/// any went unanswered (the peer did not reply in time, or the connection failed), so their
/// versions are held and sent again; a peer answering that it cannot vouch for an author is
/// final, and that author's versions are refused (ADR 0024).
async fn fetch_certificates(
    conn: &Conn,
    limiter: &Arc<RateLimiter>,
    backend: &Arc<dyn Backend>,
    folder: FolderId,
    authors: Vec<DeviceId>,
) -> bool {
    let mut answered = true;
    for chunk in authors.chunks(CERTIFICATES_AT_ONCE) {
        let mut requests = tokio::task::JoinSet::new();
        for author in chunk.iter().copied() {
            let (conn, limiter) = (conn.clone(), limiter.clone());
            requests.spawn(async move {
                (
                    author,
                    request_certificate(&conn, &limiter, folder, author).await,
                )
            });
        }
        while let Some(joined) = requests.join_next().await {
            match joined {
                Ok((_, Ok(certificate))) => backend.learn_certificate(certificate),
                // Only these say the peer knows and cannot vouch: anything else, such as a
                // peer from before certificate streams, may vouch later.
                Ok((author, Err(Error::Rejected(code))))
                    if code == "UnknownDevice" || code == "NotAMember" =>
                {
                    debug!(%author, %code, "the peer cannot vouch for an author");
                    backend.cannot_vouch(&folder, author);
                }
                Ok((author, Err(e))) => {
                    debug!(%author, error = %e, "certificate request failed; holding its versions");
                    answered = false;
                }
                Err(_) => answered = false,
            }
        }
    }
    answered
}

/// Requests a manifest from a connected peer on a new stream.
pub(crate) async fn request_manifest(
    conn: &Conn,
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
    conn: &Conn,
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

/// Requests the certificate of `device`, an author in `folder`, from a connected peer on a new
/// stream (ADR 0024). The certificate is verified and must be for `device`.
pub(crate) async fn request_certificate(
    conn: &Conn,
    limiter: &RateLimiter,
    folder: FolderId,
    device: DeviceId,
) -> Result<Signed<DeviceCertificate>> {
    limiter.request(crate::traffic::Request::Certificate).await;
    crate::within(
        crate::REQUEST_TIMEOUT,
        certificate_exchange(conn, folder, device),
    )
    .await
}

async fn certificate_exchange(
    conn: &Conn,
    folder: FolderId,
    device: DeviceId,
) -> Result<Signed<DeviceCertificate>> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Stream(e.to_string()))?;
    frame_io::write(
        &mut send,
        &SyncMessage::CertificateRequest { folder, device },
    )
    .await?;
    let _ = send.finish();
    match frame_io::expect(&mut recv).await? {
        SyncMessage::CertificateResponse { certificate } => {
            let body = certificate
                .verify_certificate()
                .map_err(|_| Error::Auth("invalid device certificate"))?;
            if body.device != device {
                return Err(Error::Protocol("certificate for another device".into()));
            }
            Ok(certificate)
        }
        SyncMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        other => Err(Error::Protocol(format!(
            "unexpected certificate response: {}",
            variant_name(&other)
        ))),
    }
}

/// The device ID of a peer's endpoint.
pub(crate) fn device_of(conn: &Conn) -> DeviceId {
    conn.remote()
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
