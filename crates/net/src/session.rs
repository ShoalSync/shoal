//! Sync sessions, for both `tidefell/sync/1` and `tidefell/sync/2`, which differ only in their
//! messages and in what an index holds (a [`Protocol`]). See `specs/sync-v1.md` and
//! `specs/sync-v2.md`.
//!
//! The dialing side opens the main stream and sends `Hello` first. After both `Hello`s, each
//! side requests the other's index for every shared folder, answers requests, and pushes new
//! local items as they appear. Requests other than the index, such as certificates, use one extra
//! stream each.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use serde::Serialize;
use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::{DeviceId, FolderId, Signed};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::limits::RateLimiter;
use crate::locks::{Locked, ReadWrite};
use crate::transport::{Conn, RecvHalf, SendHalf};
use crate::{frame_io, Error, Result};

/// Most items sent in one `IndexBatch` (ADR 0024).
const BATCH: usize = 512;

/// Most received-item hashes a session remembers to avoid echoing them.
const MAX_RECEIVED: usize = 1 << 18;

/// Most revocations read from one `Hello`: a user revokes few devices, and a peer cannot make
/// this device keep an unbounded list.
const MAX_REVOCATIONS_PER_HELLO: usize = 256;

/// Why a request was refused, in the words both protocols share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    NotAMember,
    UnknownFolder,
    UnknownDevice,
    ProtocolViolation,
}

/// A message, as the session sees it.
pub(crate) enum Event<P: Protocol> {
    Hello {
        certificate: Signed<DeviceCertificate>,
        folders: Vec<FolderId>,
        clock_ms: Option<i64>,
        revocations: Option<Vec<Signed<DeviceRevocation>>>,
    },
    IndexRequest {
        folder: FolderId,
        epoch: u64,
        since: u64,
    },
    IndexBatch {
        folder: FolderId,
        epoch: u64,
        up_to: u64,
        items: Vec<P::Item>,
        last: bool,
    },
    CertificateRequest {
        folder: FolderId,
        device: DeviceId,
    },
    CertificateResponse(Signed<DeviceCertificate>),
    /// A refusal, by the name of its code.
    Error(String),
    /// A message of this protocol's own, such as sync/1's manifest requests.
    Other(P::Message),
}

/// What a sync protocol version is made of: its messages and the items of its index.
pub(crate) trait Protocol: Sized + Send + Sync + 'static {
    type Message: Serialize + tidefell_proto::frame::Message + Send + Sync + 'static;
    type Item: Serialize + Clone + Send + Sync + 'static;

    fn hello(
        certificate: Signed<DeviceCertificate>,
        folders: Vec<FolderId>,
        clock_ms: i64,
        revocations: Vec<Signed<DeviceRevocation>>,
    ) -> Self::Message;
    fn index_request(folder: FolderId, epoch: u64, since: u64) -> Self::Message;
    fn index_batch(
        folder: FolderId,
        epoch: u64,
        up_to: u64,
        items: Vec<Self::Item>,
        last: bool,
    ) -> Self::Message;
    fn certificate_request(folder: FolderId, device: DeviceId) -> Self::Message;
    fn certificate_response(certificate: Signed<DeviceCertificate>) -> Self::Message;
    fn error(code: Code) -> Self::Message;
    fn event(message: Self::Message) -> Event<Self>;
    /// The message's name, for logs and errors.
    fn name(message: &Self::Message) -> &'static str;
    /// The most encoded bytes of items in one batch.
    const MAX_BATCH_BYTES: usize;
}

/// A page of a folder's index, for an `IndexBatch`.
pub(crate) struct Page<I> {
    pub(crate) epoch: u64,
    pub(crate) up_to: u64,
    pub(crate) items: Vec<I>,
}

