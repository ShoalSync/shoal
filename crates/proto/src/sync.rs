//! Messages of the `tidefell/sync/1` protocol. See `specs/sync-v1.md`.
//!
//! Variant order in [`SyncMessage`] and [`EntryKind`] is part of the wire format: postcard
//! encodes the variant index. Append new variants only in a new protocol version.

use serde::{Deserialize, Serialize};

use crate::identity::DeviceCertificate;
use crate::{
    ContentHash, DeviceId, EntryId, Error, FolderId, Manifest, PublicKey, Signed, SignedKind,
    VersionVector,
};

/// What an entry is in a given version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    /// A regular file.
    File {
        /// Hash of the file's content; identifies its [`Manifest`].
        content: ContentHash,
        /// Content length in bytes.
        size: u64,
        /// Whether the file is executable.
        executable: bool,
    },
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink {
        /// Link target, stored as written.
        target: String,
    },
    /// The entry was deleted.
    Tombstone,
}

/// One version of one entry, authored and signed by a device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryVersion {
    /// The folder the entry belongs to.
    pub folder: FolderId,
    /// The entry's stable identifier.
    pub entry: EntryId,
    /// Path relative to the folder root: `/`-separated, NFC-normalised, no leading `/`,
    /// no `.` or `..` components.
    pub path: String,
    /// What the entry is in this version.
    pub kind: EntryKind,
    /// Modification time, milliseconds since the Unix epoch (UTC).
    pub modified_ms: i64,
    /// The device that made this change.
    pub author: DeviceId,
    /// Version vector after this change.
    pub version: VersionVector,
}

impl SignedKind for EntryVersion {
    const KIND: &'static str = "sync/1/entry-version";

    /// The postcard encoding, except that a vector without holes leaves out the empty list of
    /// holes that would end it, so such a version has the encoding it had before vectors had
    /// holes.
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

    /// Decodes either form, and refuses an empty list of holes written out, so each version has
    /// one encoding.
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

/// An [`EntryVersion`] as encoded before vectors had holes: engine state format 8 and meta
/// snapshot format 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyEntryVersion {
    folder: FolderId,
    entry: EntryId,
    path: String,
    kind: EntryKind,
    modified_ms: i64,
    author: DeviceId,
    version: crate::version::LegacyVersionVector,
}

impl From<LegacyEntryVersion> for EntryVersion {
    fn from(v: LegacyEntryVersion) -> Self {
        Self {
            folder: v.folder,
            entry: v.entry,
            path: v.path,
            kind: v.kind,
            modified_ms: v.modified_ms,
            author: v.author,
            version: v.version.into(),
        }
    }
}

/// Checks the path rules from `specs/sync-v1.md`: relative, `/`-separated, non-empty
/// components, no `.` or `..`, not under [`RESERVED_DIR`], no NUL or backslash.
///
/// NFC normalisation is the author's responsibility and is not checked here.
pub fn is_valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && path.split('/').next() != Some(RESERVED_DIR)
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// The per-folder directory each replica keeps for its own use; no entry may live under it
/// (ADR 0024).
pub const RESERVED_DIR: &str = ".tidefell";

impl Signed<EntryVersion> {
    /// Verifies the signature and checks that the signer is the version's author.
    pub fn verify_authored(&self) -> Result<EntryVersion, Error> {
        let version = self.verify()?;
        if PublicKey::from(version.author) != self.signer {
            return Err(Error::SignerMismatch);
        }
        Ok(version)
    }
}

/// Why a peer rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// The sender is not a member of the folder.
    NotAMember,
    /// The requested folder is unknown.
    UnknownFolder,
    /// The requested manifest is unknown.
    UnknownManifest,
    /// The message was malformed or unexpected.
    ProtocolViolation,
}

/// A message on a `tidefell/sync/1` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMessage {
    /// First message from each side: who the device belongs to, and the folders it offers to
    /// sync with this peer.
    Hello {
        /// The sending device's certificate. Its `device` must be the connection's endpoint ID.
        certificate: Signed<DeviceCertificate>,
        /// Folders the sender offers to sync with this peer.
        folders: Vec<FolderId>,
    },
    /// Ask for every version the responder has recorded after sequence `since` in its index
    /// for `folder`.
    IndexRequest {
        /// Folder to synchronise.
        folder: FolderId,
        /// The responder's index epoch the requester last saw, or 0 if none.
        epoch: u64,
        /// Highest responder sequence the requester has already received. Ignored, and treated
        /// as 0, if `epoch` does not match the responder's current epoch.
        since: u64,
    },
    /// Versions answering an [`SyncMessage::IndexRequest`]. Sent in batches; `last` marks the
    /// end of the response.
    IndexBatch {
        /// Folder the versions belong to.
        folder: FolderId,
        /// The responder's index epoch.
        epoch: u64,
        /// Highest responder sequence covered by this and earlier batches. The requester sends
        /// it as `since` next time.
        up_to: u64,
        /// Signed versions, each authored by `EntryVersion::author`.
        versions: Vec<Signed<EntryVersion>>,
        /// True on the final batch.
        last: bool,
    },
    /// Ask for the manifest of a file's content.
    ManifestRequest {
        /// Folder the content belongs to; used for access control.
        folder: FolderId,
        /// Content to describe.
        content: ContentHash,
    },
    /// Answer to [`SyncMessage::ManifestRequest`].
    ManifestResponse {
        /// The manifest.
        manifest: Manifest,
    },
    /// A request was rejected.
    Error {
        /// Reason.
        code: ErrorCode,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        for ok in [
            "a",
            "a/b.txt",
            "dir/.hidden",
            "a..b",
            "dir/.tidefell",
            ".tidefellx",
        ] {
            assert!(is_valid_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "./a",
            "a/../b",
            "..",
            "a\\b",
            "a\0b",
            ".tidefell",
            ".tidefell/tmp/x",
        ] {
            assert!(!is_valid_path(bad), "{bad:?}");
        }
    }
}
