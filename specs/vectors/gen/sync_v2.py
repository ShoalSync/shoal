"""Golden encodings for specs/sync-v2.md, written from the spec, independent of the Rust code.

Run from this directory with Python 3 and the blake3, cryptography and pynacl packages:

    python3 sync_v2.py > ../sync-v2.json
"""

import json

import blake3

import encryption_v1 as enc
import hpke
from wire import Key, boolean, bytes_, frame, i, raw, seq, string, trailing_vector, u, variant

DEVICE = enc.AUTHOR
USER = Key(b"\x77" * 32)
FOLDER = enc.FOLDER
ENTRY = enc.ENTRY


def certificate() -> bytes:
    body = USER.public + DEVICE.public + string("test device") + i(1_758_600_000_000)
    return USER.sign("identity/1/device-certificate", body)


def revocation() -> bytes:
    body = USER.public + b"\x33" * 32 + i(1_760_400_000_000) + b"\x01" + i(1_760_300_000_000)
    return USER.sign("identity/1/device-revocation", body)


def sealed_version(chunk_list: bytes, sealed: bytes, counters: dict, holes=()) -> bytes:
    key = enc.FolderKey(enc.FOLDER_KEY)
    body = (
        FOLDER
        + ENTRY
        + DEVICE.public
        + u(1)
        + key.id
        + chunk_list
        + bytes_(sealed)
        + trailing_vector(counters, holes)
    )
    return DEVICE.sign("sync/2/sealed-version", body)


def main():
    vectors = json.load(open("../encryption-v1.json"))
    ids = [bytes.fromhex(c["chunk_id"]) for c in vectors["chunks"]]
    sealed = bytes.fromhex(vectors["sealed_payload"]["sealed"])
    inline = variant(0, seq(ids, raw(32)))
    lists = variant(1, seq([bytes.fromhex(vectors["list_blob"]["list_id"]), b"\x45" * 32], raw(32)))

    version = sealed_version(inline, sealed, {DEVICE.public: 3})
    # Seen changes 1 and 3 to 5 of its author but not 2, and change 2 of another device.
    with_holes = sealed_version(
        inline, sealed, {DEVICE.public: 5, b"\x99" * 32: 2}, [(DEVICE.public, 2, 2)]
    )
    large = sealed_version(lists, sealed, {DEVICE.public: 1})
    encryption_key = bytes.fromhex(vectors["grant"]["signed_encryption_key"])
    grant = bytes.fromhex(vectors["grant"]["signed_key_grant"])

    item = lambda x: x  # noqa: E731
    messages = [
        (
            "hello",
            variant(0, certificate(), seq([FOLDER], raw(32)), i(1_758_600_000_000), seq([revocation()], item)),
        ),
        ("index-request-initial", variant(1, FOLDER, u(0), u(0))),
        (
            "index-batch",
            variant(
                2,
                FOLDER,
                u(0x0123_4567_89AB_CDEF),
                u(3),
                seq([variant(0, version), variant(1, encryption_key), variant(2, grant)], item),
                boolean(True),
            ),
        ),
        (
            "index-batch-with-holes-and-lists",
            variant(
                2,
                FOLDER,
                u(0x0123_4567_89AB_CDEF),
                u(5),
                seq([variant(0, with_holes), variant(0, large)], item),
                boolean(False),
            ),
        ),
        ("certificate-request", variant(3, FOLDER, DEVICE.public)),
        ("certificate-response", variant(4, certificate())),
        ("error-unknown-device", variant(5, variant(3))),
    ]
    out = {
        "alpn": "tidefell/sync/2",
        "spec": "specs/sync-v2.md",
        "encoding": "varint length prefix + postcard",
        "device_key_seed": "11" * 32,
        "user_key_seed": "77" * 32,
        "vectors": [{"name": name, "frame": frame(body).hex()} for name, body in messages],
    }
    print(json.dumps(out, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
