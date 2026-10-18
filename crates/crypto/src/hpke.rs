//! HPKE (RFC 9180), base mode, single-shot, for one suite: DHKEM(X25519, HKDF-SHA256),
//! HKDF-SHA256, ChaCha20-Poly1305. Only what key grants need; checked against the RFC's own test
//! vector for this suite.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{Error, Result};

const KEM_SUITE: &[u8] = b"KEM\x00\x20";
const HPKE_SUITE: &[u8] = b"HPKE\x00\x20\x00\x01\x00\x03";

/// LabeledExtract: the pseudorandom key.
fn labeled_extract(suite: &[u8], salt: &[u8], label: &[u8], ikm: &[u8]) -> [u8; 32] {
    let labeled = [b"HPKE-v1".as_slice(), suite, label, ikm].concat();
    let (prk, _) = Hkdf::<Sha256>::extract(Some(salt), &labeled);
    prk.into()
}

/// LabeledExpand of `N` bytes, at most 32 here: far below HKDF-SHA256's limit.
fn labeled_expand<const N: usize>(
    prk: &[u8; 32],
    suite: &[u8],
    label: &[u8],
    info: &[u8],
) -> [u8; N] {
    let len = u16::try_from(N).unwrap_or(u16::MAX).to_be_bytes();
    let labeled = [len.as_slice(), b"HPKE-v1", suite, label, info].concat();
    let mut out = [0u8; N];
    if let Ok(hkdf) = Hkdf::<Sha256>::from_prk(prk) {
        let _ = hkdf.expand(&labeled, &mut out);
    }
    out
}

/// The key pair DHKEM derives from input keying material (RFC 9180, 7.1.3).
pub(crate) fn derive_key_pair(ikm: &[u8]) -> StaticSecret {
    let prk = labeled_extract(KEM_SUITE, b"", b"dkp_prk", ikm);
    StaticSecret::from(labeled_expand::<32>(&prk, KEM_SUITE, b"sk", b""))
}

fn shared_secret(dh: &[u8; 32], enc: &[u8; 32], recipient: &[u8; 32]) -> [u8; 32] {
    let prk = labeled_extract(KEM_SUITE, b"", b"eae_prk", dh);
    labeled_expand(
        &prk,
        KEM_SUITE,
        b"shared_secret",
        &[enc.as_slice(), recipient].concat(),
    )
}

fn key_schedule(shared: &[u8; 32], info: &[u8]) -> (ChaCha20Poly1305, [u8; 12]) {
    let psk_id_hash = labeled_extract(HPKE_SUITE, b"", b"psk_id_hash", b"");
    let info_hash = labeled_extract(HPKE_SUITE, b"", b"info_hash", info);
    let context = [&[0u8][..], &psk_id_hash, &info_hash].concat();
    let secret = labeled_extract(HPKE_SUITE, shared, b"secret", b"");
    let key: [u8; 32] = labeled_expand(&secret, HPKE_SUITE, b"key", &context);
    let nonce: [u8; 12] = labeled_expand(&secret, HPKE_SUITE, b"base_nonce", &context);
    (ChaCha20Poly1305::new(&key.into()), nonce)
}

/// Seals `plaintext` to `recipient` with the ephemeral key derived from `ikm_e`: the
/// encapsulated key and the ciphertext.
pub(crate) fn seal(
    recipient: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    ikm_e: &[u8],
) -> Result<([u8; 32], Vec<u8>)> {
    let ephemeral = derive_key_pair(ikm_e);
    let enc = PublicKey::from(&ephemeral).to_bytes();
    let dh = ephemeral.diffie_hellman(&PublicKey::from(*recipient));
    if !dh.was_contributory() {
        return Err(Error::Malformed("encryption key"));
    }
    let (aead, nonce) = key_schedule(&shared_secret(dh.as_bytes(), &enc, recipient), info);
    let ciphertext = aead
        .encrypt(
            &nonce.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Malformed("grant"))?;
    Ok((enc, ciphertext))
}

/// Opens what [`seal`] sealed to `secret`'s public key.
pub(crate) fn open(
    secret: &StaticSecret,
    enc: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let dh = secret.diffie_hellman(&PublicKey::from(*enc));
    if !dh.was_contributory() {
        return Err(Error::Open);
    }
    let recipient = PublicKey::from(secret).to_bytes();
    let (aead, nonce) = key_schedule(&shared_secret(dh.as_bytes(), enc, &recipient), info);
    aead.decrypt(
        &nonce.into(),
        Payload {
            msg: ciphertext,
            aad,
        },
    )
    .map_err(|_| Error::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 9180, appendix A.2.1, sequence number 0.
    #[test]
    fn rfc_9180_a_2_1() {
        let info = unhex("4f6465206f6e2061204772656369616e2055726e");
        let aad = unhex("436f756e742d30");
        let pt = unhex("4265617574792069732074727574682c20747275746820626561757479");
        let sk_r = derive_key_pair(&unhex(
            "1ac01f181fdf9f352797655161c58b75c656a6cc2716dcb66372da835542e1df",
        ));
        assert_eq!(
            sk_r.to_bytes().to_vec(),
            unhex("8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb")
        );
        let pk_r = PublicKey::from(&sk_r).to_bytes();
        assert_eq!(
            pk_r.to_vec(),
            unhex("4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a")
        );
        let (enc, ct) = seal(
            &pk_r,
            &info,
            &aad,
            &pt,
            &unhex("909a9b35d3dc4713a5e72a4da274b55d3d3821a37e5d099e74a647db583a904b"),
        )
        .unwrap();
        assert_eq!(
            enc.to_vec(),
            unhex("1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a")
        );
        assert_eq!(
            ct,
            unhex("1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28")
        );
        assert_eq!(open(&sk_r, &enc, &info, &aad, &ct).unwrap(), pt);
        assert_eq!(open(&sk_r, &enc, b"other", &aad, &ct), Err(Error::Open));
    }
}
