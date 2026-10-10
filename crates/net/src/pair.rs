//! `tidefell/pair/1`: adding a new device to a user. See `specs/pair-v1.md` and ADR 0015.
//!
//! The new device runs a [`PairingServer`], a pairing-only endpoint that shows a
//! [`PairingCode`]. A device holding the user key calls [`crate::Node::pair`] with the code, shows
//! the confirmation code, and then completes or declines the returned [`ApproverSession`].

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl};
use tidefell_identity::pairing;
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::pair::{FolderOffer, PairErrorCode, PairMessage, PairingCode};
use tidefell_proto::{DeviceId, PublicKey, Signed, ALPN_PAIR_V1};
use tokio::sync::mpsc;
use tracing::debug;

use crate::locks::Locked;
use crate::node::{bind_endpoint, NodeConfig, RelayConfig};
use crate::{frame_io, Error, Result};

/// Connections with a bad proof after which a code stops being served.
const MAX_FAILURES: u32 = 5;
/// How long the new device waits for the person to confirm on the approver.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(300);
/// How long to wait for a relay before showing a code without one.
const RELAY_TIMEOUT: Duration = Duration::from_secs(10);

/// What happened on a [`PairingServer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingEvent {
    /// An approver proved it has the code; the person should compare `confirmation_code` with
    /// the one on the approver.
    Confirming {
        /// The approver's certificate: its name and user.
        approver: DeviceCertificate,
        /// Six digits, also shown on the approver.
        confirmation_code: String,
    },
    /// Pairing finished; the code is used up.
    Paired(Box<Paired>),
    /// A pairing attempt failed; the code stays valid unless it expired or failed too often.
    Failed {
        /// Why.
        reason: String,
    },
}

/// The result of a successful pairing, on the new device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    /// This device's new certificate.
    pub certificate: Signed<DeviceCertificate>,
    /// The approver, to keep connected.
    pub approver: DeviceCertificate,
    /// How to reach the approver.
    pub approver_addr: EndpointAddr,
    /// Folders the user has on the approver.
    pub folders: Vec<FolderOffer>,
}

struct CodeState {
    secret: [u8; 16],
    expires: Instant,
    used: bool,
    busy: bool,
    failures: u32,
}

#[derive(Clone)]
struct PairProtocol {
    device: DeviceId,
    name: String,
    state: Arc<Mutex<CodeState>>,
    events: mpsc::UnboundedSender<PairingEvent>,
}

impl std::fmt::Debug for PairProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairProtocol").finish_non_exhaustive()
    }
}

/// A new device's pairing-only endpoint.
pub struct PairingServer {
    endpoint: Endpoint,
    router: Router,
    code: PairingCode,
    events: mpsc::UnboundedReceiver<PairingEvent>,
}

impl std::fmt::Debug for PairingServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingServer")
            .field("device", &self.code.device)
            .finish_non_exhaustive()
    }
}

impl PairingServer {
    /// Binds a pairing-only endpoint with the device key in `config` and creates a code valid
    /// for `ttl`. `name` is the name this device suggests for itself.
    pub async fn start(config: NodeConfig, name: String, ttl: Duration) -> Result<Self> {
        let endpoint = bind_endpoint(&config, vec![ALPN_PAIR_V1.to_vec()]).await?;
        if config.relay != RelayConfig::Disabled {
            // Include the relay in the code when one is reachable.
            let _ = tokio::time::timeout(RELAY_TIMEOUT, endpoint.online()).await;
        }
        let secret = pairing::new_secret().map_err(|e| Error::Bind(e.to_string()))?;
        let device = DeviceId(*endpoint.id().as_bytes());
        let addr = endpoint.addr();
        let code = PairingCode {
            device,
            secret,
            relay: addr.relay_urls().next().map(|u| u.to_string()),
            addrs: addr.ip_addrs().copied().collect(),
        };
        let (tx, events) = mpsc::unbounded_channel();
        let protocol = PairProtocol {
            device,
            name,
            state: Arc::new(Mutex::new(CodeState {
                secret,
                // An impossibly long lifetime expires the code at once rather than overflow.
                expires: Instant::now().checked_add(ttl).unwrap_or_else(Instant::now),
                used: false,
                busy: false,
                failures: 0,
            })),
            events: tx,
        };
        let router = Router::builder(endpoint.clone())
            .accept(ALPN_PAIR_V1, protocol)
            .spawn();
        Ok(Self {
            endpoint,
            router,
            code,
            events,
        })
    }

