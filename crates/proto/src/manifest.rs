//! File content manifests: the ordered chunks that make up one version of a file.

use serde::{Deserialize, Serialize};

use crate::{ChunkHash, ContentHash};

/// One chunk of a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkRef {
    /// BLAKE3 hash of the chunk's bytes.
    pub hash: ChunkHash,
    /// Length of the chunk in bytes.
    pub len: u32,
}

/// The chunks of a file, in order. `content` is the BLAKE3 hash of the concatenated chunks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Manifest {
    /// BLAKE3 hash of the full content.
    pub content: ContentHash,
    /// The chunks in file order.
    pub chunks: Vec<ChunkRef>,
}

impl Manifest {
    /// Total content length in bytes.
    pub fn len(&self) -> u64 {
        self.chunks.iter().map(|c| u64::from(c.len)).sum()
    }

    /// True if the file is empty.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}
