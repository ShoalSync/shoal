//! Tidefell `chunker` crate: FastCDC content-defined chunking, BLAKE3 hashing, chunk index.
//!
//! Content is split with FastCDC (2020), so an insertion in the middle of a file only changes
//! the chunks around it. Every chunk is identified by its BLAKE3 hash, and the whole file by the
//! BLAKE3 hash of its content. The result is a [`Manifest`].

mod index;

use std::io::{self, Read};

use fastcdc::v2020::StreamCDC;
use tidefell_proto::{ChunkHash, ChunkRef, ContentHash, Manifest};

pub use index::ChunkIndex;

/// FastCDC size parameters, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Smallest chunk, except for the last chunk of a file.
    pub min: u32,
    /// Target average chunk size.
    pub avg: u32,
    /// Largest chunk.
    pub max: u32,
}

impl Params {
    /// The reference parameters from `specs/sync-v1.md`: 64 KiB / 256 KiB / 1 MiB.
    pub const DEFAULT: Params = Params {
        min: 64 * 1024,
        avg: 256 * 1024,
        max: 1024 * 1024,
    };
}

impl Default for Params {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Hashes one chunk.
pub fn hash_chunk(data: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(data).as_bytes())
}

/// Hashes a file's full content.
pub fn hash_content(data: &[u8]) -> ContentHash {
    ContentHash(*blake3::hash(data).as_bytes())
}

/// Splits everything `reader` yields into chunks, calling `sink` with each chunk's reference
/// and bytes in order, and returns the manifest.
///
/// Memory use is bounded by `params.max`; the content is never held in full.
pub fn chunk_reader<R, F>(reader: R, params: Params, mut sink: F) -> io::Result<Manifest>
where
    R: Read,
    F: FnMut(ChunkRef, &[u8]) -> io::Result<()>,
{
    let mut content = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let cdc = StreamCDC::new(
        reader,
        params.min as usize,
        params.avg as usize,
        params.max as usize,
    );
    for chunk in cdc {
        let chunk = chunk?;
        content.update(&chunk.data);
        let chunk_ref = ChunkRef {
            hash: hash_chunk(&chunk.data),
            len: u32::try_from(chunk.length).expect("chunk length is bounded by params.max"),
        };
        sink(chunk_ref, &chunk.data)?;
        chunks.push(chunk_ref);
    }
    Ok(Manifest {
        content: ContentHash(*content.finalize().as_bytes()),
        chunks,
    })
}

/// Chunks an in-memory buffer and returns only the manifest.
pub fn manifest_of(data: &[u8], params: Params) -> Manifest {
    chunk_reader(data, params, |_, _| Ok(())).expect("reading from a slice cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small parameters so tests exercise many chunks without large inputs.
    const SMALL: Params = Params {
        min: 256,
        avg: 1024,
        max: 4096,
    };

    /// Deterministic pseudo-random bytes (xorshift).
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn empty_input_has_no_chunks() {
        let m = manifest_of(&[], SMALL);
        assert!(m.is_empty());
        assert_eq!(m.content, hash_content(&[]));
    }

    #[test]
    fn chunks_reassemble_to_input() {
        let data = noise(50_000, 1);
        let mut out = Vec::new();
        let m = chunk_reader(&data[..], SMALL, |r, bytes| {
            assert_eq!(r.hash, hash_chunk(bytes));
            assert_eq!(r.len as usize, bytes.len());
            out.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, data);
        assert_eq!(m.len(), data.len() as u64);
        assert_eq!(m.content, hash_content(&data));
        assert!(m.chunks.len() > 5);
        let last = m.chunks.len() - 1;
        for c in &m.chunks[..last] {
            assert!((SMALL.min..=SMALL.max).contains(&c.len));
        }
    }

    #[test]
    fn insertion_only_changes_nearby_chunks() {
        let data = noise(200_000, 2);
        let mut edited = data.clone();
        edited.splice(100_000..100_000, b"inserted bytes".iter().copied());

        let before = manifest_of(&data, SMALL);
        let after = manifest_of(&edited, SMALL);
        let shared = after
            .chunks
            .iter()
            .filter(|c| before.chunks.contains(c))
            .count();
        assert!(
            shared + 3 >= after.chunks.len(),
            "only chunks around the edit should change: {shared} of {} shared",
            after.chunks.len()
        );
    }

    #[test]
    fn deterministic() {
        let data = noise(30_000, 3);
        assert_eq!(manifest_of(&data, SMALL), manifest_of(&data, SMALL));
    }

    #[test]
    fn default_params_are_valid() {
        let data = noise(3 * 1024 * 1024, 4);
        let m = manifest_of(&data, Params::DEFAULT);
        assert!(m.chunks.len() >= 3);
        assert_eq!(m.len(), data.len() as u64);
    }
}
