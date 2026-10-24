//! Messages of the `shoal/sync/2` protocol, for encrypted folders. See `specs/sync-v2.md`;
//! the sealing itself is in `shoal-crypto` (`specs/encryption-v1.md`).
//!
//! Variant order is part of the wire format, as in [`crate::sync`].

use serde::{Deserialize, Serialize};

use crate::identity::{DeviceCertificate, DeviceRevocation};
use crate::sync::{is_valid_path, EntryKind, EntryVersion};
use crate::{
    ChunkId, DeviceId, EntryId, Error, FolderId, KeyId, ListId, PublicKey, Signed, SignedKind,
    VersionVector,
};

/// The most chunk IDs a version lists itself, and in one list blob.
pub const MAX_INLINE_CHUNKS: usize = 4096;
/// The fewest and most list blobs a version names, when it uses them.
pub const LIST_BLOBS: std::ops::RangeInclusive<usize> = 2..=256;
/// The largest encoded signed sealed version.
pub const MAX_SEALED_VERSION: usize = 1 << 20;
/// The shortest sealed payload: its nonce, one 64-byte block and the tag.
pub const MIN_SEALED_PAYLOAD: usize = 24 + 64 + 16;
/// The largest chunk plaintext.
pub const MAX_CHUNK_LEN: u32 = 1 << 20;
/// The most items in one [`SyncMessage::IndexBatch`], and its largest encoding.
pub const MAX_BATCH_ITEMS: usize = 512;
pub const MAX_BATCH_BYTES: usize = 4 << 20;
/// The most revocations read from one [`SyncMessage::Hello`].
pub const MAX_HELLO_REVOCATIONS: usize = 256;

/// A file's chunk IDs, in the clear so a storage peer knows what to keep.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChunkList {
    /// Up to [`MAX_INLINE_CHUNKS`] chunk IDs; none for anything but a file.
    Inline(Vec<ChunkId>),
    /// The list blobs holding them, for files of more than [`MAX_INLINE_CHUNKS`] chunks.
    Lists(Vec<ListId>),
}

/// One version of one entry of an encrypted folder: what anyone may see, and the sealed rest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SealedVersion {
    pub folder: FolderId,
    pub entry: EntryId,
    /// The device that made this change; must sign it.
    pub author: DeviceId,
    /// The generation of the folder key it is sealed with, from 1.
    pub generation: u64,
    /// The folder key it is sealed with.
    pub key: KeyId,
    pub chunks: ChunkList,
    /// The sealed [`SealedPayload`].
    pub sealed: Vec<u8>,
    /// Version vector after this change; encoded last, as in [`EntryVersion`].
    pub version: VersionVector,
}

impl SignedKind for SealedVersion {
    const KIND: &'static str = "sync/2/sealed-version";

    /// As [`EntryVersion`]: a vector without holes leaves out the empty list that would end it.
    #[expect(
        clippy::expect_used,
        reason = "postcard fails to encode only sequences of unknown length, which no message has"
    )]
    fn encode(&self) -> Vec<u8> {
        let mut body = postcard::to_stdvec(self).expect("in-memory encoding cannot fail");
        if !self.version.has_holes() {
            assert_eq!(
                body.pop(),
                Some(0),
                "the empty list of holes ends the encoding"
            );
        }
        body
    }

    fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        match postcard::take_from_bytes::<Self>(body) {
            Ok((version, rest)) if version.version.has_holes() && rest.is_empty() => Ok(version),
            Ok(_) => Err(postcard::Error::DeserializeBadEncoding),
            Err(_) => {
                let mut full = body.to_vec();
                full.push(0);
                let (version, rest) = postcard::take_from_bytes::<Self>(&full)?;
                if rest.is_empty() {
                    Ok(version)
                } else {
                    Err(postcard::Error::DeserializeBadEncoding)
                }
            }
        }
    }
}

/// A list blob with its list ID.
pub type ListBlob = (ListId, Vec<u8>);

