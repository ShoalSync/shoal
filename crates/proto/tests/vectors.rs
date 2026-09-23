//! Golden encodings in `specs/vectors/`. Any change to these bytes is a wire-format change.
//!
//! Regenerate after an intentional change with `TIDEFELL_BLESS=1 cargo test -p tidefell-proto`,
//! and bump the ALPN version if the change is breaking.

use std::fmt::Write as _;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use tidefell_proto::sync::{EntryKind, EntryVersion, ErrorCode, SyncMessage};
use tidefell_proto::{
    frame, ChunkHash, ChunkRef, ContentHash, DeviceId, EntryId, FolderId, Manifest, Signed,
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

fn device_key() -> SigningKey {
    SigningKey::from_bytes(&[0x11; 32])
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

fn sync_v1_vectors() -> Vec<(&'static str, Vec<u8>)> {
    let folder = FolderId([0x22; 32]);
    let messages = [
        (
            "hello",
            SyncMessage::Hello {
                device: DeviceId(device_key().verifying_key().to_bytes()),
                folders: vec![folder],
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

#[test]
fn sync_v1_golden_vectors() {
    let path = specs_dir().join("vectors/sync-v1.json");
    let actual: Vec<Value> = sync_v1_vectors()
        .into_iter()
        .map(|(name, bytes)| json!({ "name": name, "frame": hex(&bytes) }))
        .collect();
    let actual = json!({
        "alpn": "tidefell/sync/1",
        "encoding": "varint length prefix + postcard",
        "signing_key_seed": hex(&[0x11; 32]),
        "vectors": actual,
    });

    if std::env::var_os("TIDEFELL_BLESS").is_some() {
        let text = serde_json::to_string_pretty(&actual).unwrap() + "\n";
        std::fs::write(&path, text).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("missing vectors; run with TIDEFELL_BLESS=1"),
    )
    .unwrap();
    assert_eq!(
        actual, expected,
        "wire encoding changed; see the header of this test"
    );
}

#[test]
fn signed_entry_version_verifies() {
    let signed = Signed::sign(&sample_version(), &device_key());
    assert_eq!(signed.verify_authored().unwrap(), sample_version());
}

#[test]
fn entry_version_signed_by_another_device_is_rejected() {
    let other = SigningKey::from_bytes(&[0x12; 32]);
    let signed = Signed::sign(&sample_version(), &other);
    assert!(signed.verify().is_ok());
    assert!(matches!(
        signed.verify_authored(),
        Err(tidefell_proto::Error::SignerMismatch)
    ));
}

#[test]
fn alpn_constants_match_specs() {
    for (alpn, file) in [
        (tidefell_proto::ALPN_SYNC_V1, "sync-v1.md"),
        (tidefell_proto::ALPN_BLOBS_V1, "blobs-v1.md"),
    ] {
        let spec = std::fs::read_to_string(specs_dir().join(file)).unwrap();
        let alpn = std::str::from_utf8(alpn).unwrap();
        assert!(
            spec.contains(&format!("ALPN: `{alpn}`")),
            "{file} does not declare ALPN {alpn}"
        );
    }
}
