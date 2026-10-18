"""Checks `wire.py` against sync/1's golden vectors, which come from the Rust code: the two
implementations agree on the encoding the new vectors build on.

    python3 check_wire.py
"""

import json

from wire import Key, boolean, frame, i, option, raw, seq, string, trailing_vector, u, variant

want = {v["name"]: v["frame"] for v in json.load(open("../sync-v1.json"))["vectors"]}
dev, user = Key(b"\x11" * 32), Key(b"\x77" * 32)
cert = user.sign(
    "identity/1/device-certificate",
    user.public + dev.public + string("test device") + i(1_758_600_000_000),
)
folder = b"\x22" * 32
hello = variant(
    0, cert, seq([folder], raw(32)), option(1_758_600_000_000, i), option([], lambda v: seq(v, bytes))
)
assert frame(hello).hex() == want["hello"]
version = (
    folder
    + b"\x33" * 16
    + string("docs/readme.txt")
    + variant(0, b"\x44" * 32, u(1234), boolean(False))
    + i(1_758_600_000_000)
    + dev.public
    + trailing_vector({dev.public: 3})
)
batch = variant(
    2,
    folder,
    u(0x0123_4567_89AB_CDEF),
    u(1),
    seq([dev.sign("sync/1/entry-version", version)], bytes),
    boolean(True),
)
assert frame(batch).hex() == want["index-batch"]
print("wire.py agrees with sync/1's vectors")
