//! Checks the sync/2 and blobs/2 messages against `specs/vectors/sync-v2.json` and
//! `blobs-v2.json`, which a second implementation, written in Python from the specs, makes
//! (`specs/vectors/gen`). Unlike the v1 vectors they are never regenerated from this code: each
//! message is built here from its fields and must encode to the same bytes, and each frame must
//! decode and verify.

use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use serde_json::Value;
use shoal_proto::blobs2::{BlobId, BlobsErrorCode, BlobsMessage};
use shoal_proto::identity::{DeviceCertificate, DeviceRevocation};
use shoal_proto::sync::EntryKind;
use shoal_proto::sync2::{
    ChunkList, EncryptionKey, ErrorCode, Item, KeyGrant, SealedPayload, SealedVersion, SyncMessage,
};
use shoal_proto::{frame, ChunkId, DeviceId, EntryId, FolderId, KeyId, ListId, Signed, UserId};

fn load(file: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/vectors")
        .join(file);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn bytes(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn array<const N: usize>(v: &Value) -> [u8; N] {
    bytes(v).try_into().unwrap()
}

fn frames(file: &str) -> Vec<(String, Vec<u8>)> {
    load(file)["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["name"].as_str().unwrap().to_owned(), bytes(&v["frame"])))
        .collect()
}

fn device() -> SigningKey {
    SigningKey::from_bytes(&[0x11; 32])
}

fn user() -> SigningKey {
    SigningKey::from_bytes(&[0x77; 32])
}

fn device_id() -> DeviceId {
    DeviceId(device().verifying_key().to_bytes())
}

const FOLDER: FolderId = FolderId([0x22; 32]);
const ENTRY: EntryId = EntryId([0x33; 16]);

fn certificate() -> Signed<DeviceCertificate> {
    Signed::sign(
        &DeviceCertificate {
            user: UserId(user().verifying_key().to_bytes()),
            device: device_id(),
            name: "test device".into(),
            issued_ms: 1_758_600_000_000,
        },
        &user(),
    )
}

fn revocation() -> Signed<DeviceRevocation> {
    Signed::sign(
        &DeviceRevocation {
            user: UserId(user().verifying_key().to_bytes()),
            device: DeviceId([0x33; 32]),
            revoked_ms: 1_760_400_000_000,
            lost_ms: Some(1_760_300_000_000),
        },
        &user(),
    )
}

/// The sealed version the vectors carry, from encryption-v1.json's chunks and payload.
fn sealed_version(chunks: ChunkList, version: shoal_proto::VersionVector) -> Signed<SealedVersion> {
    let enc = load("encryption-v1.json");
    Signed::sign(
        &SealedVersion {
            folder: FOLDER,
            entry: ENTRY,
            author: device_id(),
            generation: 1,
            key: KeyId(array(&enc["folder_key"]["key_id"])),
            chunks,
            sealed: bytes(&enc["sealed_payload"]["sealed"]),
            version,
        },
        &device(),
    )
}

fn inline_chunks() -> ChunkList {
    let enc = load("encryption-v1.json");
    ChunkList::Inline(
        enc["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| ChunkId(array(&c["chunk_id"])))
            .collect(),
    )
}

#[test]
fn sync_v2_messages_encode_as_the_spec_says() {
    let enc = load("encryption-v1.json");
    let author = device_id();
    let encryption_key: Signed<EncryptionKey> =
        postcard::from_bytes(&bytes(&enc["grant"]["signed_encryption_key"])).unwrap();
    let grant: Signed<KeyGrant> =
        postcard::from_bytes(&bytes(&enc["grant"]["signed_key_grant"])).unwrap();

    let mut with_holes: shoal_proto::VersionVector = [(author, 1), (DeviceId([0x99; 32]), 2)]
        .into_iter()
        .collect();
    for n in 3..=5 {
        with_holes.add(author, n);
    }
    let lists = ChunkList::Lists(vec![
        ListId(array(&enc["list_blob"]["list_id"])),
        ListId([0x45; 32]),
    ]);
    let built = [
        (
            "hello",
            SyncMessage::Hello {
                certificate: certificate(),
                folders: vec![FOLDER],
                clock_ms: 1_758_600_000_000,
                revocations: vec![revocation()],
            },
        ),
        (
            "index-request-initial",
            SyncMessage::IndexRequest {
                folder: FOLDER,
                epoch: 0,
                since: 0,
            },
        ),
        (
            "index-batch",
            SyncMessage::IndexBatch {
                folder: FOLDER,
                epoch: 0x0123_4567_89ab_cdef,
                up_to: 3,
                items: vec![
                    Item::Version(sealed_version(
                        inline_chunks(),
                        [(author, 3)].into_iter().collect(),
                    )),
                    Item::EncryptionKey(encryption_key),
                    Item::Grant(grant),
                ],
                last: true,
            },
        ),
        (
            "index-batch-with-holes-and-lists",
            SyncMessage::IndexBatch {
                folder: FOLDER,
                epoch: 0x0123_4567_89ab_cdef,
                up_to: 5,
                items: vec![
                    Item::Version(sealed_version(inline_chunks(), with_holes)),
                    Item::Version(sealed_version(lists, [(author, 1)].into_iter().collect())),
                ],
                last: false,
            },
        ),
        (
            "certificate-request",
            SyncMessage::CertificateRequest {
                folder: FOLDER,
                device: author,
            },
        ),
        (
            "certificate-response",
            SyncMessage::CertificateResponse {
                certificate: certificate(),
            },
        ),
        (
            "error-unknown-device",
            SyncMessage::Error {
                code: ErrorCode::UnknownDevice,
            },
        ),
    ];
    let expected = frames("sync-v2.json");
    assert_eq!(expected.len(), built.len());
    for ((name, message), (want_name, want)) in built.iter().zip(&expected) {
        assert_eq!(name, want_name);
        assert_eq!(frame::encode(message), *want, "{name}");
        let (decoded, used): (SyncMessage, usize) = frame::decode(want).unwrap();
        assert_eq!(used, want.len());
        assert_eq!(decoded, *message, "{name}");
    }
}

#[test]
fn items_in_the_vectors_verify() {
    for (name, frame_bytes) in frames("sync-v2.json") {
        let (message, _): (SyncMessage, usize) = frame::decode(&frame_bytes).unwrap();
        let SyncMessage::IndexBatch { items, .. } = message else {
            continue;
        };
        for item in items {
            match item {
                Item::Version(v) => {
                    v.verify_authored()
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                Item::EncryptionKey(k) => {
                    k.verify_own().unwrap();
                }
                Item::Grant(g) => {
                    let grant = g.verify().unwrap();
                    assert_eq!(grant.sealed.len(), 48);
                }
            }
        }
    }
}

#[test]
fn the_sealed_payload_decodes_and_checks() {
    let enc = load("encryption-v1.json");
    let payload = SealedPayload::decode(&bytes(&enc["sealed_payload"]["payload"])).unwrap();
    assert_eq!(payload.path, "docs/readme.txt");
    let EntryKind::File { size, .. } = payload.kind else {
        panic!("a file");
    };
    assert_eq!(size, 5000 + 4096);
    assert_eq!(payload.chunk_lens, vec![5000, 4096]);
    assert_eq!(payload.encode(), bytes(&enc["sealed_payload"]["payload"]));

    let version = sealed_version(inline_chunks(), [(device_id(), 3)].into_iter().collect())
        .verify_authored()
        .unwrap();
    let decrypted = payload.clone().into_version(&version, 2).unwrap();
    assert_eq!(decrypted.path, "docs/readme.txt");
    assert_eq!(decrypted.author, device_id());
    // The lengths must match the chunk IDs, and add up to the size.
    assert!(payload.clone().into_version(&version, 3).is_err());
    let mut short = payload;
    short.chunk_lens = vec![5000, 4095];
    assert!(short.into_version(&version, 2).is_err());

    // A payload with a byte left over is refused (encryption-v1.json, refused).
    let mut longer = bytes(&enc["sealed_payload"]["payload"]);
    longer.push(7);
    assert!(SealedPayload::decode(&longer).is_err());
}

#[test]
fn blobs_v2_messages_encode_as_the_spec_says() {
    let enc = load("encryption-v1.json");
    let chunk = &enc["chunks"][1];
    let chunk_id = BlobId::from(ChunkId(array(&chunk["chunk_id"])));
    let list_id = BlobId::from(ListId(array(&enc["list_blob"]["list_id"])));
    let built = [
        (
            "hello",
            BlobsMessage::Hello {
                certificate: certificate(),
            },
        ),
        (
            "request",
            BlobsMessage::Request {
                folder: FOLDER,
                blobs: vec![chunk_id, list_id],
            },
        ),
        (
            "blob-sealed-chunk",
            BlobsMessage::Blob {
                id: chunk_id,
                data: bytes(&chunk["sealed"]),
            },
        ),
        (
            "blob-list",
            BlobsMessage::Blob {
                id: list_id,
                data: bytes(&enc["list_blob"]["blob"]),
            },
        ),
        (
            "missing",
            BlobsMessage::Missing {
                id: BlobId([0x55; 32]),
            },
        ),
        (
            "availability-request",
            BlobsMessage::AvailabilityRequest {
                folder: FOLDER,
                blobs: vec![chunk_id, list_id, BlobId([0x55; 32])],
            },
        ),
        (
            "availability",
            BlobsMessage::Availability {
                bitmap: vec![0b011],
            },
        ),
        (
            "error-too-many-blobs",
            BlobsMessage::Error {
                code: BlobsErrorCode::TooManyBlobs,
            },
        ),
        (
            "replica-request",
            BlobsMessage::ReplicaRequest { folder: FOLDER },
        ),
        ("replica", BlobsMessage::Replica { full: true }),
        (
            "busy",
            BlobsMessage::Busy {
                retry_after_ms: 1500,
            },
        ),
    ];
    let expected = frames("blobs-v2.json");
    assert_eq!(expected.len(), built.len());
    for ((name, message), (want_name, want)) in built.iter().zip(&expected) {
        assert_eq!(name, want_name);
        assert_eq!(frame::encode(message), *want, "{name}");
        let (decoded, _): (BlobsMessage, usize) = frame::decode(want).unwrap();
        assert_eq!(decoded, *message, "{name}");
    }
}

#[test]
fn v2_messages_refuse_bytes_left_over() {
    let (_, hello) = &frames("sync-v2.json")[0];
    let (_, body_start) = (hello[0], 2);
    let mut longer = hello[body_start..].to_vec();
    longer.push(0);
    let mut framed = vec![];
    let len = longer.len();
    framed.push(u8::try_from(len & 0x7f).unwrap() | 0x80);
    framed.push(u8::try_from(len >> 7).unwrap());
    framed.extend(longer);
    assert!(frame::decode::<SyncMessage>(&framed).is_err());
}

#[test]
fn storage_v1_messages_encode_as_the_spec_says() {
    use shoal_proto::storage::{Hint, StorageErrorCode, StorageMessage};
    let hint = |n: u8, priority| Hint {
        entry: EntryId([n; 16]),
        priority,
    };
    let built = [
        (
            "hello",
            StorageMessage::Hello {
                certificate: certificate(),
            },
        ),
        (
            "priorities",
            StorageMessage::Priorities {
                folder: FOLDER,
                issued_ms: 1_792_000_000_000,
                hints: vec![hint(0x11, 200), hint(0x22, 0), hint(0x33, 128)],
            },
        ),
        ("accepted", StorageMessage::Accepted),
        (
            "error-not-stored",
            StorageMessage::Error {
                code: StorageErrorCode::NotStored,
            },
        ),
        (
            "usage-request",
            StorageMessage::UsageRequest { folder: FOLDER },
        ),
        (
            "usage",
            StorageMessage::Usage {
                quota_bytes: 100_000_000_000,
                held_bytes: 42_000_000_000,
                room_bytes: 58_000_000_000,
            },
        ),
        (
            "drop",
            StorageMessage::Drop {
                folder: FOLDER,
                below: 100,
            },
        ),
        (
            "dropped",
            StorageMessage::Dropped {
                chunks: 1234,
                bytes: 321_000_000,
            },
        ),
    ];
    let expected = frames("storage-v1.json");
    assert_eq!(expected.len(), built.len());
    for ((name, message), (want_name, want)) in built.iter().zip(&expected) {
        assert_eq!(name, want_name);
        assert_eq!(frame::encode(message), *want, "{name}");
        let (decoded, _): (StorageMessage, usize) = frame::decode(want).unwrap();
        assert_eq!(decoded, *message, "{name}");
    }
}