/// What a session needs from local storage and policy, for one protocol.
pub(crate) trait Side<P: Protocol>: Send + Sync + 'static {
    fn certificate(&self) -> Signed<DeviceCertificate>;
    fn now_ms(&self) -> i64;
    /// Folders this device syncs over this protocol, offered when dialing a peer.
    fn folders(&self) -> Vec<FolderId>;
    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool;
    fn certificate_of(
        &self,
        folder: &FolderId,
        device: DeviceId,
    ) -> Option<Signed<DeviceCertificate>>;
    fn learn_certificate(&self, certificate: Signed<DeviceCertificate>);
    fn revocations(&self) -> Vec<Signed<DeviceRevocation>>;
    fn learn_revocations(&self, revocations: Vec<Signed<DeviceRevocation>>);
    fn is_revoked(&self, certificate: &DeviceCertificate) -> bool;
    fn unknown_authors(&self, items: &[P::Item]) -> Vec<DeviceId>;
    fn stored_unknown_authors(&self, folder: &FolderId) -> Vec<DeviceId>;
    /// `peer`, which shares `folder`, answered it cannot vouch for `author` (ADR 0024).
    fn cannot_vouch(&self, folder: &FolderId, author: DeviceId, peer: &DeviceCertificate);
    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<Page<P::Item>>;
    fn apply(&self, folder: &FolderId, peer: DeviceId, items: Vec<P::Item>);
    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64);
    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64);
    /// Answers a request of the protocol's own on an extra stream; `shared` holds the folders
    /// shared with the peer.
    fn answer_other(&self, _request: P::Message, _shared: &BTreeSet<FolderId>) -> P::Message {
        P::error(Code::ProtocolViolation)
    }
    fn changes(&self) -> watch::Receiver<u64>;
}

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
fn offer_for<P: Protocol>(side: &dyn Side<P>, peer: &DeviceCertificate) -> BTreeSet<FolderId> {
    side.folders()
        .into_iter()
        .filter(|f| side.is_member(f, peer))
        .collect()
}

fn hello<P: Protocol>(side: &dyn Side<P>, folders: &BTreeSet<FolderId>) -> P::Message {
    P::hello(
        side.certificate(),
        folders.iter().copied().collect(),
        side.now_ms(),
        side.revocations(),
    )
}

struct Handshake {
    peer: Peer,
    theirs: BTreeSet<FolderId>,
    /// The folders our `Hello` named: what the peer believes we offer.
    offered: BTreeSet<FolderId>,
    send: SendHalf,
    recv: RecvHalf,
}

/// Passes on the revocations in a peer's `Hello` whose signatures hold (ADR 0040).
fn learn_revocations<P: Protocol>(
    side: &dyn Side<P>,
    revocations: Option<Vec<Signed<DeviceRevocation>>>,
) {
    let verified: Vec<_> = revocations
        .unwrap_or_default()
        .into_iter()
        .take(MAX_REVOCATIONS_PER_HELLO)
        .filter(|r| r.verify_revocation().is_ok())
        .collect();
    if !verified.is_empty() {
        side.learn_revocations(verified);
    }
}

/// Refuses a session with a revoked peer, and any session once this device is revoked
/// (ADR 0040).
fn check_revoked<P: Protocol>(side: &dyn Side<P>, peer: &DeviceCertificate) -> Result<()> {
    if side.is_revoked(peer) {
        return Err(Error::Auth("the peer's device is revoked"));
    }
    let own = side.certificate();
    if own
        .verify_certificate()
        .is_ok_and(|own| side.is_revoked(&own))
    {
        return Err(Error::Auth("this device is revoked"));
    }
    Ok(())
}

/// Reads the peer's first `Hello`.
async fn read_hello<P: Protocol>(
    recv: &mut RecvHalf,
) -> Result<(
    Signed<DeviceCertificate>,
    Vec<FolderId>,
    Option<i64>,
    Option<Vec<Signed<DeviceRevocation>>>,
)> {
    let message: P::Message = frame_io::expect_limited(recv, crate::MAX_UNAUTHENTICATED).await?;
    match P::event(message) {
        Event::Hello {
            certificate,
            folders,
            clock_ms,
            revocations,
        } => Ok((certificate, folders, clock_ms, revocations)),
        _ => Err(Error::Protocol("expected Hello".into())),
    }
}

