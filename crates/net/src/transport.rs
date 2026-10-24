//! Connections to peers: over iroh, or, in simulations, in memory (ADR 0036). The protocols
//! see only what both give: bidirectional streams opened and accepted on a connection to an
//! authenticated device, and the connection closing.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use shoal_proto::DeviceId;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{Error, Result};

/// A connection to a peer.
#[derive(Debug, Clone)]
pub(crate) enum Conn {
    Iroh(Connection),
    #[cfg(feature = "simulation")]
    Memory(crate::memory::Link),
}

impl Conn {
    /// Opens a stream to the peer.
    pub(crate) async fn open_bi(&self) -> Result<(SendHalf, RecvHalf)> {
        match self {
            Self::Iroh(conn) => conn
                .open_bi()
                .await
                .map(|(s, r)| (SendHalf::Iroh(s), RecvHalf::Iroh(r)))
                .map_err(|e| Error::Stream(e.to_string())),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.open_bi(),
        }
    }

    /// Waits for the peer to open a stream.
    pub(crate) async fn accept_bi(&self) -> Result<(SendHalf, RecvHalf)> {
        match self {
            Self::Iroh(conn) => conn
                .accept_bi()
                .await
                .map(|(s, r)| (SendHalf::Iroh(s), RecvHalf::Iroh(r)))
                .map_err(|e| Error::Stream(e.to_string())),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.accept_bi().await,
        }
    }

    /// The device at the other end, as the connection authenticated it.
    pub(crate) fn remote(&self) -> DeviceId {
        match self {
            Self::Iroh(conn) => DeviceId(*conn.remote_id().as_bytes()),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.remote(),
        }
    }

    /// Identifies this connection among others to the same device.
    pub(crate) fn stable_id(&self) -> usize {
        match self {
            Self::Iroh(conn) => conn.stable_id(),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.id(),
        }
    }

    /// Closes the connection.
    pub(crate) fn close(&self, code: u32, reason: &[u8]) {
        match self {
            Self::Iroh(conn) => conn.close(code.into(), reason),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.close(),
        }
    }

    /// True once the connection is closed.
    pub(crate) fn is_closed(&self) -> bool {
        match self {
            Self::Iroh(conn) => conn.close_reason().is_some(),
            #[cfg(feature = "simulation")]
            Self::Memory(link) => link.is_closed(),
        }
    }

    /// The path in use, and its round-trip time: relayed or direct, and whether direct to an
    /// address on the local network. A simulated connection is direct and local.
    pub(crate) fn path(&self) -> Option<Path> {
        match self {
            Self::Iroh(conn) => {
                let paths = conn.paths();
                let selected = paths.iter().find(|p| p.is_selected())?;
                Some(Path {
                    relay: selected.is_relay(),
                    local: !selected.is_relay()
                        && crate::limits::is_local_addr(selected.remote_addr()),
                    rtt: selected.rtt(),
                })
            }
            #[cfg(feature = "simulation")]
            Self::Memory(_) => Some(Path {
                relay: false,
                local: true,
                rtt: std::time::Duration::ZERO,
            }),
        }
    }
}

/// The path a connection uses.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Path {
    pub(crate) relay: bool,
    pub(crate) local: bool,
    pub(crate) rtt: std::time::Duration,
}

/// The sending half of a stream.
#[derive(Debug)]
pub(crate) enum SendHalf {
    Iroh(SendStream),
    #[cfg(feature = "simulation")]
    Memory(crate::memory::Writer),
}

impl SendHalf {
    /// Ends the stream: the peer reads to its end, then sees it end.
    pub(crate) fn finish(&mut self) -> Result<()> {
        match self {
            Self::Iroh(send) => send.finish().map_err(|e| Error::Stream(e.to_string())),
            #[cfg(feature = "simulation")]
            Self::Memory(writer) => {
                writer.finish();
                Ok(())
            }
        }
    }
}

impl AsyncWrite for SendHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Iroh(send) => Pin::new(send).poll_write(cx, buf).map_err(io::Error::from),
            #[cfg(feature = "simulation")]
            Self::Memory(writer) => Pin::new(writer).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Iroh(send) => Pin::new(send).poll_flush(cx),
            #[cfg(feature = "simulation")]
            Self::Memory(writer) => Pin::new(writer).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Iroh(send) => Pin::new(send).poll_shutdown(cx),
            #[cfg(feature = "simulation")]
            Self::Memory(writer) => Pin::new(writer).poll_shutdown(cx),
        }
    }
}

/// The receiving half of a stream.
#[derive(Debug)]
pub(crate) enum RecvHalf {
    Iroh(RecvStream),
    #[cfg(feature = "simulation")]
    Memory(crate::memory::Reader),
}

impl AsyncRead for RecvHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Iroh(recv) => Pin::new(recv).poll_read(cx, buf),
            #[cfg(feature = "simulation")]
            Self::Memory(reader) => Pin::new(reader).poll_read(cx, buf),
        }
    }
}
