//! Stream framing: each message is a varint (LEB128) length followed by that many bytes of
//! postcard encoding.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Error;

/// Largest frame a peer accepts, in bytes.
pub const MAX_FRAME_LEN: u64 = 16 * 1024 * 1024;

/// Encodes `message` as one frame.
#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which no message has"
)]
pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(message).expect("in-memory encoding cannot fail");
    let mut out = Vec::with_capacity(body.len().saturating_add(10));
    write_varint(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out
}

/// Decodes one frame from the start of `input`. Returns the message and the bytes consumed.
pub fn decode<T: Message>(input: &[u8]) -> Result<(T, usize), Error> {
    let (len, header) = read_varint(input)?;
    if len > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge(len));
    }
    let end = usize::try_from(len)
        .ok()
        .and_then(|len| header.checked_add(len))
        .ok_or(Error::FrameTooLarge(len))?;
    let body = input.get(header..end).ok_or(Error::Truncated)?;
    Ok((T::decode_body(body)?, end))
}

/// A message sent in frames, and how its body is decoded.
pub trait Message: DeserializeOwned {
    /// Decodes a frame body: exactly the postcard encoding, unless the message says otherwise.
    fn decode_body(body: &[u8]) -> Result<Self, Error> {
        Ok(postcard::from_bytes(body)?)
    }
}

impl Message for crate::blobs::BlobsMessage {}

impl Message for crate::pair::PairMessage {}

impl Message for crate::sync::SyncMessage {
    /// Also reads a `Hello` from before `clock_ms` (ADR 0030): its frame ends after `folders`,
    /// and it is read with the clock absent. Every other message must be complete.
    fn decode_body(body: &[u8]) -> Result<Self, Error> {
        match postcard::from_bytes(body) {
            Ok(message) => Ok(message),
            Err(e) => {
                let mut padded = body.to_vec();
                padded.push(0);
                match postcard::from_bytes(&padded) {
                    Ok(hello @ Self::Hello { clock_ms: None, .. }) => Ok(hello),
                    _ => Err(Error::Decode(e)),
                }
            }
        }
    }
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "i < 10: shifts stay below 64 bits and i + 1 cannot overflow"
)]
fn read_varint(input: &[u8]) -> Result<(u64, usize), Error> {
    let mut value = 0u64;
    for (i, &byte) in input.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(Error::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Message for (u8, String) {}
    impl Message for Vec<u8> {}

    #[test]
    fn round_trip_consumes_exact_length() {
        let mut buf = encode(&(1u8, String::from("hi")));
        buf.extend_from_slice(&[0xff, 0xff]);
        let ((a, b), used): ((u8, String), usize) = decode(&buf).unwrap();
        assert_eq!((a, b.as_str()), (1, "hi"));
        assert_eq!(used, buf.len() - 2);
    }

    #[test]
    fn truncated_and_oversized() {
        let buf = encode(&vec![0u8; 300]);
        assert!(matches!(
            decode::<Vec<u8>>(&buf[..buf.len() - 1]),
            Err(Error::Truncated)
        ));
        let mut huge = Vec::new();
        write_varint(&mut huge, MAX_FRAME_LEN + 1);
        assert!(matches!(
            decode::<Vec<u8>>(&huge),
            Err(Error::FrameTooLarge(_))
        ));
    }

    #[test]
    fn varint_matches_leb128() {
        let mut out = Vec::new();
        write_varint(&mut out, 300);
        assert_eq!(out, [0xac, 0x02]);
    }
}
