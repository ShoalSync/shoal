"""Golden encodings for specs/storage-v1.md, written from the spec, independent of the Rust code.

Run from this directory with Python 3 and the blake3, cryptography and pynacl packages:

    python3 storage_v1.py > ../storage-v1.json
"""

import json

import sync_v2
from wire import frame, i, raw, seq, u, variant

FOLDER = sync_v2.FOLDER


def hint(h) -> bytes:
    entry, priority = h
    # A priority is one byte, as postcard writes a u8.
    return raw(16)(entry) + bytes([priority])


def main():
    hints = [(b"\x11" * 16, 200), (b"\x22" * 16, 0), (b"\x33" * 16, 128)]
    messages = [
        ("hello", variant(0, sync_v2.certificate())),
        ("priorities", variant(1, FOLDER, i(1_792_000_000_000), seq(hints, hint))),
        ("accepted", variant(2)),
        ("error-not-stored", variant(3, variant(1))),
        ("usage-request", variant(4, FOLDER)),
        ("usage", variant(5, u(100_000_000_000), u(42_000_000_000), u(58_000_000_000))),
        # `below` is one byte, as postcard writes a u8.
        ("drop", variant(6, FOLDER, bytes([100]))),
        ("dropped", variant(7, u(1234), u(321_000_000))),
    ]
    out = {
        "alpn": "shoal/storage/1",
        "spec": "specs/storage-v1.md",
        "encoding": "varint length prefix + postcard",
        "vectors": [{"name": name, "frame": frame(body).hex()} for name, body in messages],
    }
    print(json.dumps(out, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
