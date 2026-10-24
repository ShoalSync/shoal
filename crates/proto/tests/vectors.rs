//! Golden encodings in `specs/vectors/`. Any change to these bytes is a wire-format change.
//!
//! Regenerate after an intentional change with `SHOAL_BLESS=1 cargo test -p shoal-proto`,
//! and bump the ALPN version if the change is breaking.

use std::fmt::Write as _;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use shoal_proto::blobs::{BlobsErrorCode, BlobsMessage};
use shoal_proto::frame::Message;
use shoal_proto::identity::{DeviceCertificate, DeviceRevocation};
use shoal_proto::pair::{FolderOffer, OfferedEncryption, PairErrorCode, PairMessage, PairingCode};
use shoal_proto::sync::{EntryKind, EntryVersion, ErrorCode, SyncMessage};
use shoal_proto::{
    frame, ChunkHash, ChunkRef, ContentHash, DeviceId, EntryId, FolderId, Manifest, Signed, UserId,
};

fn specs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../specs")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn device_key() -> SigningKey {
    SigningKey::from_bytes(&[0x11; 32])
}

fn user_key() -> SigningKey {
    SigningKey::from_bytes(&[0x77; 32])
}

fn certificate() -> Signed<DeviceCertificate> {
    Signed::sign(
        &DeviceCertificate {
            user: UserId(user_key().verifying_key().to_bytes()),
            device: DeviceId(device_key().verifying_key().to_bytes()),
            name: "test device".into(),
            issued_ms: 1_758_600_000_000,
        },
        &user_key(),
    )
}

fn sample_version() -> EntryVersion {
    let author = DeviceId(device_key().verifying_key().to_bytes());
    EntryVersion {
        folder: FolderId([0x22; 32]),
        entry: EntryId([0x33; 16]),
        path: "docs/readme.txt".into(),
        kind: EntryKind::File {
            content: ContentHash([0x44; 32]),
            size: 1234,
            executable: false,
        },
        modified_ms: 1_758_600_000_000,
        author,
        version: [(author, 3)].into_iter().collect(),
    }
}

/// A version that has seen its author's changes 1 and 3 to 5, but not 2: made on top of a
/// version that had not seen change 2 (ADR 0029).
fn version_with_holes() -> EntryVersion {
    let author = DeviceId(device_key().verifying_key().to_bytes());
    let mut version = sample_version();
    version.version = [(author, 1), (DeviceId([0x99; 32]), 2)]
        .into_iter()
        .collect();
    for n in 3..=5 {
        version.version.add(author, n);
    }
    version
}

fn revocation() -> Signed<DeviceRevocation> {
    Signed::sign(
        &DeviceRevocation {
            user: UserId(user_key().verifying_key().to_bytes()),
            device: DeviceId([0x33; 32]),
            revoked_ms: 1_760_400_000_000,
            lost_ms: Some(1_760_300_000_000),
        },
        &user_key(),
    )
}

