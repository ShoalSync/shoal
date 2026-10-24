"""An encoder for Shoal's wire format written from the specs, independent of `crates/proto`,
so the golden vectors it makes check the Rust code rather than confirm it.

postcard 1.x: unsigned integers are LEB128 varints, signed ones zig-zag first; strings and
byte strings are a varint length then the bytes; fixed-size identifiers are raw bytes; enums are a
varint variant index then the fields; `Option` is one byte, 0 or 1, then the value.
"""

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

SIG_DOMAIN = b"shoal/sig/1\x00"


def varint(n: int) -> bytes:
    assert n >= 0
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def zigzag(n: int) -> bytes:
    return varint((n << 1) ^ (n >> 63) if n < 0 else n << 1)


def u(n: int) -> bytes:
    return varint(n)


def i(n: int) -> bytes:
    return zigzag(n)


def bytes_(b: bytes) -> bytes:
    return varint(len(b)) + b


def string(s: str) -> bytes:
    return bytes_(s.encode())


def boolean(v: bool) -> bytes:
    return b"\x01" if v else b"\x00"


def option(value, enc) -> bytes:
    return b"\x00" if value is None else b"\x01" + enc(value)


def seq(items, enc) -> bytes:
    items = list(items)
    return varint(len(items)) + b"".join(enc(x) for x in items)


def raw(n: int):
    def enc(b: bytes) -> bytes:
        assert len(b) == n, (len(b), n)
        return b

    return enc


def variant(index: int, *fields: bytes) -> bytes:
    return varint(index) + b"".join(fields)


def frame(body: bytes) -> bytes:
    return varint(len(body)) + body


def version_vector(counters: dict, holes=()) -> bytes:
    """The highest change seen per device, sorted, no zeros; then holes. Callers in the last
    field of a version leave the empty hole list out (sync/1, Version vectors)."""
    items = sorted((d, c) for d, c in counters.items() if c)
    m = varint(len(items)) + b"".join(d + varint(c) for d, c in items)
    h = seq(sorted(holes), lambda x: x[0] + varint(x[1]) + varint(x[2]))
    return m, h


def trailing_vector(counters: dict, holes=()) -> bytes:
    m, h = version_vector(counters, holes)
    return m if not holes else m + h


class Key:
    def __init__(self, seed: bytes):
        self.private = Ed25519PrivateKey.from_private_bytes(seed)
        self.public = self.private.public_key().public_bytes(
            serialization.Encoding.Raw, serialization.PublicFormat.Raw
        )

    def sign(self, kind: str, body: bytes) -> bytes:
        """`Signed<T>`: signer, body as bytes, signature over the domain, kind and body."""
        message = SIG_DOMAIN + bytes([len(kind)]) + kind.encode() + body
        return self.public + bytes_(body) + self.private.sign(message)