/// Performs the `Hello` exchange on the main stream.
async fn handshake<P: Protocol>(
    conn: &Conn,
    side: &dyn Side<P>,
    initiator: bool,
) -> Result<Handshake> {
    let (mut send, mut recv) = if initiator {
        conn.open_bi().await
    } else {
        conn.accept_bi().await
    }
    .map_err(|e| Error::Stream(e.to_string()))?;

    let (theirs, peer, offered, clock) = if initiator {
        // The dialer offers everything before it knows the peer, then narrows its offer with a
        // second `Hello` as soon as the session starts (see `run`).
        let offered: BTreeSet<FolderId> = side.folders().into_iter().collect();
        frame_io::write(&mut send, &hello(side, &offered)).await?;
        let (certificate, folders, clock_ms, revocations) = read_hello::<P>(&mut recv).await?;
        let peer = verify_peer(&certificate, conn)?;
        learn_revocations(side, revocations);
        check_revoked(side, &peer)?;
        side.learn_certificate(certificate);
        (folders, peer, offered, clock_ms)
    } else {
        let (certificate, folders, clock_ms, revocations) = read_hello::<P>(&mut recv).await?;
        let peer = verify_peer(&certificate, conn)?;
        learn_revocations(side, revocations);
        // A revoked peer is still told what this side knows, so it learns that it is revoked,
        // and is offered nothing.
        let refused = check_revoked(side, &peer);
        let offered = if refused.is_ok() {
            side.learn_certificate(certificate);
            offer_for(side, &peer)
        } else {
            BTreeSet::new()
        };
        frame_io::write(&mut send, &hello(side, &offered)).await?;
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
    let clock_offset_ms = clock.map(|theirs| theirs.saturating_sub(side.now_ms()).max(-i64::MAX));

    // Share what both sides named, and only folders the peer may sync now. Folders added or
    // removed since our `Hello` are announced by `reoffer` once the session runs.
    let theirs: BTreeSet<FolderId> = theirs.into_iter().collect();
    let allowed = offer_for(side, &peer);
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
struct Session<P: Protocol> {
    peer: Peer,
    /// The peer's latest offer.
    theirs: BTreeSet<FolderId>,
    /// Our latest offer.
    offered: BTreeSet<FolderId>,
    /// Work for the writer task.
    out: mpsc::UnboundedSender<Outgoing<P>>,
    /// Items received from this peer, not to be pushed back to it.
    received: Received,
    /// Folders whose items from this peer were held this session, for want of their authors'
    /// certificates: the peer is not told it is caught up with them, so it sends them again next
    /// time (ADR 0024).
    held: BTreeSet<FolderId>,
    /// The revocations this peer has been told of (ADR 0040).
    revocations_told: Vec<Signed<DeviceRevocation>>,
}

impl<P: Protocol> Session<P> {
    /// Recomputes the shared set. Returns folders that became shared.
    fn reshare(&mut self) -> Vec<FolderId> {
        let now: BTreeSet<FolderId> = self.theirs.intersection(&self.offered).copied().collect();
        let mut shared = self.peer.shared.write_locked();
        let added = now.difference(&shared).copied().collect();
        *shared = now;
        added
    }

    fn send(&self, outgoing: Outgoing<P>) -> Result<()> {
        self.out.send(outgoing).map_err(|_| Error::Closed)
    }

    fn request_index(
        &self,
        side: &dyn Side<P>,
        folders: impl IntoIterator<Item = FolderId>,
    ) -> Result<()> {
        let device = self.peer.certificate.device;
        for folder in folders {
            let (epoch, since) = side.peer_progress(&folder, device);
            self.send(Outgoing::Message(P::index_request(folder, epoch, since)))?;
        }
        Ok(())
    }
}

/// Work for a session's writer task. Only the writer writes to the main stream, so the reading
/// side never waits on the network (ADR 0024).
enum Outgoing<P: Protocol> {
    Message(P::Message),
    /// Answer an index request, in pages.
    Index {
        folder: FolderId,
        epoch: u64,
        since: u64,
    },
    /// Send the peer new items of every folder it has been sent the index of.
    Push,
}

/// Hashes of the items a peer sent in this session. The writer skips them when pushing, so
/// items are not echoed back to where they came from (ADR 0024).
type Received = Arc<std::sync::Mutex<std::collections::HashSet<[u8; 32]>>>;

#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which no message has"
)]
fn item_hash<I: Serialize>(item: &I) -> [u8; 32] {
    *blake3::hash(&postcard::to_stdvec(item).expect("in-memory encoding cannot fail")).as_bytes()
}

