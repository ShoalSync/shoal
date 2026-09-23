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

use crate::backend::{Backend, IndexPage};
use crate::{frame_io, Error, Result};

/// Most versions sent in one `IndexBatch`.
const BATCH: usize = 512;

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
}

impl Peer {
    pub(crate) fn shares(&self, folder: &FolderId) -> bool {
        self.shared.read().unwrap().contains(folder)
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

    let (theirs, peer) = if initiator {
        frame_io::write(
            &mut send,
            &SyncMessage::Hello {
                certificate: backend.certificate(),
                folders: backend.folders(),
            },
        )
        .await?;
        let SyncMessage::Hello {
            certificate,
            folders,
        } = frame_io::expect(&mut recv).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        (folders, verify_peer(&certificate, conn)?)
    } else {
        let SyncMessage::Hello {
            certificate,
            folders,
        } = frame_io::expect(&mut recv).await?
        else {
            return Err(Error::Protocol("expected Hello".into()));
        };
        let peer = verify_peer(&certificate, conn)?;
        frame_io::write(
            &mut send,
            &SyncMessage::Hello {
                certificate: backend.certificate(),
                folders: offer_for(backend, &peer).into_iter().collect(),
            },
        )
        .await?;
        (folders, peer)
    };

    // The dialer offered everything before it knew the peer; from here on both sides offer
    // only folders the peer is a member of.
    let offered = offer_for(backend, &peer);
    let theirs: BTreeSet<FolderId> = theirs.into_iter().collect();
    let shared = theirs.intersection(&offered).copied().collect();
    Ok(Handshake {
        peer: Peer {
            certificate: peer,
            shared: Arc::new(RwLock::new(shared)),
        },
        theirs,
        offered,
        send,
        recv,
    })
}

/// Per-session state after the handshake.
struct Session {
    peer: Peer,
    /// The peer's latest offer.
    theirs: BTreeSet<FolderId>,
    /// Our latest offer.
    offered: BTreeSet<FolderId>,
    /// What we have sent to this peer, per folder: (epoch, up_to).
    sent: HashMap<FolderId, (u64, u64)>,
}

impl Session {
    /// Recomputes the shared set. Returns folders that became shared.
    fn reshare(&mut self) -> Vec<FolderId> {
        let now: BTreeSet<FolderId> = self.theirs.intersection(&self.offered).copied().collect();
        let mut shared = self.peer.shared.write().unwrap();
        let added = now.difference(&shared).copied().collect();
        self.sent.retain(|f, _| now.contains(f));
        *shared = now;
        added
    }
}

async fn request_index(
    backend: &dyn Backend,
    send: &mut SendStream,
    device: tidefell_proto::DeviceId,
    folders: impl IntoIterator<Item = FolderId>,
) -> Result<()> {
    for folder in folders {
        let (epoch, since) = backend.peer_progress(&folder, device);
        frame_io::write(
            send,
            &SyncMessage::IndexRequest {
                folder,
                epoch,
                since,
            },
        )
        .await?;
    }
    Ok(())
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
        mut send,
        recv,
    } = handshake(&conn, backend.as_ref(), initiator).await?;
    let device = peer.certificate.device;
    let initial: Vec<FolderId> = peer.shared.read().unwrap().iter().copied().collect();
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

    request_index(backend.as_ref(), &mut send, device, initial).await?;

    let mut session = Session {
        peer,
        theirs,
        offered,
        sent: HashMap::new(),
    };
    let mut changes = backend.changes();
    loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                handle(message?, &conn, &mut session, backend.as_ref(), &mut send).await?;
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    break;
                }
                reoffer(&mut session, backend.as_ref(), &mut send).await?;
                push_updates(backend.as_ref(), &mut send, &mut session.sent).await?;
            }
        }
    }
    conn.close(0u32.into(), b"done");
    Ok(())
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

/// Re-sends `Hello` if the folders we offer the peer changed (a folder was added or removed,
/// or membership changed).
async fn reoffer(
    session: &mut Session,
    backend: &dyn Backend,
    send: &mut SendStream,
) -> Result<()> {
    let offer = offer_for(backend, &session.peer.certificate);
    if offer == session.offered {
        return Ok(());
    }
    frame_io::write(
        send,
        &SyncMessage::Hello {
            certificate: backend.certificate(),
            folders: offer.iter().copied().collect(),
        },
    )
    .await?;
    session.offered = offer;
    let added = session.reshare();
    request_index(backend, send, session.peer.certificate.device, added).await
}

async fn handle(
    message: SyncMessage,
    conn: &Connection,
    session: &mut Session,
    backend: &dyn Backend,
    send: &mut SendStream,
) -> Result<()> {
    let peer = &session.peer;
    let device = peer.certificate.device;
    let sent = &mut session.sent;
    match message {
        SyncMessage::Hello {
            certificate,
            folders,
        } => {
            // An updated offer. The certificate must still be for this connection's device.
            let certificate = verify_peer(&certificate, conn)?;
            if certificate.user != peer.certificate.user {
                return Err(Error::Protocol(
                    "certificate changed user mid-session".into(),
                ));
            }
            session.theirs = folders.into_iter().collect();
            let added = session.reshare();
            request_index(backend, send, device, added).await
        }
        SyncMessage::IndexRequest {
            folder,
            epoch,
            since,
        } => {
            if !peer.shares(&folder) {
                return send_error(send, ErrorCode::NotAMember).await;
            }
            let Some(page) = backend.index_since(&folder, epoch, since) else {
                return send_error(send, ErrorCode::UnknownFolder).await;
            };
            sent.insert(folder, (page.epoch, page.up_to));
            send_page(send, folder, page).await
        }
        SyncMessage::IndexBatch {
            folder,
            epoch,
            up_to,
            versions,
            last,
        } => {
            if !peer.shares(&folder) {
                // Possible briefly after the folder stopped being shared; ignore it.
                return Ok(());
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

async fn push_updates(
    backend: &dyn Backend,
    send: &mut SendStream,
    sent: &mut HashMap<FolderId, (u64, u64)>,
) -> Result<()> {
    for (folder, (epoch, up_to)) in sent.iter_mut() {
        let Some(page) = backend.index_since(folder, *epoch, *up_to) else {
            continue;
        };
        if page.versions.is_empty() && page.epoch == *epoch {
            continue;
        }
        (*epoch, *up_to) = (page.epoch, page.up_to);
        send_page(send, *folder, page).await?;
    }
    Ok(())
}

async fn send_page(send: &mut SendStream, folder: FolderId, page: IndexPage) -> Result<()> {
    let mut batches: Vec<Vec<_>> = page.versions.chunks(BATCH).map(<[_]>::to_vec).collect();
    if batches.is_empty() {
        batches.push(Vec::new());
    }
    let count = batches.len();
    for (i, versions) in batches.into_iter().enumerate() {
        frame_io::write(
            send,
            &SyncMessage::IndexBatch {
                folder,
                epoch: page.epoch,
                up_to: page.up_to,
                versions,
                last: i + 1 == count,
            },
        )
        .await?;
    }
    Ok(())
}

async fn send_error(send: &mut SendStream, code: ErrorCode) -> Result<()> {
    frame_io::write(send, &SyncMessage::Error { code }).await
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
            let response = match frame_io::expect(&mut recv).await {
                Ok(SyncMessage::ManifestRequest { folder, content }) => {
                    if !shared.read().unwrap().contains(&folder) {
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
