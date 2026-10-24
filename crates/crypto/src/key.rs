//! Folder keys and what they seal: chunks and versions' payloads (`specs/encryption-v1.md`).

use std::fmt;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use shoal_proto::{ChunkId, DeviceId, EntryId, FolderId, KeyId};

use crate::{pad, random, unpad, Error, Result};

/// The largest chunk plaintext: FastCDC's maximum.
pub const MAX_CHUNK: usize = 1 << 20;

/// Chunks are padded to a multiple of this, hiding their exact length.
const CHUNK_BLOCK: usize = 4096;
/// Payloads are padded to a multiple of this, hiding the length of a path.
const PAYLOAD_BLOCK: usize = 64;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// One generation of an encrypted folder's key, and the keys derived from it. Each generation's
/// subkeys derive from its own key, so content written after a rotation is unlike what came
/// before it (ADR 0023, as amended).
#[derive(Clone)]
pub struct FolderKey {
    key: [u8; 32],
    generation: u64,
    id: KeyId,
    chunk_id_key: [u8; 32],
    chunk_key: [u8; 32],
    chunk_nonce_key: [u8; 32],
    metadata_key: [u8; 32],
}

impl fmt::Debug for FolderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderKey")
            .field("generation", &self.generation)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for FolderKey {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation && self.id == other.id
    }
}

impl Eq for FolderKey {}

impl FolderKey {
    /// The key `key` as generation `generation`.
    pub fn new(key: [u8; 32], generation: u64) -> Self {
        let derive = |context| blake3::derive_key(context, &key);
        let full_id = derive("shoal 2026-10-24 folder key id");
        let mut id = [0u8; KeyId::LEN];
        id.copy_from_slice(&full_id[..KeyId::LEN]);
        Self {
            key,
            generation,
            id: KeyId(id),
            chunk_id_key: derive("shoal 2026-10-24 chunk id"),
            chunk_key: derive("shoal 2026-10-24 chunk key"),
            chunk_nonce_key: derive("shoal 2026-10-24 chunk nonce"),
            metadata_key: derive("shoal 2026-10-24 metadata key"),
        }
    }

    /// A new random key for generation `generation`.
    pub fn generate(generation: u64) -> Result<Self> {
        Ok(Self::new(random()?, generation))
    }

    /// The key's generation, from 1.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The key's ID, derived from it.
    pub fn id(&self) -> KeyId {
        self.id
    }

