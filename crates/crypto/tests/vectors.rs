//! Checks the crate against `specs/vectors/encryption-v1.json`, which a second implementation,
//! written in Python from the spec, makes (`specs/vectors/gen`). The signed encryption key and
//! grant are checked with the sync/2 messages, in `shoal-proto`.

use std::path::PathBuf;

use serde_json::Value;
use shoal_crypto::{
    list_blob, open_grant, read_list_blob, seal_grant_with, EncryptionSecret, Error, FolderKey,
    GrantFor,
};
use shoal_proto::{ChunkId, DeviceId, EntryId, FolderId, KeyId, ListId};

fn vectors() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../specs/vectors/encryption-v1.json");
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

#[test]
fn folder_key_chunks_lists_and_payloads() {
    let v = vectors();
    let folder = FolderId(array(&v["folder"]));
    let entry = EntryId(array(&v["entry"]));
    let key = FolderKey::new(array(&v["folder_key"]["key"]), 1);
    assert_eq!(key.id(), KeyId(array(&v["folder_key"]["key_id"])));

    for chunk in v["chunks"].as_array().unwrap() {
        let plaintext = bytes(&chunk["plaintext"]);
        let id = ChunkId(array(&chunk["chunk_id"]));
        let (sealed_id, sealed) = key.seal_chunk(&folder, &plaintext).unwrap();
        assert_eq!(sealed_id, id);
        assert_eq!(sealed, bytes(&chunk["sealed"]));
        assert_eq!(
            sealed.len(),
            usize::try_from(chunk["padded_len"].as_u64().unwrap()).unwrap() + 16
        );
        assert_eq!(key.open_chunk(&folder, &id, &sealed).unwrap(), plaintext);
    }

    let ids: Vec<ChunkId> = v["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| ChunkId(array(&c["chunk_id"])))
        .chain([ChunkId([0x44; 32])])
        .collect();
    let (list_id, blob) = list_blob(&ids).unwrap();
    assert_eq!(blob, bytes(&v["list_blob"]["blob"]));
    assert_eq!(list_id, ListId(array(&v["list_blob"]["list_id"])));
    assert_eq!(read_list_blob(&list_id, &blob).unwrap(), ids);

    let p = &v["sealed_payload"];
    let author = DeviceId(array(&p["author"]));
    let sealed = key
        .seal_payload_with(
            &folder,
            &entry,
            &author,
            &bytes(&p["payload"]),
            array(&p["nonce"]),
        )
        .unwrap();
    assert_eq!(sealed, bytes(&p["sealed"]));
    assert_eq!(
        key.open_payload(&folder, &entry, &author, &sealed).unwrap(),
        bytes(&p["payload"])
    );
}

#[test]
fn grant() {
    let v = vectors();
    let g = &v["grant"];
    let key = FolderKey::new(
        array(&v["folder_key"]["key"]),
        g["generation"].as_u64().unwrap(),
    );
    let secret = EncryptionSecret::from_ikm(&bytes(&g["recipient_ikm"]));
    assert_eq!(secret.to_bytes(), array(&g["recipient_private_key"]));
    assert_eq!(secret.public(), array(&g["recipient_key"]));
    let to = GrantFor {
        folder: FolderId(array(&v["folder"])),
        recipient: DeviceId(array(&g["recipient"])),
        recipient_key: secret.public(),
    };
    let (enc, sealed) = seal_grant_with(&key, &to, &bytes(&g["ephemeral_ikm"])).unwrap();
    assert_eq!(enc, array(&g["enc"]));
    assert_eq!(sealed, bytes(&g["sealed"]));
    let opened = open_grant(&secret, &to, key.generation(), &key.id(), &enc, &sealed).unwrap();
    assert_eq!(opened.as_bytes(), key.as_bytes());
}

#[test]
fn refused() {
    let v = vectors();
    let folder = FolderId(array(&v["folder"]));
    let entry = EntryId(array(&v["entry"]));
    let key = FolderKey::new(array(&v["folder_key"]["key"]), 1);
    let g = &v["grant"];
    let secret = EncryptionSecret::from_ikm(&bytes(&g["recipient_ikm"]));
    let to = GrantFor {
        folder,
        recipient: DeviceId(array(&g["recipient"])),
        recipient_key: secret.public(),
    };
    let mut seen = 0;
    for case in v["refused"].as_array().unwrap() {
        let result = match case["name"].as_str().unwrap() {
            "chunk-without-terminator" | "chunk-not-matching-its-id" => key
                .open_chunk(
                    &folder,
                    &ChunkId(array(&case["chunk_id"])),
                    &bytes(&case["sealed"]),
                )
                .map(|_| ()),
            // The payload opens; decoding it, in sync/2, finds the byte left over.
            "payload-with-bytes-left-over" => {
                let author = DeviceId(array(&case["author"]));
                let payload = key
                    .open_payload(&folder, &entry, &author, &bytes(&case["sealed"]))
                    .unwrap();
                let expected = bytes(&v["sealed_payload"]["payload"]);
                assert_eq!(payload, [expected.as_slice(), &[7]].concat());
                Err(Error::Malformed("payload"))
            }
            "grant-of-a-key-not-matching-its-key-id" => open_grant(
                &secret,
                &to,
                1,
                &key.id(),
                &array(&case["enc"]),
                &bytes(&case["sealed"]),
            )
            .map(|_| ()),
            "list-blob-not-matching-its-id" => {
                read_list_blob(&ListId(array(&case["list_id"])), &bytes(&case["blob"])).map(|_| ())
            }
            other => panic!("unknown case {other}"),
        };
        let expected = match case["name"].as_str().unwrap() {
            "chunk-without-terminator" => Error::Padding,
            "chunk-not-matching-its-id" => Error::Mismatch("chunk"),
            "payload-with-bytes-left-over" => Error::Malformed("payload"),
            "grant-of-a-key-not-matching-its-key-id" => Error::Mismatch("granted key"),
            _ => Error::Mismatch("list blob"),
        };
        assert_eq!(result, Err(expected), "{}", case["name"]);
        seen += 1;
    }
    assert_eq!(seen, 5);
}
