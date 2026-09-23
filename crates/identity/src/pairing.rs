//! Pairing secrets, proofs and confirmation codes for `tidefell/pair/1` (see
//! `specs/pair-v1.md`).

use tidefell_proto::DeviceId;

use crate::Error;

const KEY_CONTEXT: &str = "tidefell pair/1 secret";

fn key(secret: &[u8; 16]) -> [u8; 32] {
    blake3::derive_key(KEY_CONTEXT, secret)
}

fn tag(secret: &[u8; 16], label: &[u8], approver: &DeviceId, new: &DeviceId) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(&key(secret));
    hasher.update(label);
    hasher.update(approver.as_bytes());
    hasher.update(new.as_bytes());
    hasher.finalize()
}

/// A fresh random pairing secret.
pub fn new_secret() -> Result<[u8; 16], Error> {
    let mut secret = [0u8; 16];
    getrandom::fill(&mut secret).map_err(|e| Error::Random(e.to_string()))?;
    Ok(secret)
}

/// The approver's proof that it has seen the code with `secret`.
pub fn proof(secret: &[u8; 16], approver: &DeviceId, new: &DeviceId) -> [u8; 32] {
    *tag(secret, b"proof", approver, new).as_bytes()
}

/// Checks a proof in constant time.
pub fn verify_proof(
    secret: &[u8; 16],
    approver: &DeviceId,
    new: &DeviceId,
    proof: &[u8; 32],
) -> bool {
    tag(secret, b"proof", approver, new) == blake3::Hash::from_bytes(*proof)
}

/// The six-digit code both devices show so the person can check they are pairing the right two.
pub fn confirmation_code(secret: &[u8; 16], approver: &DeviceId, new: &DeviceId) -> String {
    let bytes = tag(secret, b"confirm", approver, new);
    let n = u32::from_le_bytes(bytes.as_bytes()[..4].try_into().expect("4 bytes"));
    format!("{:06}", n % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: DeviceId = DeviceId([0xaa; 32]);
    const N: DeviceId = DeviceId([0xbb; 32]);
    const SECRET: [u8; 16] = [0x5a; 16];

    #[test]
    fn proofs_bind_the_secret_and_both_devices() {
        let p = proof(&SECRET, &A, &N);
        assert!(verify_proof(&SECRET, &A, &N, &p));
        assert!(!verify_proof(&[0; 16], &A, &N, &p));
        assert!(!verify_proof(&SECRET, &N, &A, &p));
        assert!(!verify_proof(&SECRET, &A, &DeviceId([0xcc; 32]), &p));
    }

    #[test]
    fn confirmation_codes_are_six_digits_and_stable() {
        let code = confirmation_code(&SECRET, &A, &N);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(code, confirmation_code(&SECRET, &A, &DeviceId([0xcc; 32])));
        // Fixed by the spec; a change here breaks pairing between versions.
        assert_eq!(code, confirmation_code(&SECRET, &A, &N));
        assert_eq!(hex(&proof(&SECRET, &A, &N))[..16], *EXPECTED_PROOF_PREFIX);
        assert_eq!(code, EXPECTED_CODE);
    }

    const EXPECTED_PROOF_PREFIX: &str = "d4eeaef2fda0daad";
    const EXPECTED_CODE: &str = "101467";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