/// A file's chunk IDs as a version lists them (`specs/encryption-v1.md`, "Chunk lists"): inline
/// up to [`MAX_INLINE_CHUNKS`], otherwise in list blobs of [`MAX_INLINE_CHUNKS`] each, the last
/// holding the rest, returned with their bytes. `None` for a file of more chunks than
/// [`LIST_BLOBS`] lists hold.
pub fn chunk_list(ids: &[ChunkId]) -> Option<(ChunkList, Vec<ListBlob>)> {
    if ids.len() <= MAX_INLINE_CHUNKS {
        return Some((ChunkList::Inline(ids.to_vec()), Vec::new()));
    }
    let blobs: Vec<ListBlob> = ids
        .chunks(MAX_INLINE_CHUNKS)
        .map(|list| {
            let bytes = postcard::to_stdvec(list).unwrap_or_default();
            (ListId(*blake3::hash(&bytes).as_bytes()), bytes)
        })
        .collect();
    if !LIST_BLOBS.contains(&blobs.len()) {
        return None;
    }
    let lists = blobs.iter().map(|(id, _)| *id).collect();
    Some((ChunkList::Lists(lists), blobs))
}

/// The chunk IDs in list blob `bytes`, if it is the blob `id` names and well formed.
pub fn read_list_blob(id: &ListId, bytes: &[u8]) -> Option<Vec<ChunkId>> {
    if blake3::hash(bytes).as_bytes() != &id.0 {
        return None;
    }
    let ids: Vec<ChunkId> = postcard::from_bytes(bytes).ok()?;
    (!ids.is_empty() && ids.len() <= MAX_INLINE_CHUNKS).then_some(ids)
}

impl SealedVersion {
    /// Checks what anyone can check without the key: the bounds of `specs/sync-v2.md`.
    pub fn check_bounds(&self) -> Result<(), Error> {
        let chunks_ok = match &self.chunks {
            ChunkList::Inline(ids) => ids.len() <= MAX_INLINE_CHUNKS,
            ChunkList::Lists(ids) => LIST_BLOBS.contains(&ids.len()),
        };
        if self.generation == 0 || !chunks_ok || self.sealed.len() < MIN_SEALED_PAYLOAD {
            return Err(Error::Invalid("sealed version out of bounds"));
        }
        Ok(())
    }
}

impl Signed<SealedVersion> {
    /// Verifies the signature, that the signer is the author, and the bounds.
    pub fn verify_authored(&self) -> Result<SealedVersion, Error> {
        if self.body.len().saturating_add(96) > MAX_SEALED_VERSION {
            return Err(Error::Invalid("sealed version too large"));
        }
        let version = self.verify()?;
        if PublicKey::from(version.author) != self.signer {
            return Err(Error::SignerMismatch);
        }
        version.check_bounds()?;
        Ok(version)
    }
}

/// What a sealed version seals: what a member writes to disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SealedPayload {
    pub path: String,
    pub kind: EntryKind,
    pub modified_ms: i64,
    /// For a file, the plaintext length of each chunk, in the order of the version's chunk IDs.
    pub chunk_lens: Vec<u32>,
}

impl SealedPayload {
    /// Decodes an opened payload, which must use every byte.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        match postcard::take_from_bytes::<Self>(bytes) {
            Ok((payload, [])) => Ok(payload),
            _ => Err(Error::Invalid("malformed sealed payload")),
        }
    }

    /// Encodes it for sealing.
    #[expect(
        clippy::expect_used,
        reason = "postcard fails to encode only sequences of unknown length, which no message has"
    )]
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("in-memory encoding cannot fail")
    }

    /// The decrypted version, once the payload is checked against the sealed version it came in
    /// and `chunk_count`, the number of chunk IDs the version names, lists expanded.
    pub fn into_version(
        self,
        sealed: &SealedVersion,
        chunk_count: usize,
    ) -> Result<EntryVersion, Error> {
        if !is_valid_path(&self.path) {
            return Err(Error::Invalid("sealed payload has an invalid path"));
        }
        let consistent = match &self.kind {
            EntryKind::File { size, .. } => {
                self.chunk_lens.len() == chunk_count
                    && self
                        .chunk_lens
                        .iter()
                        .all(|len| (1..=MAX_CHUNK_LEN).contains(len))
                    && self
                        .chunk_lens
                        .iter()
                        .map(|len| u64::from(*len))
                        .sum::<u64>()
                        == *size
            }
            _ => self.chunk_lens.is_empty() && chunk_count == 0,
        };
        if !consistent {
            return Err(Error::Invalid("sealed payload does not match its chunks"));
        }
        Ok(EntryVersion {
            folder: sealed.folder,
            entry: sealed.entry,
            path: self.path,
            kind: self.kind,
            modified_ms: self.modified_ms,
            author: sealed.author,
            version: sealed.version.clone(),
        })
    }
}

