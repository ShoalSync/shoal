//! Stream framing: each message is a varint (LEB128) length followed by that many bytes of
//! postcard encoding.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Error;

/// Largest frame a peer accepts, in bytes.
pub const MAX_FRAME_LEN: u64 = 16 * 1024 * 1024;

/// Encodes `message` as one frame.
pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(message).expect("in-memory encoding cannot fail");
    let mut out = Vec::with_capacity(body.len() + 4);
    write_varint(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out
}

/// Decodes one frame from the start of `input`. Returns the message and the bytes consumed.
pub fn decode<T: DeserializeOwned>(input: &[u8]) -> Result<(T, usize), Error> {
    let (len, header) = read_varint(input)?;
    if len > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge(len));
    }
    let end = header + len as usize;
    let body = input.get(header..end).ok_or(Error::Truncated)?;
    Ok((postcard::from_bytes(body)?, end))
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
