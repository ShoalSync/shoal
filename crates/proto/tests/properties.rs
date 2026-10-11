//! Property tests (ADR 0031): every wire message and signed envelope survives an encode and
//! decode round trip, and decoding arbitrary bytes returns an error or a value, never a panic.

use std::net::SocketAddr;
use std::str::FromStr;

use ed25519_dalek::SigningKey;
use proptest::prelude::*;
use tidefell_proto::blobs::{BlobsErrorCode, BlobsMessage};
use tidefell_proto::frame;
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::pair::{FolderOffer, PairErrorCode, PairMessage, PairingCode};
use tidefell_proto::sync::{is_valid_path, EntryKind, EntryVersion, ErrorCode, SyncMessage};
use tidefell_proto::{
    ChunkHash, ChunkRef, ContentHash, DeviceId, EntryId, FolderId, Manifest, Signed, SignedKind,
    UserId, VersionVector,
};

fn bytes32() -> impl Strategy<Value = [u8; 32]> {
    any::<[u8; 32]>()
}

fn text() -> impl Strategy<Value = String> {
    "\\PC{0,24}"
}

fn key() -> impl Strategy<Value = SigningKey> {
    bytes32().prop_map(|b| SigningKey::from_bytes(&b))
}

fn device() -> impl Strategy<Value = DeviceId> {
    (0u8..4).prop_map(|n| DeviceId([n; 32]))
}

/// Vectors over a few devices, some with holes: changes removed below the highest seen.
fn vector() -> impl Strategy<Value = VersionVector> {
    (
        prop::collection::vec((device(), 1u64..40), 0..4),
        prop::collection::vec((device(), 1u64..40), 0..4),
    )
        .prop_map(|(counters, removed)| {
            let mut v: VersionVector = counters.into_iter().collect();
            for (d, n) in removed {
                if n < v.get(&d) {
                    v.remove(d, n);
                }
            }
            v
        })
}

fn entry_kind() -> impl Strategy<Value = EntryKind> {
    prop_oneof![
        (bytes32(), any::<u64>(), any::<bool>()).prop_map(|(c, size, executable)| {
            EntryKind::File {
                content: ContentHash(c),
                size,
                executable,
            }
        }),
        Just(EntryKind::Directory),
        text().prop_map(|target| EntryKind::Symlink { target }),
        Just(EntryKind::Tombstone),
    ]
}

fn entry_version() -> impl Strategy<Value = EntryVersion> {
    (
        bytes32(),
        any::<[u8; 16]>(),
        text(),
        entry_kind(),
        any::<i64>(),
        device(),
        vector(),
    )
        .prop_map(
            |(folder, entry, path, kind, modified_ms, author, version)| EntryVersion {
                folder: FolderId(folder),
                entry: EntryId(entry),
                path,
                kind,
                modified_ms,
                author,
                version,
            },
        )
}

fn certificate() -> impl Strategy<Value = Signed<DeviceCertificate>> {
    (key(), bytes32(), text(), any::<i64>()).prop_map(|(user, device, name, issued_ms)| {
        let certificate = DeviceCertificate {
            user: UserId(user.verifying_key().to_bytes()),
            device: DeviceId(device),
            name,
            issued_ms,
        };
        Signed::sign(&certificate, &user)
    })
}

fn manifest() -> impl Strategy<Value = Manifest> {
    (
        bytes32(),
        prop::collection::vec((bytes32(), any::<u32>()), 0..4),
    )
        .prop_map(|(content, chunks)| Manifest {
            content: ContentHash(content),
            chunks: chunks
                .into_iter()
                .map(|(hash, len)| ChunkRef {
                    hash: ChunkHash(hash),
                    len,
                })
                .collect(),
        })
}

fn sync_message() -> impl Strategy<Value = SyncMessage> {
    prop_oneof![
        (
            certificate(),
            prop::collection::vec(bytes32().prop_map(FolderId), 0..3),
            any::<Option<i64>>()
        )
            .prop_map(|(certificate, folders, clock_ms)| SyncMessage::Hello {
                certificate,
                folders,
                clock_ms,
            }),
        (bytes32(), any::<u64>(), any::<u64>()).prop_map(|(f, epoch, since)| {
            SyncMessage::IndexRequest {
                folder: FolderId(f),
                epoch,
                since,
            }
        }),
        (
            bytes32(),
            any::<u64>(),
            any::<u64>(),
            prop::collection::vec((entry_version(), key()), 0..3),
            any::<bool>()
        )
            .prop_map(
                |(f, epoch, up_to, versions, last)| SyncMessage::IndexBatch {
                    folder: FolderId(f),
                    epoch,
                    up_to,
                    versions: versions.iter().map(|(v, k)| Signed::sign(v, k)).collect(),
                    last,
                }
            ),
        (bytes32(), bytes32()).prop_map(|(f, c)| SyncMessage::ManifestRequest {
            folder: FolderId(f),
            content: ContentHash(c),
        }),
        manifest().prop_map(|manifest| SyncMessage::ManifestResponse { manifest }),
        prop_oneof![
            Just(ErrorCode::NotAMember),
            Just(ErrorCode::UnknownFolder),
            Just(ErrorCode::UnknownManifest),
            Just(ErrorCode::ProtocolViolation),
        ]
        .prop_map(|code| SyncMessage::Error { code }),
    ]
}