    /// The key itself, to grant it or keep it.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.key
    }

    /// The ID of a chunk with this plaintext: keyed, so only holders of the key can compute it.
    pub fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        ChunkId(*blake3::keyed_hash(&self.chunk_id_key, plaintext).as_bytes())
    }

    fn chunk_nonce(&self, id: &ChunkId) -> XNonce {
        let hash = blake3::keyed_hash(&self.chunk_nonce_key, id.as_bytes());
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&hash.as_bytes()[..NONCE_LEN]);
        XNonce::from(nonce)
    }

    fn chunk_aad(&self, folder: &FolderId, id: &ChunkId) -> Vec<u8> {
        [
            b"shoal/chunk/1".as_slice(),
            folder.as_bytes(),
            self.id.as_bytes(),
            id.as_bytes(),
        ]
        .concat()
    }

    /// Seals a chunk of `folder`: its ID and the sealed bytes, the same for the same plaintext.
    pub fn seal_chunk(&self, folder: &FolderId, plaintext: &[u8]) -> Result<(ChunkId, Vec<u8>)> {
        if plaintext.is_empty() || plaintext.len() > MAX_CHUNK {
            return Err(Error::TooLarge("chunk"));
        }
        let id = self.chunk_id(plaintext);
        let padded = pad(plaintext, CHUNK_BLOCK);
        let sealed = XChaCha20Poly1305::new(&self.chunk_key.into())
            .encrypt(
                &self.chunk_nonce(&id),
                Payload {
                    msg: &padded,
                    aad: &self.chunk_aad(folder, &id),
                },
            )
            .map_err(|_| Error::Malformed("chunk"))?;
        Ok((id, sealed))
    }

    /// Opens a sealed chunk of `folder` asked for as `id`, and checks it is that chunk.
    pub fn open_chunk(&self, folder: &FolderId, id: &ChunkId, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() > MAX_CHUNK + CHUNK_BLOCK + TAG_LEN {
            return Err(Error::TooLarge("sealed chunk"));
        }
        let padded = XChaCha20Poly1305::new(&self.chunk_key.into())
            .decrypt(
                &self.chunk_nonce(id),
                Payload {
                    msg: sealed,
                    aad: &self.chunk_aad(folder, id),
                },
            )
            .map_err(|_| Error::Open)?;
        let plaintext = unpad(padded, CHUNK_BLOCK)?;
        if plaintext.is_empty() || plaintext.len() > MAX_CHUNK {
            return Err(Error::TooLarge("chunk"));
        }
        if self.chunk_id(&plaintext) != *id {
            return Err(Error::Mismatch("chunk"));
        }
        Ok(plaintext)
    }

    fn payload_aad(&self, folder: &FolderId, entry: &EntryId, author: &DeviceId) -> Vec<u8> {
        [
            b"shoal/version/1".as_slice(),
            folder.as_bytes(),
            entry.as_bytes(),
            author.as_bytes(),
            self.id.as_bytes(),
        ]
        .concat()
    }

    /// Seals a version's payload (the encoded path, kind, time and chunk lengths) with a fresh
    /// random nonce.
    pub fn seal_payload(
        &self,
        folder: &FolderId,
        entry: &EntryId,
        author: &DeviceId,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        self.seal_payload_with(folder, entry, author, payload, random()?)
    }

    /// [`Self::seal_payload`] with the nonce given: for golden vectors only, since a nonce must
    /// never be used twice with one key.
    pub fn seal_payload_with(
        &self,
        folder: &FolderId,
        entry: &EntryId,
        author: &DeviceId,
        payload: &[u8],
        nonce: [u8; NONCE_LEN],
    ) -> Result<Vec<u8>> {
        let sealed = XChaCha20Poly1305::new(&self.metadata_key.into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &pad(payload, PAYLOAD_BLOCK),
                    aad: &self.payload_aad(folder, entry, author),
                },
            )
            .map_err(|_| Error::Malformed("payload"))?;
        Ok([nonce.as_slice(), &sealed].concat())
    }

    /// Opens a version's sealed payload, returning the encoded payload for the caller to decode.
    pub fn open_payload(
        &self,
        folder: &FolderId,
        entry: &EntryId,
        author: &DeviceId,
        sealed: &[u8],
    ) -> Result<Vec<u8>> {
        let (nonce, ciphertext) = sealed
            .split_first_chunk::<NONCE_LEN>()
            .ok_or(Error::Malformed("sealed payload"))?;
        let padded = XChaCha20Poly1305::new(&self.metadata_key.into())
            .decrypt(
                &XNonce::from(*nonce),
                Payload {
                    msg: ciphertext,
                    aad: &self.payload_aad(folder, entry, author),
                },
            )
            .map_err(|_| Error::Open)?;
        unpad(padded, PAYLOAD_BLOCK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER: FolderId = FolderId([0x22; 32]);

    #[test]
    fn chunks_seal_alike_and_open_only_as_themselves() {
        let key = FolderKey::new([7; 32], 1);
        let (id, sealed) = key.seal_chunk(&FOLDER, b"hello").unwrap();
        assert_eq!(
            key.seal_chunk(&FOLDER, b"hello").unwrap(),
            (id, sealed.clone())
        );
        assert_eq!(sealed.len(), 4096 + 16);
        assert_eq!(key.open_chunk(&FOLDER, &id, &sealed).unwrap(), b"hello");

        // Another folder, another key, another generation's key: none opens it.
        assert_eq!(
            key.open_chunk(&FolderId([0x23; 32]), &id, &sealed),
            Err(Error::Open)
        );
        let other = FolderKey::new([8; 32], 2);
        assert_eq!(other.open_chunk(&FOLDER, &id, &sealed), Err(Error::Open));
        assert_ne!(other.chunk_id(b"hello"), id);

        // Asked for as another chunk, it does not open: the nonce and data differ.
        let (other_id, _) = key.seal_chunk(&FOLDER, b"other").unwrap();
        assert_eq!(
            key.open_chunk(&FOLDER, &other_id, &sealed),
            Err(Error::Open)
        );

        let mut changed = sealed;
        changed[0] ^= 1;
        assert_eq!(key.open_chunk(&FOLDER, &id, &changed), Err(Error::Open));
        assert_eq!(key.seal_chunk(&FOLDER, b""), Err(Error::TooLarge("chunk")));
        assert!(key.seal_chunk(&FOLDER, &vec![1; MAX_CHUNK]).is_ok());
        assert_eq!(
            key.seal_chunk(&FOLDER, &vec![1; MAX_CHUNK + 1]),
            Err(Error::TooLarge("chunk"))
        );
    }

    #[test]
    fn payloads_open_only_for_their_version() {
        let key = FolderKey::new([7; 32], 1);
        let (entry, author) = (EntryId([1; 16]), DeviceId([2; 32]));
        let sealed = key
            .seal_payload(&FOLDER, &entry, &author, b"payload")
            .unwrap();
        assert_eq!(sealed.len(), 24 + 64 + 16);
        assert_eq!(
            key.open_payload(&FOLDER, &entry, &author, &sealed).unwrap(),
            b"payload"
        );
        // Random nonces: sealed twice, it differs.
        assert_ne!(
            key.seal_payload(&FOLDER, &entry, &author, b"payload")
                .unwrap(),
            sealed
        );
        assert_eq!(
            key.open_payload(&FOLDER, &EntryId([3; 16]), &author, &sealed),
            Err(Error::Open)
        );
        assert_eq!(
            key.open_payload(&FOLDER, &entry, &DeviceId([4; 32]), &sealed),
            Err(Error::Open)
        );
        assert_eq!(
            key.open_payload(&FOLDER, &entry, &author, &sealed[..20]),
            Err(Error::Malformed("sealed payload"))
        );
    }
}
