//! An in-memory network for simulations (ADR 0036): nodes register by device, dial each other
//! with a protocol name as iroh does, and get connections whose streams are channels. The
//! simulation partitions and heals it. A partition closes every connection across it and
//! refuses new ones until healed; it never looks like a stream that ended cleanly.

use std::collections::{HashMap, HashSet};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use shoal_proto::DeviceId;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Notify};

use crate::locks::Locked;
use crate::transport::{Conn, RecvHalf, SendHalf};
use crate::{Error, Result};

static NEXT_LINK: AtomicUsize = AtomicUsize::new(1);

/// A connection a node accepted, with the protocol it was opened for.
pub(crate) struct Incoming {
    pub(crate) alpn: Vec<u8>,
    pub(crate) conn: Conn,
}

/// A simulated network. Cloning it gives another handle to the same network.
#[derive(Debug, Clone, Default)]
pub struct Network {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    nodes: HashMap<DeviceId, mpsc::UnboundedSender<IncomingLink>>,
    /// Each device's side of the network while partitioned; devices not listed are on side 0.
    sides: HashMap<DeviceId, u32>,
    links: Vec<Weak<Shared>>,
}

struct IncomingLink {
    alpn: Vec<u8>,
    link: Link,
}

impl std::fmt::Debug for IncomingLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingLink").finish_non_exhaustive()
    }
}

impl State {
    fn side(&self, device: &DeviceId) -> u32 {
        self.sides.get(device).copied().unwrap_or(0)
    }
}

impl Network {
    /// A network with no nodes and no partition.
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits the network: each device in `sides` can reach only the devices on its side,
    /// numbered by the side it is given; devices not given are on side 0. Connections across
    /// sides close at once.
    pub fn partition(&self, sides: impl IntoIterator<Item = (DeviceId, u32)>) {
        let mut state = self.state.locked();
        state.sides = sides.into_iter().collect();
        let mut kept = Vec::new();
        for weak in std::mem::take(&mut state.links) {
            let Some(shared) = weak.upgrade() else {
                continue;
            };
            if state.side(&shared.ends.0) != state.side(&shared.ends.1) {
                shared.close();
            } else if !shared.is_closed() {
                kept.push(Arc::downgrade(&shared));
            }
        }
        state.links = kept;
    }

    /// Joins every side again. Devices reconnect as they would after any outage: on their own.
    pub fn heal(&self) {
        self.state.locked().sides.clear();
    }

    /// Cuts every connection `device` has, as when it stops.
    pub fn disconnect(&self, device: DeviceId) {
        let state = self.state.locked();
        for shared in state.links.iter().filter_map(Weak::upgrade) {
            if shared.ends.0 == device || shared.ends.1 == device {
                shared.close();
            }
        }
    }

    /// Adds `device` to the network, replacing an earlier node of the same device, and returns
    /// the connections others open to it.
    pub(crate) fn register(&self, device: DeviceId) -> mpsc::UnboundedReceiver<Incoming> {
        let (tx, mut rx) = mpsc::unbounded_channel::<IncomingLink>();
        self.state.locked().nodes.insert(device, tx);
        let (out, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(link) = rx.recv().await {
                let accepted = Incoming {
                    alpn: link.alpn,
                    conn: Conn::Memory(link.link),
                };
                if out.send(accepted).is_err() {
                    break;
                }
            }
        });
        incoming
    }

    /// Removes `device` from the network and closes its connections.
    pub(crate) fn deregister(&self, device: DeviceId) {
        self.state.locked().nodes.remove(&device);
        self.disconnect(device);
    }

    /// Connects `from` to `to` for protocol `alpn`.
    pub(crate) fn dial(&self, from: DeviceId, to: DeviceId, alpn: &[u8]) -> Result<Conn> {
        let mut state = self.state.locked();
        if state.side(&from) != state.side(&to) {
            return Err(Error::Connect(format!("{to} is unreachable")));
        }
        let Some(node) = state.nodes.get(&to).cloned() else {
            return Err(Error::Connect(format!("{to} is not on the network")));
        };
        let shared = Arc::new(Shared {
            ends: (from, to),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
            writers: Mutex::new(Vec::new()),
        });
        let id = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
        let (to_remote, remote_streams) = mpsc::unbounded_channel();
        let (to_local, local_streams) = mpsc::unbounded_channel();
        let local = Link {
            shared: shared.clone(),
            local: from,
            remote: to,
            id,
            outgoing: to_remote,
            incoming: Arc::new(tokio::sync::Mutex::new(local_streams)),
        };
        let remote = Link {
            shared: shared.clone(),
            local: to,
            remote: from,
            id,
            outgoing: to_local,
            incoming: Arc::new(tokio::sync::Mutex::new(remote_streams)),
        };
        node.send(IncomingLink {
            alpn: alpn.to_vec(),
            link: remote,
        })
        .map_err(|_| Error::Connect(format!("{to} has stopped")))?;
        state.links.push(Arc::downgrade(&shared));
        Ok(Conn::Memory(local))
    }

    /// The devices on the network.
    pub fn devices(&self) -> HashSet<DeviceId> {
        self.state.locked().nodes.keys().copied().collect()
    }
}

/// What both ends of a connection share.
struct Shared {
    ends: (DeviceId, DeviceId),
    closed: AtomicBool,
    notify: Notify,
    /// Every stream's sending side, to fail it when the connection closes.
    writers: Mutex<Vec<mpsc::WeakUnboundedSender<Chunk>>>,
}

