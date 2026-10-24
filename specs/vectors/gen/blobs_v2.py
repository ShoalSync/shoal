"""Golden encodings for specs/blobs-v2.md, written from the spec, independent of the Rust code.

Run from this directory with Python 3 and the blake3, cryptography and pynacl packages:

    python3 blobs_v2.py > ../blobs-v2.json
"""

import json

import sync_v2
from wire import boolean, bytes_, frame, raw, seq, u, variant

FOLDER = sync_v2.FOLDER


def main():
    vectors = json.load(open("../encryption-v1.json"))
    chunk = vectors["chunks"][1]
    chunk_id, sealed = bytes.fromhex(chunk["chunk_id"]), bytes.fromhex(chunk["sealed"])
    list_id = bytes.fromhex(vectors["list_blob"]["list_id"])
    list_blob = bytes.fromhex(vectors["list_blob"]["blob"])
    messages = [
        ("hello", variant(0, sync_v2.certificate())),
        ("request", variant(1, FOLDER, seq([chunk_id, list_id], raw(32)))),
        ("blob-sealed-chunk", variant(2, chunk_id, bytes_(sealed))),
        ("blob-list", variant(2, list_id, bytes_(list_blob))),
        ("missing", variant(3, b"\x55" * 32)),
        ("availability-request", variant(4, FOLDER, seq([chunk_id, list_id, b"\x55" * 32], raw(32)))),
        ("availability", variant(5, bytes_(bytes([0b011])))),
        ("error-too-many-blobs", variant(6, variant(2))),
        ("replica-request", variant(7, FOLDER)),
        ("replica", variant(8, boolean(True))),
        ("busy", variant(9, u(1500))),
    ]
    out = {
        "alpn": "shoal/blobs/2",
        "spec": "specs/blobs-v2.md",
        "encoding": "varint length prefix + postcard",
        "vectors": [{"name": name, "frame": frame(body).hex()} for name, body in messages],
    }
    print(json.dumps(out, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