fn blobs_message() -> impl Strategy<Value = BlobsMessage> {
    prop_oneof![
        certificate().prop_map(|certificate| BlobsMessage::Hello { certificate }),
        (
            bytes32(),
            prop::collection::vec(bytes32().prop_map(ChunkHash), 0..4)
        )
            .prop_map(|(f, chunks)| BlobsMessage::Request {
                folder: FolderId(f),
                chunks,
            }),
        (bytes32(), prop::collection::vec(any::<u8>(), 0..64)).prop_map(|(h, data)| {
            BlobsMessage::Chunk {
                hash: ChunkHash(h),
                data,
            }
        }),
        bytes32().prop_map(|h| BlobsMessage::Missing { hash: ChunkHash(h) }),
        (bytes32(), bytes32()).prop_map(|(f, c)| BlobsMessage::AvailabilityRequest {
            folder: FolderId(f),
            content: ContentHash(c),
        }),
        (
            bytes32(),
            any::<u32>(),
            prop::collection::vec(any::<u8>(), 0..8)
        )
            .prop_map(|(c, chunk_count, bitmap)| BlobsMessage::Availability {
                content: ContentHash(c),
                chunk_count,
                bitmap,
            }),
        prop_oneof![
            Just(BlobsErrorCode::NotAMember),
            Just(BlobsErrorCode::UnknownFolder),
            Just(BlobsErrorCode::TooManyChunks),
            Just(BlobsErrorCode::ProtocolViolation),
        ]
        .prop_map(|code| BlobsMessage::Error { code }),
        bytes32().prop_map(|f| BlobsMessage::ReplicaRequest {
            folder: FolderId(f)
        }),
        any::<bool>().prop_map(|full| BlobsMessage::Replica { full }),
        any::<u32>().prop_map(|retry_after_ms| BlobsMessage::Busy { retry_after_ms }),
    ]
}

/// Addresses as the wire carries them: serde leaves out an IPv6 address's flow information and
/// scope, so a link-local address arrives without its interface.
fn addrs() -> impl Strategy<Value = Vec<SocketAddr>> {
    let addr = any::<SocketAddr>().prop_map(|mut addr| {
        if let SocketAddr::V6(v6) = &mut addr {
            v6.set_flowinfo(0);
            v6.set_scope_id(0);
        }
        addr
    });
    prop::collection::vec(addr, 0..3)
}

fn pair_message() -> impl Strategy<Value = PairMessage> {
    let offer = (
        bytes32(),
        text(),
        prop::collection::vec(bytes32(), 0..3),
        any::<u32>(),
    )
        .prop_map(|(id, name, members, retention_days)| FolderOffer {
            id: FolderId(id),
            name,
            members: members.into_iter().map(UserId).collect(),
            retention_days,
        });
    prop_oneof![
        (certificate(), bytes32())
            .prop_map(|(approver, proof)| PairMessage::Request { approver, proof }),
        text().prop_map(|name| PairMessage::Accept { name }),
        (
            certificate(),
            proptest::option::of(text()),
            addrs(),
            prop::collection::vec(offer, 0..3)
        )
            .prop_map(
                |(certificate, relay, addrs, folders)| PairMessage::Complete {
                    certificate,
                    relay,
                    addrs,
                    folders,
                }
            ),
        prop_oneof![
            Just(PairErrorCode::BadProof),
            Just(PairErrorCode::Expired),
            Just(PairErrorCode::Declined),
            Just(PairErrorCode::BadCertificate),
            Just(PairErrorCode::ProtocolViolation),
        ]
        .prop_map(|code| PairMessage::Reject { code }),
    ]
}

/// Encodes `message` as a frame and decodes it back.
fn framed<T: frame::Message + serde::Serialize + PartialEq + std::fmt::Debug>(message: &T) {
    let bytes = frame::encode(message);
    let (decoded, used) = frame::decode::<T>(&bytes).expect("a frame just encoded decodes");
    assert_eq!(&decoded, message);
    assert_eq!(used, bytes.len());
}