impl Shared {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        for writer in self.writers.locked().drain(..) {
            if let Some(writer) = writer.upgrade() {
                let _ = writer.send(Err(io::ErrorKind::ConnectionAborted));
            }
        }
        self.notify.notify_waiters();
    }
}

/// Bytes written to a stream, or how it failed.
type Chunk = std::result::Result<Vec<u8>, io::ErrorKind>;

type Streams = mpsc::UnboundedReceiver<(Writer, Reader)>;

/// One end of a simulated connection.
#[derive(Clone)]
pub(crate) struct Link {
    shared: Arc<Shared>,
    local: DeviceId,
    remote: DeviceId,
    id: usize,
    outgoing: mpsc::UnboundedSender<(Writer, Reader)>,
    incoming: Arc<tokio::sync::Mutex<Streams>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("local", &self.local)
            .field("remote", &self.remote)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Link {
    pub(crate) fn remote(&self) -> DeviceId {
        self.remote
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    pub(crate) fn close(&self) {
        self.shared.close();
    }

    pub(crate) async fn closed(&self) {
        loop {
            let notified = self.shared.notify.notified();
            if self.shared.is_closed() {
                return;
            }
            notified.await;
        }
    }

    fn stream(&self) -> (Writer, Reader) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.writers.locked().push(tx.downgrade());
        (
            Writer {
                tx: Some(tx),
                shared: self.shared.clone(),
            },
            Reader {
                rx,
                buffer: Vec::new(),
                shared: self.shared.clone(),
            },
        )
    }

    pub(crate) fn open_bi(&self) -> Result<(SendHalf, RecvHalf)> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let (local_writer, remote_reader) = self.stream();
        let (remote_writer, local_reader) = self.stream();
        self.outgoing
            .send((remote_writer, remote_reader))
            .map_err(|_| Error::Closed)?;
        Ok((
            SendHalf::Memory(local_writer),
            RecvHalf::Memory(local_reader),
        ))
    }

    pub(crate) async fn accept_bi(&self) -> Result<(SendHalf, RecvHalf)> {
        let mut incoming = self.incoming.lock().await;
        tokio::select! {
            stream = incoming.recv() => match stream {
                Some((writer, reader)) if !self.is_closed() => {
                    Ok((SendHalf::Memory(writer), RecvHalf::Memory(reader)))
                }
                _ => Err(Error::Closed),
            },
            () = self.closed() => Err(Error::Closed),
        }
    }
}

/// The sending side of a simulated stream.
#[derive(Debug)]
pub(crate) struct Writer {
    tx: Option<mpsc::UnboundedSender<Chunk>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("ends", &self.ends)
            .finish_non_exhaustive()
    }
}

impl Writer {
    /// Ends the stream: the reader sees its end after what was written.
    pub(crate) fn finish(&mut self) {
        self.tx = None;
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.shared.is_closed() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let Some(tx) = &self.tx else {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        };
        match tx.send(Ok(buf.to_vec())) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.finish();
        Poll::Ready(Ok(()))
    }
}

/// The receiving side of a simulated stream.
#[derive(Debug)]
pub(crate) struct Reader {
    rx: mpsc::UnboundedReceiver<Chunk>,
    /// What was received and not yet read.
    buffer: Vec<u8>,
    shared: Arc<Shared>,
}

impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if !self.buffer.is_empty() {
                let n = buf.remaining().min(self.buffer.len());
                let rest = self.buffer.split_off(n);
                buf.put_slice(&self.buffer);
                self.buffer = rest;
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(bytes))) => self.buffer = bytes,
                Poll::Ready(Some(Err(kind))) => return Poll::Ready(Err(kind.into())),
                // Every sender is gone: the writer finished, unless the connection closed first.
                Poll::Ready(None) if self.shared.is_closed() => {
                    return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()))
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn device(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    #[tokio::test]
    async fn streams_carry_bytes_and_end_cleanly() {
        let network = Network::new();
        let mut incoming = network.register(device(2));
        let conn = network.dial(device(1), device(2), b"x").unwrap();
        let accepted = incoming.recv().await.unwrap();
        assert_eq!(accepted.alpn, b"x");
        assert_eq!(accepted.conn.remote(), device(1));
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let (mut their_send, mut their_recv) = accepted.conn.accept_bi().await.unwrap();
        send.write_all(b"hello").await.unwrap();
        send.finish().unwrap();
        let mut got = Vec::new();
        their_recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"hello");
        their_send.write_all(b"back").await.unwrap();
        their_send.finish().unwrap();
        let mut got = Vec::new();
        recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"back");
    }

    #[tokio::test]
    async fn a_partition_fails_streams_and_refuses_dials_until_healed() {
        let network = Network::new();
        let mut incoming = network.register(device(2));
        let conn = network.dial(device(1), device(2), b"x").unwrap();
        let accepted = incoming.recv().await.unwrap();
        let (_send, mut recv) = conn.open_bi().await.unwrap();
        let waiting = tokio::spawn(async move {
            let mut byte = [0u8];
            recv.read_exact(&mut byte).await
        });
        network.partition([(device(2), 1)]);
        assert!(
            waiting.await.unwrap().is_err(),
            "a read across the cut fails"
        );
        assert!(accepted.conn.accept_bi().await.is_err());
        assert!(conn.is_closed());
        assert!(network.dial(device(1), device(2), b"x").is_err());
        network.heal();
        assert!(network.dial(device(1), device(2), b"x").is_ok());
    }
}