#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which no message has"
)]
fn encoded_len<I: Serialize>(item: &I) -> usize {
    postcard::to_stdvec(item)
        .expect("in-memory encoding cannot fail")
        .len()
}

/// Runs a session until the peer disconnects. `on_ready` is called once the handshake
/// succeeds.
pub(crate) async fn run<P: Protocol>(
    conn: Conn,
    side: Arc<dyn Side<P>>,
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
        handshake(&conn, side.as_ref(), initiator),
    )
    .await?;
    let device = peer.certificate.device;
    let initial: Vec<FolderId> = peer.shared.read_locked().iter().copied().collect();
    debug!(%device, shared = initial.len(), "sync session started");
    on_ready(peer.clone());

    tokio::spawn(serve_requests(
        conn.clone(),
        side.clone(),
        peer.shared.clone(),
    ));

    // Reading frames is not cancel-safe, so a dedicated task feeds them through a channel.
    let (tx, mut incoming) = mpsc::channel::<Result<P::Message>>(16);
    tokio::spawn(read_loop::<P>(recv, tx));
    let (out, queue) = mpsc::unbounded_channel();
    let received = Received::default();
    let mut writer = tokio::spawn(write_loop(
        send,
        side.clone(),
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
        revocations_told: side.revocations(),
    };
    session.request_index(side.as_ref(), initial)?;
    // Subscribe before re-checking the offer: a folder added after the handshake computed it but
    // before this point would otherwise never be offered.
    let mut changes = side.changes();
    reoffer(&mut session, side.as_ref())?;
    // The peer is asked about the authors of items stored here that this side cannot vouch for
    // yet: from 0.20, versions were applied before their authors were vouched for.
    tokio::spawn({
        let (conn, limiter, side) = (conn.clone(), limiter.clone(), side.clone());
        let shared: Vec<FolderId> = session.peer.shared.read_locked().iter().copied().collect();
        let peer = session.peer.certificate.clone();
        async move {
            for folder in shared {
                let authors = side.stored_unknown_authors(&folder);
                if !authors.is_empty() {
                    fetch_certificates(&conn, &limiter, &side, &peer, folder, authors).await;
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
                let event = P::event(message);
                // The certificates of authors this side does not know come before their
                // items, from the peer that sent them (ADR 0024).
                if let Event::IndexBatch { folder, items, .. } = &event {
                    let unknown = side.unknown_authors(items);
                    if session.peer.shares(folder)
                        && !unknown.is_empty()
                        && !fetch_certificates(
                            &conn,
                            &limiter,
                            &side,
                            &session.peer.certificate,
                            *folder,
                            unknown,
                        )
                        .await
                    {
                        session.held.insert(*folder);
                    }
                }
                if let Err(e) = handle(event, &conn, &mut session, side.as_ref()) {
                    break Err(e);
                }
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                if let Err(e) = reoffer(&mut session, side.as_ref())
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

async fn read_loop<P: Protocol>(mut recv: RecvHalf, tx: mpsc::Sender<Result<P::Message>>) {
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
async fn write_loop<P: Protocol>(
    mut send: SendHalf,
    side: Arc<dyn Side<P>>,
    peer: Peer,
    mut queue: mpsc::UnboundedReceiver<Outgoing<P>>,
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
                    side.as_ref(),
                    &received,
                    folder,
                    epoch,
                    since,
                    false,
                )
                .await?
                else {
                    frame_io::write(&mut send, &P::error(Code::UnknownFolder)).await?;
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
                        side.as_ref(),
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
fn drain_pushes<P: Protocol>(
    queue: &mut mpsc::UnboundedReceiver<Outgoing<P>>,
    backlog: &mut std::collections::VecDeque<Outgoing<P>>,
) {
    while let Ok(next) = queue.try_recv() {
        if !matches!(next, Outgoing::Push) {
            backlog.push_back(next);
        }
    }
}

/// Splits a page's items into batches of at most `max_bytes` encoded, each holding at least one
/// item.
fn by_size<I: Serialize>(items: Vec<I>, max_bytes: usize) -> Vec<Vec<I>> {
    let mut batches: Vec<Vec<I>> = vec![Vec::new()];
    let mut size = 0usize;
    for item in items {
        let len = encoded_len(&item);
        if size.saturating_add(len) > max_bytes && batches.last().is_some_and(|b| !b.is_empty()) {
            batches.push(Vec::new());
            size = 0;
        }
        size = size.saturating_add(len);
        if let Some(batch) = batches.last_mut() {
            batch.push(item);
        }
    }
    batches
}

/// Sends `folder`'s index after `since` in pages of [`BATCH`], skipping items the peer sent
/// us. Returns the `(epoch, up_to)` sent, or `None` if the folder is unknown. A push with
/// nothing new sends nothing.
async fn send_index<P: Protocol>(
    send: &mut SendHalf,
    side: &dyn Side<P>,
    received: &Received,
    folder: FolderId,
    epoch: u64,
    since: u64,
    push: bool,
) -> Result<Option<(u64, u64)>> {
    let (mut epoch_asked, mut since) = (epoch, since);
    let mut first = true;
    loop {
        let Some(page) = side.index_since(&folder, epoch_asked, since, BATCH) else {
            return Ok(None);
        };
        let last = page.items.len() < BATCH;
        let unchanged = page.epoch == epoch && page.up_to == since;
        if push && first && last && page.items.is_empty() && unchanged {
            return Ok(Some((page.epoch, page.up_to)));
        }
        first = false;
        let items: Vec<_> = {
            let mut received = received.locked();
            page.items
                .into_iter()
                .filter(|v| !received.remove(&item_hash(v)))
                .collect()
        };
        // Pages in the middle only matter for their items; the last one also carries the
        // position the peer records. A page too large for one batch goes in several, and only
        // the last of the last page is marked so.
        if last || !items.is_empty() {
            let batches = by_size(items, P::MAX_BATCH_BYTES);
            let count = batches.len();
            for (n, items) in batches.into_iter().enumerate() {
                let final_batch = last && n.saturating_add(1) == count;
                frame_io::write(
                    send,
                    &P::index_batch(folder, page.epoch, page.up_to, items, final_batch),
                )
                .await?;
            }
        }
        if last {
            return Ok(Some((page.epoch, page.up_to)));
        }
        (epoch_asked, since) = (page.epoch, page.up_to);
    }
}

/// Re-sends `Hello` if the folders we offer the peer changed (a folder was added or removed,
/// or membership changed).
fn reoffer<P: Protocol>(session: &mut Session<P>, side: &dyn Side<P>) -> Result<()> {
    if let Err(e) = check_revoked(side, &session.peer.certificate) {
        // Told why before the session ends: the Hello carries the revocations, and offers
        // nothing.
        let _ = session.send(Outgoing::Message(hello(side, &BTreeSet::new())));
        return Err(e);
    }
    let offer = offer_for(side, &session.peer.certificate);
    let revocations = side.revocations();
    // A revocation can be new, or replace one of the same device (ADR 0040).
    let told = revocations == session.revocations_told;
    if offer == session.offered && told {
        return Ok(());
    }
    session.revocations_told = revocations.clone();
    session.send(Outgoing::Message(P::hello(
        side.certificate(),
        offer.iter().copied().collect(),
        side.now_ms(),
        revocations,
    )))?;
    if !told {
        // Withdrawals wait for a whole index from a peer since the revocation was learnt
        // (ADR 0040): asked for now, rather than at the folder's next change.
        let shared: Vec<FolderId> = session.peer.shared.read_locked().iter().copied().collect();
        session.request_index(side, shared)?;
    }
    if offer == session.offered {
        return Ok(());
    }
    session.offered = offer;
    let added = session.reshare();
    debug!(device = %session.peer.certificate.device, newly_shared = added.len(), "offer updated");
    session.request_index(side, added)
}

fn handle<P: Protocol>(
    event: Event<P>,
    conn: &Conn,
    session: &mut Session<P>,
    side: &dyn Side<P>,
) -> Result<()> {
    let device = session.peer.certificate.device;
    match event {
        Event::Hello {
            certificate,
            folders,
            revocations,
            ..
        } => {
            // An updated offer. The certificate must still be for this connection's device.
            let certificate = verify_peer(&certificate, conn)?;
            learn_revocations(side, revocations);
            check_revoked(side, &certificate)?;
            if certificate.user != session.peer.certificate.user {
                return Err(Error::Protocol(
                    "certificate changed user mid-session".into(),
                ));
            }
            session.theirs = folders.into_iter().collect();
            let added = session.reshare();
            debug!(%device, offered = session.theirs.len(), newly_shared = added.len(), "peer updated its offer");
            session.request_index(side, added)
        }
        Event::IndexRequest {
            folder,
            epoch,
            since,
        } => {
            if !session.peer.shares(&folder) {
                debug!(%device, %folder, "index requested for a folder not shared");
                return session.send(Outgoing::Message(P::error(Code::NotAMember)));
            }
            debug!(%device, %folder, epoch, since, "index requested");
            session.send(Outgoing::Index {
                folder,
                epoch,
                since,
            })
        }
        Event::IndexBatch {
            folder,
            epoch,
            up_to,
            items,
            last,
        } => {
            if !session.peer.shares(&folder) {
                // Possible briefly after the folder stopped being shared; ignore it.
                debug!(%device, %folder, "ignoring index for a folder not shared");
                return Ok(());
            }
            debug!(%device, %folder, items = items.len(), last, "index received");
            {
                let mut received = session.received.locked();
                // Items we already had are never pushed, so their hashes would stay; bound the
                // set. Forgetting only means some items may be echoed.
                if received.len() > MAX_RECEIVED {
                    received.clear();
                }
                received.extend(items.iter().map(item_hash));
            }
            side.apply(&folder, device, items);
            if last && !session.held.contains(&folder) {
                side.set_peer_progress(&folder, device, epoch, up_to);
            }
            Ok(())
        }
        Event::Error(code) => {
            warn!(%device, %code, "peer reported an error");
            Ok(())
        }
        Event::CertificateRequest { .. } => Err(unexpected("CertificateRequest")),
        Event::CertificateResponse(_) => Err(unexpected("CertificateResponse")),
        Event::Other(message) => Err(unexpected(P::name(&message))),
    }
}

fn unexpected(name: &str) -> Error {
    Error::Protocol(format!("unexpected message on main stream: {name}"))
}

/// Answers requests on extra streams, one each: certificates, and the protocol's own.
async fn serve_requests<P: Protocol>(conn: Conn, side: Arc<dyn Side<P>>, shared: Shared) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let side = side.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            let request = crate::within(
                crate::REQUEST_TIMEOUT,
                frame_io::expect::<_, P::Message>(&mut recv),
            );
            let response = match request.await.map(P::event) {
                Ok(Event::CertificateRequest { folder, device }) => {
                    if !shared.read_locked().contains(&folder) {
                        P::error(Code::NotAMember)
                    } else {
                        match side.certificate_of(&folder, device) {
                            Some(certificate) => P::certificate_response(certificate),
                            None => P::error(Code::UnknownDevice),
                        }
                    }
                }
                Ok(Event::Other(request)) => {
                    let folders = shared.read_locked().clone();
                    side.answer_other(request, &folders)
                }
                _ => P::error(Code::ProtocolViolation),
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
/// items are held and sent again; a peer answering that it cannot vouch for an author is passed
/// to [`Side::cannot_vouch`], which decides what that means (ADR 0024, ADR 0023).
async fn fetch_certificates<P: Protocol>(
    conn: &Conn,
    limiter: &Arc<RateLimiter>,
    side: &Arc<dyn Side<P>>,
    peer: &DeviceCertificate,
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
                    request_certificate::<P>(&conn, &limiter, folder, author).await,
                )
            });
        }
        while let Some(joined) = requests.join_next().await {
            match joined {
                Ok((_, Ok(certificate))) => side.learn_certificate(certificate),
                // Only these say the peer knows and cannot vouch: anything else, such as a
                // peer from before certificate streams, may vouch later.
                Ok((author, Err(Error::Rejected(code))))
                    if code == "UnknownDevice" || code == "NotAMember" =>
                {
                    debug!(%author, %code, "the peer cannot vouch for an author");
                    side.cannot_vouch(&folder, author, peer);
                }
                Ok((author, Err(e))) => {
                    debug!(%author, error = %e, "certificate request failed; holding its items");
                    answered = false;
                }
                Err(_) => answered = false,
            }
        }
    }
    answered
}

/// Requests the certificate of `device`, an author in `folder`, from a connected peer on a new
/// stream (ADR 0024). The certificate is verified and must be for `device`.
pub(crate) async fn request_certificate<P: Protocol>(
    conn: &Conn,
    limiter: &RateLimiter,
    folder: FolderId,
    device: DeviceId,
) -> Result<Signed<DeviceCertificate>> {
    limiter.request(crate::traffic::Request::Certificate).await;
    crate::within(
        crate::REQUEST_TIMEOUT,
        certificate_exchange::<P>(conn, folder, device),
    )
    .await
}

async fn certificate_exchange<P: Protocol>(
    conn: &Conn,
    folder: FolderId,
    device: DeviceId,
) -> Result<Signed<DeviceCertificate>> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Stream(e.to_string()))?;
    frame_io::write(&mut send, &P::certificate_request(folder, device)).await?;
    let _ = send.finish();
    match P::event(frame_io::expect(&mut recv).await?) {
        Event::CertificateResponse(certificate) => {
            let body = certificate
                .verify_certificate()
                .map_err(|_| Error::Auth("invalid device certificate"))?;
            if body.device != device {
                return Err(Error::Protocol("certificate for another device".into()));
            }
            Ok(certificate)
        }
        Event::Error(code) => Err(Error::Rejected(code)),
        _ => Err(Error::Protocol("unexpected certificate response".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::V1;
    use tidefell_proto::sync::{ErrorCode, SyncMessage};

    #[test]
    fn coalescing_pushes_keeps_other_work_in_order() {
        let (tx, mut queue) = mpsc::unbounded_channel::<Outgoing<V1>>();
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

    #[test]
    fn batches_split_by_size_and_never_come_empty() {
        let items: Vec<Vec<u8>> = (0..5).map(|n| vec![n; 100]).collect();
        let batches = by_size(items.clone(), 250);
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
        assert_eq!(batches.concat(), items);
        // An item larger than the limit still goes, alone.
        assert_eq!(by_size(vec![vec![0u8; 500]], 250).len(), 1);
        assert_eq!(
            by_size(Vec::<Vec<u8>>::new(), 250),
            vec![Vec::<Vec<u8>>::new()]
        );
        assert_eq!(by_size(items, usize::MAX).len(), 1);
    }
}
