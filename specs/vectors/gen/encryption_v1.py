"""Golden values for specs/encryption-v1.md, written from the spec, independent of the Rust code.

Run from this directory with Python 3 and the blake3, cryptography and pynacl packages:

    python3 encryption_v1.py > ../encryption-v1.json
"""

import json

import blake3
from nacl import bindings

import hpke
from wire import Key, bytes_, i, raw, seq, string, u, variant

FOLDER = b"\x22" * 32
ENTRY = b"\x33" * 16
FOLDER_KEY = bytes(range(32))
AUTHOR = Key(b"\x11" * 32)
RECIPIENT = Key(b"\x12" * 32)
RECIPIENT_IKM = b"\x88" * 32
EPHEMERAL_IKM = b"\x99" * 32
PAYLOAD_NONCE = b"\x66" * 24
MODIFIED_MS = 1_758_600_000_000
ISSUED_MS = 1_760_000_000_000


def derive_key(context: str, key: bytes) -> bytes:
    return blake3.blake3(key, derive_key_context=context).digest()


def keyed_hash(key: bytes, data: bytes) -> bytes:
    return blake3.blake3(data, key=key).digest()


def xseal(key: bytes, nonce: bytes, aad: bytes, plaintext: bytes) -> bytes:
    return bindings.crypto_aead_xchacha20poly1305_ietf_encrypt(plaintext, aad, nonce, key)


def xopen(key: bytes, nonce: bytes, aad: bytes, sealed: bytes) -> bytes:
    return bindings.crypto_aead_xchacha20poly1305_ietf_decrypt(sealed, aad, nonce, key)


def pad(data: bytes, block: int) -> bytes:
    padded = data + b"\x01"
    return padded + b"\x00" * (-len(padded) % block)


def unpad(padded: bytes, block: int) -> bytes:
    body = padded.rstrip(b"\x00")
    assert len(padded) - len(body) < block and body.endswith(b"\x01")
    return body[:-1]


class FolderKey:
    def __init__(self, key: bytes):
        self.key = key
        self.id = derive_key("shoal 2026-10-24 folder key id", key)[:16]
        self.chunk_id_key = derive_key("shoal 2026-10-24 chunk id", key)
        self.chunk_key = derive_key("shoal 2026-10-24 chunk key", key)
        self.chunk_nonce_key = derive_key("shoal 2026-10-24 chunk nonce", key)
        self.metadata_key = derive_key("shoal 2026-10-24 metadata key", key)

    def seal_chunk(self, plaintext: bytes):
        assert 0 < len(plaintext) <= 1 << 20
        chunk_id = keyed_hash(self.chunk_id_key, plaintext)
        nonce = keyed_hash(self.chunk_nonce_key, chunk_id)[:24]
        aad = b"shoal/chunk/1" + FOLDER + self.id + chunk_id
        padded = pad(plaintext, 4096)
        sealed = xseal(self.chunk_key, nonce, aad, padded)
        # Opened again as a reader would, from the chunk ID alone.
        assert unpad(xopen(self.chunk_key, nonce, aad, sealed), 4096) == plaintext
        return chunk_id, nonce, padded, sealed

    def seal_payload(self, payload: bytes, author: bytes, nonce: bytes) -> bytes:
        aad = b"shoal/version/1" + FOLDER + ENTRY + author + self.id
        sealed = nonce + xseal(self.metadata_key, nonce, aad, pad(payload, 64))
        assert unpad(xopen(self.metadata_key, sealed[:24], aad, sealed[24:]), 64) == payload
        return sealed


