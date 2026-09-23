//! Messages of the `tidefell/sync/1` protocol. See `specs/sync-v1.md`.
//!
//! Variant order in [`SyncMessage`] and [`EntryKind`] is part of the wire format: postcard
//! encodes the variant index. Append new variants only in a new protocol version.

use serde::{Deserialize, Serialize};

use crate::{
    ContentHash, DeviceId, EntryId, Error, FolderId, Manifest, Signed, SignedKind, VersionVector,
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
}

impl Signed<EntryVersion> {
    /// Verifies the signature and checks that the signer is the version's author.
    pub fn verify_authored(&self) -> Result<EntryVersion, Error> {
        let version = self.verify()?;
        if version.author != self.signer {
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
    /// First message from each side: the folders this device shares with the peer.
    Hello {
        /// The sending device.
        device: DeviceId,
        /// Folders both devices may sync.
        folders: Vec<FolderId>,
    },
    /// Ask for every version of `folder` not dominated by `since`.
    IndexRequest {
        /// Folder to synchronise.
        folder: FolderId,
        /// The requester's summary vector for the folder.
        since: VersionVector,
    },
    /// Versions answering an [`SyncMessage::IndexRequest`]. Sent in batches; `last` marks the
    /// end of the response.
    IndexBatch {
        /// Folder the versions belong to.
        folder: FolderId,
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