/// A device's X25519 key for receiving folder keys, signed by the device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EncryptionKey {
    pub device: DeviceId,
    #[serde(with = "crate::bytes")]
    pub key: [u8; 32],
    pub issued_ms: i64,
}

impl SignedKind for EncryptionKey {
    const KIND: &'static str = "sync/2/encryption-key";
}

impl Signed<EncryptionKey> {
    /// Verifies the signature and that the device signed its own key.
    pub fn verify_own(&self) -> Result<EncryptionKey, Error> {
        let key = self.verify()?;
        if PublicKey::from(key.device) != self.signer {
            return Err(Error::SignerMismatch);
        }
        Ok(key)
    }
}

/// One folder key, sealed to one device's encryption key; signed by the granting device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyGrant {
    pub folder: FolderId,
    pub generation: u64,
    pub key: KeyId,
    pub recipient: DeviceId,
    #[serde(with = "crate::bytes")]
    pub recipient_key: [u8; 32],
    /// HPKE's encapsulated key.
    #[serde(with = "crate::bytes")]
    pub enc: [u8; 32],
    /// The sealed folder key: the key and the tag.
    pub sealed: Vec<u8>,
}

impl SignedKind for KeyGrant {
    const KIND: &'static str = "sync/2/key-grant";
}

/// An entry in an encrypted folder's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    Version(Signed<SealedVersion>),
    EncryptionKey(Signed<EncryptionKey>),
    Grant(Signed<KeyGrant>),
}

/// Why a peer rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    NotAMember,
    UnknownFolder,
    ProtocolViolation,
    UnknownDevice,
}

/// A message on a `shoal/sync/2` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMessage {
    /// As sync/1's, with every field present.
    Hello {
        certificate: Signed<DeviceCertificate>,
        folders: Vec<FolderId>,
        clock_ms: i64,
        revocations: Vec<Signed<DeviceRevocation>>,
    },
    IndexRequest {
        folder: FolderId,
        epoch: u64,
        since: u64,
    },
    /// Items answering an index request: at most [`MAX_BATCH_ITEMS`] and [`MAX_BATCH_BYTES`].
    IndexBatch {
        folder: FolderId,
        epoch: u64,
        up_to: u64,
        items: Vec<Item>,
        last: bool,
    },
    CertificateRequest {
        folder: FolderId,
        device: DeviceId,
    },
    CertificateResponse {
        certificate: Signed<DeviceCertificate>,
    },
    Error {
        code: ErrorCode,
    },
}

#[cfg(test)]
mod list_tests {
    use super::*;

    #[test]
    fn files_of_more_than_4096_chunks_list_them_in_list_blobs() {
        let ids: Vec<ChunkId> = (0..5000u32)
            .map(|n| {
                let mut id = [0; 32];
                id[..4].copy_from_slice(&n.to_le_bytes());
                ChunkId(id)
            })
            .collect();
        let (inline, none) = chunk_list(&ids[..4096]).unwrap();
        assert!(matches!(inline, ChunkList::Inline(ref v) if v.len() == 4096));
        assert!(none.is_empty());
        let (list, blobs) = chunk_list(&ids).unwrap();
        let ChunkList::Lists(lists) = list else {
            panic!("not lists")
        };
        assert_eq!(lists.len(), 2);
        let read: Vec<ChunkId> = blobs
            .iter()
            .flat_map(|(id, bytes)| read_list_blob(id, bytes).unwrap())
            .collect();
        assert_eq!(read, ids);
        // Every list but the last holds exactly 4096, and a blob must match its ID.
        assert_eq!(read_list_blob(&lists[0], &blobs[0].1).unwrap().len(), 4096);
        assert!(read_list_blob(&lists[1], &blobs[0].1).is_none());
        assert!(blobs
            .iter()
            .all(|(_, b)| b.len() <= crate::blobs2::MAX_LIST_BLOB));
    }
}
