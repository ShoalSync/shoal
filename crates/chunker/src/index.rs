//! Chunk index: which file contents reference which chunks.
//!
//! Used to know when a chunk is no longer needed by any content, and to find content that can
//! be served from chunks already stored.

use std::collections::{BTreeSet, HashMap};

use shoal_proto::{ChunkHash, ContentHash, Manifest};

/// Maps chunks to the contents that contain them.
#[derive(Debug, Default, Clone)]
pub struct ChunkIndex {
    chunks: HashMap<ChunkHash, BTreeSet<ContentHash>>,
    contents: HashMap<ContentHash, Vec<ChunkHash>>,
}

impl ChunkIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a manifest. Returns false if the content was already indexed.
    pub fn insert(&mut self, manifest: &Manifest) -> bool {
        if self.contents.contains_key(&manifest.content) {
            return false;
        }
        let hashes: Vec<ChunkHash> = manifest.chunks.iter().map(|c| c.hash).collect();
        for hash in &hashes {
            self.chunks
                .entry(*hash)
                .or_default()
                .insert(manifest.content);
        }
        self.contents.insert(manifest.content, hashes);
        true
    }

    /// Forgets a content. Returns the chunks that no remaining content references.
    pub fn remove(&mut self, content: &ContentHash) -> Vec<ChunkHash> {
        let Some(hashes) = self.contents.remove(content) else {
            return Vec::new();
        };
        let mut orphaned = Vec::new();
        for hash in hashes {
            if let Some(users) = self.chunks.get_mut(&hash) {
                users.remove(content);
                if users.is_empty() {
                    self.chunks.remove(&hash);
                    orphaned.push(hash);
                }
            }
        }
        orphaned
    }

    /// Contents that contain `chunk`.
    pub fn contents_with(&self, chunk: &ChunkHash) -> impl Iterator<Item = &ContentHash> {
        self.chunks.get(chunk).into_iter().flatten()
    }

    /// True if any indexed content references `chunk`.
    pub fn is_referenced(&self, chunk: &ChunkHash) -> bool {
        self.chunks.contains_key(chunk)
    }

    /// Number of distinct chunks referenced.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Number of contents indexed.
    pub fn content_count(&self) -> usize {
        self.contents.len()
    }
}

#[cfg(test)]
mod tests {
    use shoal_proto::ChunkRef;

    use super::*;

    fn manifest(content: u8, chunks: &[u8]) -> Manifest {
        Manifest {
            content: ContentHash([content; 32]),
            chunks: chunks
                .iter()
                .map(|&c| ChunkRef {
                    hash: ChunkHash([c; 32]),
                    len: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn shared_chunks_survive_until_last_reference() {
        let mut index = ChunkIndex::new();
        assert!(index.insert(&manifest(1, &[10, 11])));
        assert!(index.insert(&manifest(2, &[11, 12])));
        assert!(!index.insert(&manifest(2, &[11, 12])));
        assert_eq!(index.chunk_count(), 3);
        assert_eq!(index.contents_with(&ChunkHash([11; 32])).count(), 2);

        assert_eq!(
            index.remove(&ContentHash([1; 32])),
            vec![ChunkHash([10; 32])]
        );
        assert!(index.is_referenced(&ChunkHash([11; 32])));

        let mut orphaned = index.remove(&ContentHash([2; 32]));
        orphaned.sort();
        assert_eq!(orphaned, vec![ChunkHash([11; 32]), ChunkHash([12; 32])]);
        assert_eq!(index.content_count(), 0);
    }

    #[test]
    fn repeated_chunk_within_one_file() {
        let mut index = ChunkIndex::new();
        index.insert(&manifest(1, &[10, 10, 10]));
        assert_eq!(
            index.remove(&ContentHash([1; 32])),
            vec![ChunkHash([10; 32])]
        );
    }
}
