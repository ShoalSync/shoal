"""HPKE (RFC 9180), base mode, for one suite: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
ChaCha20-Poly1305. Written from the RFC, independent of the Rust code, and checked against the
RFC's own test vector (appendix A.2.1) by `check()`."""

import hashlib
import hmac

from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives import serialization

KEM_ID, KDF_ID, AEAD_ID = 0x0020, 0x0001, 0x0003
KEM_SUITE = b"KEM" + KEM_ID.to_bytes(2, "big")
HPKE_SUITE = b"HPKE" + KEM_ID.to_bytes(2, "big") + KDF_ID.to_bytes(2, "big") + AEAD_ID.to_bytes(2, "big")
NK, NN, NH = 32, 12, 32


def extract(salt: bytes, ikm: bytes) -> bytes:
    return hmac.new(salt or b"\x00" * NH, ikm, hashlib.sha256).digest()


def expand(prk: bytes, info: bytes, length: int) -> bytes:
    out, t, n = b"", b"", 1
    while len(out) < length:
        t = hmac.new(prk, t + info + bytes([n]), hashlib.sha256).digest()
        out += t
        n += 1
    return out[:length]


def labeled_extract(suite: bytes, salt: bytes, label: bytes, ikm: bytes) -> bytes:
    return extract(salt, b"HPKE-v1" + suite + label + ikm)


def labeled_expand(suite: bytes, prk: bytes, label: bytes, info: bytes, length: int) -> bytes:
    return expand(prk, length.to_bytes(2, "big") + b"HPKE-v1" + suite + label + info, length)


def public(sk: bytes) -> bytes:
    return X25519PrivateKey.from_private_bytes(sk).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )


def derive_key_pair(ikm: bytes):
    prk = labeled_extract(KEM_SUITE, b"", b"dkp_prk", ikm)
    sk = labeled_expand(KEM_SUITE, prk, b"sk", b"", 32)
    return sk, public(sk)


def dh(sk: bytes, pk: bytes) -> bytes:
    return X25519PrivateKey.from_private_bytes(sk).exchange(X25519PublicKey.from_public_bytes(pk))


def extract_and_expand(dh_out: bytes, kem_context: bytes) -> bytes:
    prk = labeled_extract(KEM_SUITE, b"", b"eae_prk", dh_out)
    return labeled_expand(KEM_SUITE, prk, b"shared_secret", kem_context, 32)


def encap(pk_r: bytes, ikm_e: bytes):
    sk_e, pk_e = derive_key_pair(ikm_e)
    return extract_and_expand(dh(sk_e, pk_r), pk_e + pk_r), pk_e


def decap(enc: bytes, sk_r: bytes) -> bytes:
    return extract_and_expand(dh(sk_r, enc), enc + public(sk_r))


def key_schedule(shared_secret: bytes, info: bytes):
    psk_id_hash = labeled_extract(HPKE_SUITE, b"", b"psk_id_hash", b"")
    info_hash = labeled_extract(HPKE_SUITE, b"", b"info_hash", info)
    context = b"\x00" + psk_id_hash + info_hash
    secret = labeled_extract(HPKE_SUITE, shared_secret, b"secret", b"")
    key = labeled_expand(HPKE_SUITE, secret, b"key", context, NK)
    base_nonce = labeled_expand(HPKE_SUITE, secret, b"base_nonce", context, NN)
    return key, base_nonce


def seal(pk_r: bytes, info: bytes, aad: bytes, pt: bytes, ikm_e: bytes):
    """Single-shot seal (sequence number 0): returns (enc, ct)."""
    shared, enc = encap(pk_r, ikm_e)
    key, nonce = key_schedule(shared, info)
    return enc, ChaCha20Poly1305(key).encrypt(nonce, pt, aad)


def open_(enc: bytes, sk_r: bytes, info: bytes, aad: bytes, ct: bytes) -> bytes:
    key, nonce = key_schedule(decap(enc, sk_r), info)
    return ChaCha20Poly1305(key).decrypt(nonce, ct, aad)


def check():
    """RFC 9180, appendix A.2.1, sequence number 0."""
    h = bytes.fromhex
    info = h("4f6465206f6e2061204772656369616e2055726e")
    sk_r, pk_r = derive_key_pair(h("1ac01f181fdf9f352797655161c58b75c656a6cc2716dcb66372da835542e1df"))
    assert sk_r == h("8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb")
    assert pk_r == h("4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a")
    enc, ct = seal(
        pk_r,
        info,
        h("436f756e742d30"),
        h("4265617574792069732074727574682c20747275746820626561757479"),
        h("909a9b35d3dc4713a5e72a4da274b55d3d3821a37e5d099e74a647db583a904b"),
    )
    assert enc == h("1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a")
    assert ct == h(
        "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28"
    )
    assert open_(enc, sk_r, info, h("436f756e742d30"), ct) == h(
        "4265617574792069732074727574682c20747275746820626561757479"
    )


if __name__ == "__main__":
    check()
    print("RFC 9180 A.2.1 ok")