    /// The code to show.
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    /// Waits for the next event. Returns `None` once the server has stopped.
    pub async fn next_event(&mut self) -> Option<PairingEvent> {
        self.events.recv().await
    }

    /// Stops serving and closes the endpoint.
    pub async fn shutdown(self) {
        let _ = self.router.shutdown().await;
        self.endpoint.close().await;
    }
}

impl ProtocolHandler for PairProtocol {
    async fn accept(&self, conn: Connection) -> std::result::Result<(), AcceptError> {
        let Ok((mut send, mut recv)) = conn.accept_bi().await else {
            return Ok(());
        };
        let outcome = self.serve(&conn, &mut send, &mut recv).await;
        let _ = send.finish();
        // Let the last frame reach the approver, which closes the connection once it has read
        // to the end, before reporting: the app may shut this endpoint down on `Paired`.
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
        match outcome {
            Ok(Some(paired)) => {
                let _ = self.events.send(PairingEvent::Paired(Box::new(paired)));
            }
            Ok(None) => {}
            Err(e) => {
                debug!(error = %e, "pairing attempt failed");
                let _ = self.events.send(PairingEvent::Failed {
                    reason: e.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl PairProtocol {
    async fn serve(
        &self,
        conn: &Connection,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> Result<Option<Paired>> {
        // Pairing peers are not yet trusted: small messages, and a deadline (ADR 0024).
        let request = frame_io::expect_limited(recv, crate::MAX_UNAUTHENTICATED);
        let PairMessage::Request { approver, proof } =
            crate::within(crate::HANDSHAKE_TIMEOUT, request).await?
        else {
            return reject(send, PairErrorCode::ProtocolViolation, "expected a request").await;
        };
        let approver_id = DeviceId(*conn.remote_id().as_bytes());
        let approver_cert = match approver.verify_certificate() {
            Ok(c) if c.device == approver_id => c,
            _ => {
                return reject(
                    send,
                    PairErrorCode::ProtocolViolation,
                    "approver certificate invalid",
                )
                .await
            }
        };
        let secret = {
            let mut state = self.state.locked();
            if state.used
                || state.busy
                || state.failures >= MAX_FAILURES
                || Instant::now() > state.expires
            {
                None
            } else if !pairing::verify_proof(&state.secret, &approver_id, &self.device, &proof) {
                state.failures = state.failures.saturating_add(1);
                Some(Err(()))
            } else {
                state.busy = true;
                Some(Ok(state.secret))
            }
        };
        let secret = match secret {
            None => return reject(send, PairErrorCode::Expired, "code expired or used").await,
            Some(Err(())) => return reject(send, PairErrorCode::BadProof, "wrong proof").await,
            Some(Ok(secret)) => secret,
        };
        // This attempt now owns the code until it finishes, one way or another.
        let _busy = Busy(&self.state);

        frame_io::write(
            send,
            &PairMessage::Accept {
                name: self.name.clone(),
            },
        )
        .await?;
        let _ = self.events.send(PairingEvent::Confirming {
            approver: approver_cert.clone(),
            confirmation_code: pairing::confirmation_code(&secret, &approver_id, &self.device),
        });

        let reply = tokio::time::timeout(
            CONFIRM_TIMEOUT,
            frame_io::expect_limited(recv, crate::MAX_UNAUTHENTICATED),
        )
        .await
        .map_err(|_| Error::Protocol("timed out waiting for confirmation".into()))??;
        match reply {
            PairMessage::Complete {
                certificate,
                relay,
                addrs,
                folders,
            } => {
                let valid = certificate.signer == PublicKey::from(approver_cert.user)
                    && certificate
                        .verify_certificate()
                        .is_ok_and(|c| c.device == self.device && c.user == approver_cert.user);
                if !valid {
                    return reject(
                        send,
                        PairErrorCode::BadCertificate,
                        "certificate is not for this device",
                    )
                    .await;
                }
                self.state.locked().used = true;
                let id = EndpointId::from_bytes(approver_id.as_bytes())
                    .map_err(|_| Error::Auth("invalid approver ID"))?;
                let mut approver_addr = EndpointAddr::new(id);
                if let Some(url) = relay.and_then(|r| r.parse::<RelayUrl>().ok()) {
                    approver_addr = approver_addr.with_relay_url(url);
                }
                for ip in addrs {
                    approver_addr = approver_addr.with_ip_addr(ip);
                }
                Ok(Some(Paired {
                    certificate,
                    approver: approver_cert,
                    approver_addr,
                    folders,
                }))
            }
            PairMessage::Reject { code } => Err(Error::Rejected(format!("{code:?}"))),
            _ => reject(send, PairErrorCode::ProtocolViolation, "unexpected message").await,
        }
    }
}

/// Clears [`CodeState::busy`] when the attempt that set it ends.
struct Busy<'a>(&'a Mutex<CodeState>);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.locked().busy = false;
    }
}

async fn reject<T>(send: &mut SendStream, code: PairErrorCode, reason: &str) -> Result<T> {
    let _ = frame_io::write(send, &PairMessage::Reject { code }).await;
    Err(Error::Rejected(reason.to_owned()))
}

/// The approver's side of a pairing, after the new device accepted the proof.
pub struct ApproverSession {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    device: DeviceId,
    suggested_name: String,
    confirmation_code: String,
}

impl std::fmt::Debug for ApproverSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApproverSession")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl ApproverSession {
    /// Dials the device in `code` and proves the code, as `approver` (this device's certificate).
    pub(crate) async fn start(
        endpoint: &Endpoint,
        approver: Signed<DeviceCertificate>,
        code: &PairingCode,
    ) -> Result<Self> {
        let id = EndpointId::from_bytes(code.device.as_bytes())
            .map_err(|_| Error::Connect("invalid device ID in pairing code".into()))?;
        let mut addr = EndpointAddr::new(id);
        if let Some(url) = code
            .relay
            .as_deref()
            .and_then(|r| r.parse::<RelayUrl>().ok())
        {
            addr = addr.with_relay_url(url);
        }
        for ip in &code.addrs {
            addr = addr.with_ip_addr(*ip);
        }
        let conn = endpoint
            .connect(addr, ALPN_PAIR_V1)
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Stream(e.to_string()))?;
        let me = DeviceId(*endpoint.id().as_bytes());
        frame_io::write(
            &mut send,
            &PairMessage::Request {
                approver,
                proof: pairing::proof(&code.secret, &me, &code.device),
            },
        )
        .await?;
        let reply = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED);
        match crate::within(crate::HANDSHAKE_TIMEOUT, reply).await? {
            PairMessage::Accept { name } => Ok(Self {
                conn,
                send,
                recv,
                device: code.device,
                suggested_name: name,
                confirmation_code: pairing::confirmation_code(&code.secret, &me, &code.device),
            }),
            PairMessage::Reject { code } => Err(Error::Rejected(format!("{code:?}"))),
            _ => Err(Error::Protocol("expected accept".into())),
        }
    }

    /// The new device's ID.
    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// The name the new device suggests for itself.
    pub fn suggested_name(&self) -> &str {
        &self.suggested_name
    }

    /// Six digits the new device also shows.
    pub fn confirmation_code(&self) -> &str {
        &self.confirmation_code
    }

    /// Sends the certificate, this device's addresses and the folders to offer. Fails if the
    /// new device rejects the certificate.
    pub async fn complete(
        mut self,
        certificate: Signed<DeviceCertificate>,
        approver_addr: &EndpointAddr,
        folders: Vec<FolderOffer>,
    ) -> Result<()> {
        let addrs: Vec<SocketAddr> = approver_addr.ip_addrs().copied().collect();
        frame_io::write(
            &mut self.send,
            &PairMessage::Complete {
                certificate,
                relay: approver_addr.relay_urls().next().map(|u| u.to_string()),
                addrs,
                folders,
            },
        )
        .await?;
        let _ = self.send.finish();
        let reply = frame_io::read_limited(&mut self.recv, crate::MAX_UNAUTHENTICATED);
        let reply: Option<PairMessage> = crate::within(crate::HANDSHAKE_TIMEOUT, reply).await?;
        self.conn.close(0u32.into(), b"done");
        match reply {
            None => Ok(()),
            Some(PairMessage::Reject { code }) => Err(Error::Rejected(format!("{code:?}"))),
            Some(_) => Err(Error::Protocol("unexpected reply".into())),
        }
    }

    /// Tells the new device the person declined.
    pub async fn decline(mut self) -> Result<()> {
        frame_io::write(
            &mut self.send,
            &PairMessage::Reject {
                code: PairErrorCode::Declined,
            },
        )
        .await?;
        let _ = self.send.finish();
        let reply =
            frame_io::read_limited::<_, PairMessage>(&mut self.recv, crate::MAX_UNAUTHENTICATED);
        let _ = crate::within(crate::HANDSHAKE_TIMEOUT, reply).await;
        self.conn.close(0u32.into(), b"declined");
        Ok(())
    }
}