proptest! {
    #[test]
    fn sync_messages_round_trip(message in sync_message()) {
        framed(&message);
    }

    #[test]
    fn blobs_messages_round_trip(message in blobs_message()) {
        framed(&message);
    }

    #[test]
    fn pair_messages_round_trip(message in pair_message()) {
        framed(&message);
    }

    #[test]
    fn signed_versions_round_trip(version in entry_version(), key in key()) {
        let signed = Signed::sign(&version, &key);
        prop_assert_eq!(signed.verify().unwrap(), version.clone());
        // The signed body is the one encoding of the version.
        prop_assert_eq!(EntryVersion::decode(&signed.body).unwrap().encode(), signed.body.clone());
        let bytes = postcard::to_stdvec(&signed).unwrap();
        let back: Signed<EntryVersion> = postcard::from_bytes(&bytes).unwrap();
        prop_assert_eq!(back.verify().unwrap(), version);
    }

    #[test]
    fn certificates_round_trip(signed in certificate()) {
        let certificate = signed.verify_certificate().unwrap();
        prop_assert_eq!(Signed::<DeviceCertificate>::verify(&signed).unwrap(), certificate);
    }

    #[test]
    fn pairing_codes_round_trip(
        device in bytes32(),
        secret in any::<[u8; 16]>(),
        relay in proptest::option::of(text()),
        addrs in addrs(),
    ) {
        let code = PairingCode { device: DeviceId(device), secret, relay, addrs };
        prop_assert_eq!(PairingCode::from_text(&code.to_text()).unwrap(), code);
    }

    #[test]
    fn ids_round_trip(bytes in bytes32()) {
        let id = FolderId(bytes);
        prop_assert_eq!(FolderId::from_str(&id.to_string()).unwrap(), id);
    }

    /// Comparing vectors is a partial order: reflexive, each comparison the reverse of the
    /// other way round, and transitive.
    #[test]
    fn vectors_are_partially_ordered(a in vector(), b in vector(), c in vector()) {
        use tidefell_proto::Causality::{After, Before, Concurrent, Equal};
        let flip = |x| match x {
            Before => After,
            After => Before,
            other => other,
        };
        prop_assert_eq!(a.compare(&a), Equal);
        prop_assert_eq!(a.compare(&b), flip(b.compare(&a)));
        prop_assert_eq!(a.compare(&b) == Equal, a == b);
        let at_most = |x: &VersionVector, y: &VersionVector| matches!(x.compare(y), Before | Equal);
        if at_most(&a, &b) && at_most(&b, &c) {
            prop_assert!(at_most(&a, &c));
        }
        // A merge is at least both sides.
        let mut merged = a.clone();
        merged.merge(&b);
        prop_assert!(at_most(&a, &merged) && at_most(&b, &merged));
        let _ = Concurrent;
    }

    /// Whatever arrives from a peer decodes to an error or a value, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = frame::decode::<SyncMessage>(&bytes);
        let _ = frame::decode::<BlobsMessage>(&bytes);
        let _ = frame::decode::<PairMessage>(&bytes);
        let _ = EntryVersion::decode(&bytes);
        let _ = DeviceCertificate::decode(&bytes);
        if let Ok(signed) = postcard::from_bytes::<Signed<EntryVersion>>(&bytes) {
            let _ = signed.verify();
            let _ = signed.verify_authored();
        }
        if let Ok(signed) = postcard::from_bytes::<Signed<DeviceCertificate>>(&bytes) {
            let _ = signed.verify_certificate();
        }
        if let Ok(version) = postcard::from_bytes::<VersionVector>(&bytes) {
            let _ = version.within_ceiling();
            let mut merged = VersionVector::new();
            merged.merge(&version);
            let _ = version.compare(&merged);
        }
    }

    /// Text a user or peer types or sends parses to an error or a value, never a panic.
    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,200}") {
        let _ = PairingCode::from_text(&text);
        let _ = FolderId::from_str(&text);
        let _ = DeviceId::from_str(&text);
        let _ = is_valid_path(&text);
    }

    /// An accepted path never leaves the folder: no empty, `.` or `..` component, no absolute
    /// path, no backslash.
    #[test]
    fn accepted_paths_stay_in_the_folder(path in "[a-z./\\\\]{0,12}") {
        if is_valid_path(&path) {
            prop_assert!(!path.starts_with('/') && !path.contains('\\'));
            prop_assert!(path.split('/').all(|c| !c.is_empty() && c != "." && c != ".."));
        }
    }

    /// An accepted path is in normalisation form C, and a path's NFC form is accepted when the
    /// path is otherwise valid (ADR 0033).
    #[test]
    fn accepted_paths_are_nfc(path in "[a-z/\\u{300}-\\u{36f}\\u{c0}-\\u{17f}]{1,12}") {
        use unicode_normalization::UnicodeNormalization;
        if is_valid_path(&path) {
            prop_assert!(unicode_normalization::is_nfc(&path));
        }
        let nfc: String = path.nfc().collect();
        let others_valid = !path.is_empty() && path.split('/').all(|c| !c.is_empty());
        if others_valid {
            prop_assert!(is_valid_path(&nfc), "{nfc:?}");
        }
    }
}
