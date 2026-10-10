//! Async frame I/O: varint length prefix followed by a postcard body.

use serde::Serialize;
use tidefell_proto::frame::{self, Message, MAX_FRAME_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Error, Result};

/// Writes one frame.
pub(crate) async fn write<W, T>(writer: &mut W, message: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    writer
        .write_all(&frame::encode(message))
        .await
        .map_err(|e| Error::Stream(e.to_string()))
}

/// Reads one frame. Returns `None` if the stream ended cleanly before a new frame started.
pub(crate) async fn read<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: Message,
{
    read_limited(reader, MAX_FRAME_LEN).await
}

/// Reads one frame of at most `max` bytes, for messages from peers that have not yet
/// authenticated (ADR 0024).
pub(crate) async fn read_limited<R, T>(reader: &mut R, max: u64) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: Message,
{
    let mut len: u64 = 0;
    // Ten bytes of seven bits each, the most a varint of 64 bits takes.
    for shift in (0u32..70).step_by(7) {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && shift == 0 => {
                return Ok(None)
            }
            Err(e) => return Err(Error::Stream(e.to_string())),
        };
        len |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
        if byte & 0x80 == 0 {
            if len > max.min(MAX_FRAME_LEN) {
                return Err(Error::Protocol(format!("frame of {len} bytes")));
            }
            let mut body = vec![0u8; len as usize];
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| Error::Stream(e.to_string()))?;
            return decode(&body).map(Some);
        }
    }
    Err(Error::Protocol("frame length varint too long".into()))
}

/// Decodes a frame body. A body that ends before a message's last field, when that field is
/// optional, decodes with it absent: that is how a field is appended to a message and still
/// read from devices that do not send it, such as `Hello`'s clock (ADR 0030).
fn decode<T: Message>(body: &[u8]) -> Result<T> {
    T::decode_body(body).map_err(|e| Error::Protocol(e.to_string()))
}

/// Reads one frame of at most `max` bytes, treating end of stream as an error.
pub(crate) async fn expect_limited<R, T>(reader: &mut R, max: u64) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: Message,
{
    read_limited(reader, max).await?.ok_or(Error::Closed)
}

/// Reads one frame, treating end of stream as an error.
pub(crate) async fn expect<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: Message,
{
    read(reader).await?.ok_or(Error::Closed)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    /// A message for the tests; each protocol has its own.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Test(u32, Vec<u8>);

    impl Message for Test {}

    #[tokio::test]
    async fn round_trip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            write(&mut a, &Test(7, vec![1u8; 1000])).await.unwrap();
            write(&mut a, &Test(8, b"second".to_vec())).await.unwrap();
        });
        let first: Test = expect(&mut b).await.unwrap();
        assert_eq!(first.0, 7);
        assert_eq!(first.1.len(), 1000);
        let second: Test = expect(&mut b).await.unwrap();
        assert_eq!(second, Test(8, b"second".to_vec()));
        writer.await.unwrap();
        assert!(read::<_, Test>(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn limited_read_refuses_large_frames_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        write(&mut a, &Test(0, vec![0u8; 100])).await.unwrap();
        let err = read_limited::<_, Test>(&mut b, 50).await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }
}