def main():
    key = FolderKey(FOLDER_KEY)
    chunks = [
        (b"0123456789abcdef" * 313)[:5000],
        bytes((n * 7) & 0xFF for n in range(4096)),
    ]
    sealed_chunks = []
    for plaintext in chunks:
        chunk_id, nonce, padded, sealed = key.seal_chunk(plaintext)
        sealed_chunks.append(
            {
                "plaintext": plaintext.hex(),
                "chunk_id": chunk_id.hex(),
                "nonce": nonce.hex(),
                "padded_len": len(padded),
                "sealed": sealed.hex(),
            }
        )
    ids = [bytes.fromhex(c["chunk_id"]) for c in sealed_chunks]

    list_blob = seq(ids + [b"\x44" * 32], raw(32))
    content = blake3.blake3(b"".join(chunks)).digest()
    kind = variant(0, content, u(sum(map(len, chunks))), b"\x00")
    payload = string("docs/readme.txt") + kind + i(MODIFIED_MS) + seq([len(c) for c in chunks], u)
    sealed_payload = key.seal_payload(payload, AUTHOR.public, PAYLOAD_NONCE)

    recipient_sk, recipient_pk = hpke.derive_key_pair(RECIPIENT_IKM)
    generation = 1
    info = (
        b"shoal/key-grant/1"
        + FOLDER
        + generation.to_bytes(8, "big")
        + key.id
        + RECIPIENT.public
        + recipient_pk
    )
    enc, sealed_key = hpke.seal(recipient_pk, info, b"", FOLDER_KEY, EPHEMERAL_IKM)
    assert len(sealed_key) == 48
    assert hpke.open_(enc, recipient_sk, info, b"", sealed_key) == FOLDER_KEY

    encryption_key = RECIPIENT.public + recipient_pk + i(ISSUED_MS)
    grant = (
        FOLDER
        + u(generation)
        + key.id
        + RECIPIENT.public
        + recipient_pk
        + enc
        + bytes_(sealed_key)
    )

    # Inputs a reader must refuse, each made like a valid one with one thing wrong.
    plaintext = chunks[0]
    first_id, first_nonce = ids[0], keyed_hash(key.chunk_nonce_key, ids[0])[:24]
    chunk_aad = b"shoal/chunk/1" + FOLDER + key.id + first_id
    unterminated = plaintext + b"\x00" * (-len(plaintext) % 4096)
    other = chunks[1][:-1] + b"\xff"
    payload_aad = b"shoal/version/1" + FOLDER + ENTRY + AUTHOR.public + key.id
    wrong_key = b"\xee" * 32
    _, wrong_grant = hpke.seal(recipient_pk, info, b"", wrong_key, EPHEMERAL_IKM)
    refused = [
        {
            "name": "chunk-without-terminator",
            "chunk_id": first_id.hex(),
            "sealed": xseal(key.chunk_key, first_nonce, chunk_aad, unterminated).hex(),
        },
        {
            "name": "chunk-not-matching-its-id",
            "chunk_id": first_id.hex(),
            "sealed": xseal(key.chunk_key, first_nonce, chunk_aad, pad(other, 4096)).hex(),
        },
        {
            "name": "payload-with-bytes-left-over",
            "author": AUTHOR.public.hex(),
            "sealed": (
                PAYLOAD_NONCE
                + xseal(key.metadata_key, PAYLOAD_NONCE, payload_aad, pad(payload + b"\x07", 64))
            ).hex(),
        },
        {
            "name": "grant-of-a-key-not-matching-its-key-id",
            "info": info.hex(),
            "enc": enc.hex(),
            "sealed": wrong_grant.hex(),
        },
        {
            "name": "list-blob-not-matching-its-id",
            "blob": (list_blob[:-1] + b"\x00").hex(),
            "list_id": blake3.blake3(list_blob).digest().hex(),
        },
    ]
    assert hpke.open_(enc, recipient_sk, info, b"", wrong_grant) == wrong_key

    hpke.check()
    out = {
        "refused": refused,
        "spec": "specs/encryption-v1.md",
        "folder": FOLDER.hex(),
        "entry": ENTRY.hex(),
        "folder_key": {
            "key": FOLDER_KEY.hex(),
            "key_id": key.id.hex(),
            "chunk_id_key": key.chunk_id_key.hex(),
            "chunk_key": key.chunk_key.hex(),
            "chunk_nonce_key": key.chunk_nonce_key.hex(),
            "metadata_key": key.metadata_key.hex(),
        },
        "chunks": sealed_chunks,
        "list_blob": {"blob": list_blob.hex(), "list_id": blake3.blake3(list_blob).digest().hex()},
        "sealed_payload": {
            "author_seed": "11" * 32,
            "author": AUTHOR.public.hex(),
            "payload": payload.hex(),
            "nonce": PAYLOAD_NONCE.hex(),
            "sealed": sealed_payload.hex(),
        },
        "grant": {
            "recipient_seed": "12" * 32,
            "recipient": RECIPIENT.public.hex(),
            "recipient_ikm": RECIPIENT_IKM.hex(),
            "recipient_private_key": recipient_sk.hex(),
            "recipient_key": recipient_pk.hex(),
            "ephemeral_ikm": EPHEMERAL_IKM.hex(),
            "generation": generation,
            "info": info.hex(),
            "enc": enc.hex(),
            "sealed": sealed_key.hex(),
            "signed_encryption_key": RECIPIENT.sign("sync/2/encryption-key", encryption_key).hex(),
            "signed_key_grant": AUTHOR.sign("sync/2/key-grant", grant).hex(),
        },
    }
    print(json.dumps(out, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