fn sync_v1_vectors() -> Vec<(&'static str, Vec<u8>)> {
    let folder = FolderId([0x22; 32]);
    let messages = [
        (
            "hello",
            SyncMessage::Hello {
                certificate: certificate(),
                folders: vec![folder],
                clock_ms: Some(1_758_600_000_000),
                revocations: Some(Vec::new()),
            },
        ),
        (
            "hello-with-revocations",
            SyncMessage::Hello {
                certificate: certificate(),
                folders: vec![folder],
                clock_ms: Some(1_758_600_000_000),
                revocations: Some(vec![revocation()]),
            },
        ),
        (
            "index-request-initial",
            SyncMessage::IndexRequest {
                folder,
                epoch: 0,
                since: 0,
            },
        ),
        (
            "index-batch",
            SyncMessage::IndexBatch {
                folder,
                epoch: 0x0123_4567_89ab_cdef,
                up_to: 1,
                versions: vec![Signed::sign(&sample_version(), &device_key())],
                last: true,
            },
        ),
        (
            "index-batch-with-holes",
            SyncMessage::IndexBatch {
                folder,
                epoch: 0x0123_4567_89ab_cdef,
                up_to: 2,
                versions: vec![Signed::sign(&version_with_holes(), &device_key())],
                last: true,
            },
        ),
        (
            "manifest-request",
            SyncMessage::ManifestRequest {
                folder,
                content: ContentHash([0x44; 32]),
            },
        ),
        (
            "manifest-response",
            SyncMessage::ManifestResponse {
                manifest: Manifest {
                    content: ContentHash([0x44; 32]),
                    chunks: vec![
                        ChunkRef {
                            hash: ChunkHash([0x55; 32]),
                            len: 1000,
                        },
                        ChunkRef {
                            hash: ChunkHash([0x66; 32]),
                            len: 234,
                        },
                    ],
                },
            },
        ),
        (
            "error-not-a-member",
            SyncMessage::Error {
                code: ErrorCode::NotAMember,
            },
        ),
        (
            "certificate-request",
            SyncMessage::CertificateRequest {
                folder,
                device: DeviceId(device_key().verifying_key().to_bytes()),
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
    messages
        .into_iter()
        .map(|(name, msg)| {
            let bytes = frame::encode(&msg);
            let (decoded, used): (SyncMessage, usize) = frame::decode(&bytes).unwrap();
            assert_eq!(decoded, msg, "{name} does not round-trip");
            assert_eq!(used, bytes.len());
            (name, bytes)
        })
        .collect()
}

fn blobs_v1_vectors() -> Vec<(&'static str, Vec<u8>)> {
    let folder = FolderId([0x22; 32]);
    let messages = [
        (
            "hello",
            BlobsMessage::Hello {
                certificate: certificate(),
            },
        ),
        (
            "request",
            BlobsMessage::Request {
                folder,
                chunks: vec![ChunkHash([0x55; 32]), ChunkHash([0x66; 32])],
            },
        ),
        (
            "chunk",
            BlobsMessage::Chunk {
                hash: ChunkHash([0x55; 32]),
                data: b"chunk bytes".to_vec(),
            },
        ),
        (
            "missing",
            BlobsMessage::Missing {
                hash: ChunkHash([0x66; 32]),
            },
        ),
        (
            "availability-request",
            BlobsMessage::AvailabilityRequest {
                folder,
                content: ContentHash([0x44; 32]),
            },
        ),
        (
            "availability",
            BlobsMessage::Availability {
                content: ContentHash([0x44; 32]),
                chunk_count: 10,
                bitmap: vec![0b0000_0101, 0b0000_0010],
            },
        ),
        (
            "error-too-many-chunks",
            BlobsMessage::Error {
                code: BlobsErrorCode::TooManyChunks,
            },
        ),
        ("replica-request", BlobsMessage::ReplicaRequest { folder }),
        ("replica", BlobsMessage::Replica { full: true }),
        (
            "busy",
            BlobsMessage::Busy {
                retry_after_ms: 2500,
            },
        ),
    ];
    messages
        .into_iter()
        .map(|(name, msg)| {
            let bytes = frame::encode(&msg);
            let (decoded, used): (BlobsMessage, usize) = frame::decode(&bytes).unwrap();
            assert_eq!(decoded, msg, "{name} does not round-trip");
            assert_eq!(used, bytes.len());
            (name, bytes)
        })
        .collect()
}

fn pair_v2_vectors() -> Vec<(&'static str, Vec<u8>)> {
    let messages = [
        (
            "request",
            PairMessage::Request {
                approver: certificate(),
                proof: [0x88; 32],
            },
        ),
        (
            "accept",
            PairMessage::Accept {
                name: "Pixel 9".into(),
            },
        ),
        (
            "complete",
            PairMessage::Complete {
                certificate: certificate(),
                relay: Some("https://relay.shoal.org./".into()),
                addrs: vec!["192.168.1.20:7777".parse().unwrap()],
                folders: vec![
                    FolderOffer {
                        id: FolderId([0x22; 32]),
                        name: "Photos".into(),
                        members: vec![UserId(user_key().verifying_key().to_bytes())],
                        retention_days: 30,
                        encryption: None,
                    },
                    FolderOffer {
                        id: FolderId([0x23; 32]),
                        name: "Papers".into(),
                        members: vec![UserId(user_key().verifying_key().to_bytes())],
                        retention_days: 0,
                        encryption: Some(OfferedEncryption {
                            storage_peers: vec![DeviceId([0x44; 32])],
                        }),
                    },
                ],
            },
        ),
        (
            "reject-declined",
            PairMessage::Reject {
                code: PairErrorCode::Declined,
            },
        ),
    ];
    let mut vectors: Vec<(&'static str, Vec<u8>)> = messages
        .into_iter()
        .map(|(name, msg)| {
            let bytes = frame::encode(&msg);
            let (decoded, used): (PairMessage, usize) = frame::decode(&bytes).unwrap();
            assert_eq!(decoded, msg, "{name} does not round-trip");
            assert_eq!(used, bytes.len());
            (name, bytes)
        })
        .collect();
    // The pairing code's postcard body (its text form is TFP1- and this in base32).
    let code = PairingCode {
        device: DeviceId(device_key().verifying_key().to_bytes()),
        secret: [0x5a; 16],
        relay: None,
        addrs: vec!["10.0.2.15:41641".parse().unwrap()],
    };
    assert_eq!(PairingCode::from_text(&code.to_text()).unwrap(), code);
    vectors.push(("pairing-code", postcard::to_stdvec(&code).unwrap()));
    vectors
}

fn check_vectors(file: &str, alpn: &str, vectors: Vec<(&'static str, Vec<u8>)>) {
    let path = specs_dir().join("vectors").join(file);
    let actual: Vec<Value> = vectors
        .into_iter()
        .map(|(name, bytes)| json!({ "name": name, "frame": hex(&bytes) }))
        .collect();
    let actual = json!({
        "alpn": alpn,
        "encoding": "varint length prefix + postcard",
        "device_key_seed": hex(&[0x11; 32]),
        "user_key_seed": hex(&[0x77; 32]),
        "vectors": actual,
    });

    if std::env::var_os("SHOAL_BLESS").is_some() {
        let text = serde_json::to_string_pretty(&actual).unwrap() + "\n";
        std::fs::write(&path, text).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("missing vectors; run with SHOAL_BLESS=1"),
    )
    .unwrap();
    assert_eq!(
        actual, expected,
        "wire encoding changed; see the header of this test"
    );
}

#[test]
fn sync_v1_golden_vectors() {
    check_vectors("sync-v1.json", "shoal/sync/1", sync_v1_vectors());
}

#[test]
fn blobs_v1_golden_vectors() {
    check_vectors("blobs-v1.json", "shoal/blobs/1", blobs_v1_vectors());
}

#[test]
fn pair_v2_golden_vectors() {
    check_vectors("pair-v2.json", "shoal/pair/2", pair_v2_vectors());
}

#[test]
fn certificate_must_be_signed_by_its_user() {
    assert!(certificate().verify_certificate().is_ok());
    let forged = Signed::sign(&certificate().verify().unwrap(), &device_key());
    assert!(forged.verify().is_ok());
    assert!(matches!(
        forged.verify_certificate(),
        Err(shoal_proto::Error::SignerMismatch)
    ));
}

#[test]
fn signed_entry_version_verifies() {
    let signed = Signed::sign(&sample_version(), &device_key());
    assert_eq!(signed.verify_authored().unwrap(), sample_version());
}

#[test]
fn entry_versions_have_one_encoding() {
    // Without holes, the encoding is the one from before vectors had holes.
    let plain = Signed::sign(&sample_version(), &device_key());
    assert_eq!(
        plain.body,
        postcard::to_stdvec(&sample_version()).unwrap()[..plain.body.len()]
    );
    assert_eq!(plain.verify_authored().unwrap(), sample_version());
    let holes = Signed::sign(&version_with_holes(), &device_key());
    assert_eq!(holes.verify_authored().unwrap(), version_with_holes());

    // An empty list of holes written out, or bytes after the version, are refused.
    for body in [
        postcard::to_stdvec(&sample_version()).unwrap(),
        [holes.body.clone(), vec![0]].concat(),
    ] {
        let signature = ed25519_dalek::Signer::sign(
            &device_key(),
            &shoal_proto::signed::signing_bytes("sync/1/entry-version", &body),
        );
        // Signed is the signer, the body with its length, then the signature.
        let encoded = [
            &postcard::to_stdvec(&holes).unwrap()[..32],
            &postcard::to_stdvec(&body).unwrap(),
            &signature.to_bytes(),
        ]
        .concat();
        let resigned: Signed<EntryVersion> = postcard::from_bytes(&encoded).unwrap();
        // The signature is good; the encoding is not.
        assert!(matches!(
            resigned.verify_authored(),
            Err(shoal_proto::Error::Decode(_))
        ));
    }
}

#[test]
fn entry_version_signed_by_another_device_is_rejected() {
    let other = SigningKey::from_bytes(&[0x12; 32]);
    let signed = Signed::sign(&sample_version(), &other);
    assert!(signed.verify().is_ok());
    assert!(matches!(
        signed.verify_authored(),
        Err(shoal_proto::Error::SignerMismatch)
    ));
}

#[test]
fn alpn_constants_match_specs() {
    for (alpn, file) in [
        (shoal_proto::ALPN_SYNC_V1, "sync-v1.md"),
        (shoal_proto::ALPN_BLOBS_V1, "blobs-v1.md"),
        (shoal_proto::ALPN_PAIR_V2, "pair-v2.md"),
    ] {
        let spec = std::fs::read_to_string(specs_dir().join(file)).unwrap();
        let alpn = std::str::from_utf8(alpn).unwrap();
        assert!(
            spec.contains(&format!("ALPN: `{alpn}`")),
            "{file} does not declare ALPN {alpn}"
        );
    }
}

#[test]
fn hello_from_before_revocations_decodes_and_is_read_by_0_19() {
    // 0.19's "hello" vector: its frame ends after `clock_ms` (ADR 0040).
    let frame = unhex(
        "dc0100c853ad0f0cd2b619aea92ceec4fd56a24d6499d584ce79257e45cfd8139b60a752c853ad0f0cd2b6\
         19aea92ceec4fd56a24d6499d584ce79257e45cfd8139b60a7d04ab232742bb4ab3a1368bd4615e4e6d02\
         24ab71a016baf8520a332c97787370b746573742064657669636580c8d3cbae667d729b8774f97583e7bf\
         1636b4e3e4d60f6950f7e08ee9d26285bbb9cc565274e2b87d7adc85bcef55f5de8a7f65c99189feefdee\
         ee79a9ef495c4ff150fb7000122222222222222222222222222222222222222222222222222222222222222\
         220180c8d3cbae66",
    );
    let (decoded, _) = shoal_proto::frame::decode::<SyncMessage>(&frame).unwrap();
    let SyncMessage::Hello {
        clock_ms,
        revocations,
        ..
    } = decoded
    else {
        panic!("not a hello");
    };
    assert_eq!(clock_ms, Some(1_758_600_000_000));
    assert_eq!(revocations, None, "absent, not empty");

    // 0.19 reads a Hello's body with postcard, which stops after `clock_ms` and leaves the
    // revocations unread: the same fields as 0.19's Hello.
    #[derive(serde::Deserialize, Debug)]
    enum Hello019 {
        Hello {
            _certificate: Signed<DeviceCertificate>,
            _folders: Vec<FolderId>,
            clock_ms: Option<i64>,
        },
    }
    let new = postcard::to_stdvec(&SyncMessage::Hello {
        certificate: certificate(),
        folders: vec![FolderId([0x22; 32])],
        clock_ms: Some(5),
        revocations: Some(vec![revocation()]),
    })
    .unwrap();
    let Hello019::Hello { clock_ms, .. } = postcard::from_bytes(&new).unwrap();
    assert_eq!(clock_ms, Some(5));
}

#[test]
fn a_revocation_binds_its_user() {
    let revocation = revocation().verify_revocation().unwrap();
    let mine = DeviceCertificate {
        device: revocation.device,
        ..certificate().verify_certificate().unwrap()
    };
    assert!(revocation.revokes(&mine));
    let theirs = DeviceCertificate {
        user: UserId([0x44; 32]),
        ..mine.clone()
    };
    assert!(
        !revocation.revokes(&theirs),
        "another user's device of the same ID"
    );
    let other = DeviceCertificate {
        device: DeviceId([0x55; 32]),
        ..mine
    };
    assert!(!revocation.revokes(&other));
    // Signed by someone other than the user it names: refused.
    let forged = Signed::sign(
        &DeviceRevocation {
            user: UserId(user_key().verifying_key().to_bytes()),
            ..revocation
        },
        &device_key(),
    );
    assert!(forged.verify_revocation().is_err());
}

#[test]
fn hello_from_before_the_clock_decodes() {
    // A Hello as devices sent it before `clock_ms` (ADR 0030): the frame ends after `folders`.
    let old = SyncMessage::Hello {
        certificate: certificate(),
        folders: vec![FolderId([0x22; 32])],
        clock_ms: None,
        revocations: None,
    };
    let mut body = postcard::to_stdvec(&old).unwrap();
    assert_eq!(body.pop(), Some(0), "None is one zero byte at the end");
    assert_eq!(body.pop(), Some(0), "and another");
    let decoded = SyncMessage::decode_body(&body).unwrap();
    assert_eq!(decoded, old);
    // Any other message must be complete: an index batch cut short before `last` is refused,
    // not read as `last: false`.
    let batch = postcard::to_stdvec(&SyncMessage::IndexBatch {
        folder: FolderId([0x22; 32]),
        epoch: 1,
        up_to: 1,
        versions: Vec::new(),
        last: false,
    })
    .unwrap();
    assert!(SyncMessage::decode_body(&batch[..batch.len() - 1]).is_err());
    assert!(SyncMessage::decode_body(&body[..body.len() - 5]).is_err());
}
