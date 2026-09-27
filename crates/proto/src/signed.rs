//! Signed envelopes: a message body plus the signing device's key and Ed25519 signature.

use std::marker::PhantomData;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Error, PublicKey};

/// Prefix of every signed byte string, so Tidefell signatures cannot be confused with signatures
/// from other protocols.
pub const SIGNATURE_DOMAIN: &[u8] = b"tidefell/sig/1\0";

/// A message type that can be signed. `KIND` separates signatures over different types, so a
/// signature over one type never verifies as another.
pub trait SignedKind: Serialize + DeserializeOwned {
    /// Unique, stable kind tag. Never reuse or change a tag.
    const KIND: &'static str;

    /// The body to sign: the postcard encoding, unless the kind specifies another.
    fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("in-memory encoding cannot fail")
    }

    /// Decodes a verified body.
    fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }
}

/// A signed message. The body is kept as the exact bytes that were signed.
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Signed<T> {
    /// The signing key: a device or a user, depending on the message kind.
    pub signer: PublicKey,
    /// postcard encoding of the message.
    pub body: Vec<u8>,
    /// Ed25519 signature over [`signing_bytes`].
    #[serde(with = "crate::bytes")]
    pub signature: [u8; 64],
    #[serde(skip)]
    _kind: PhantomData<fn() -> T>,
}

impl<T> Clone for Signed<T> {
    fn clone(&self) -> Self {
        Self {
            signer: self.signer,
            body: self.body.clone(),
            signature: self.signature,
            _kind: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Signed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signed")
            .field("signer", &self.signer)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl<T> PartialEq for Signed<T> {
    fn eq(&self, other: &Self) -> bool {
        self.signer == other.signer && self.body == other.body && self.signature == other.signature
    }
}

impl<T> Eq for Signed<T> {}

/// The byte string a signature covers: domain, kind tag length and bytes, then the body.
pub fn signing_bytes(kind: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNATURE_DOMAIN.len() + 1 + kind.len() + body.len());
    out.extend_from_slice(SIGNATURE_DOMAIN);
    out.push(u8::try_from(kind.len()).expect("kind tags are short"));
    out.extend_from_slice(kind.as_bytes());
    out.extend_from_slice(body);
    out
}

impl<T: SignedKind> Signed<T> {
    /// Encodes and signs `message` with `key`.
    pub fn sign(message: &T, key: &SigningKey) -> Self {
        let body = message.encode();
        let signature = key.sign(&signing_bytes(T::KIND, &body));
        Self {
            signer: PublicKey(key.verifying_key().to_bytes()),
            body,
            signature: signature.to_bytes(),
            _kind: PhantomData,
        }
    }

    /// Verifies the signature and decodes the body.
    pub fn verify(&self) -> Result<T, Error> {
        let key =
            VerifyingKey::from_bytes(self.signer.as_bytes()).map_err(|_| Error::InvalidKey)?;
        key.verify_strict(
            &signing_bytes(T::KIND, &self.body),
            &Signature::from_bytes(&self.signature),
        )
        .map_err(|_| Error::BadSignature)?;
        Ok(T::decode(&self.body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Ping(u32);
    impl SignedKind for Ping {
        const KIND: &'static str = "test/ping";
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Pong(u32);
    impl SignedKind for Pong {
        const KIND: &'static str = "test/pong";
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[1; 32])
    }

    #[test]
    fn sign_and_verify() {
        let signed = Signed::sign(&Ping(42), &key());
        assert_eq!(signed.verify().unwrap(), Ping(42));
    }

    #[test]
    fn tampered_body_fails() {
        let mut signed = Signed::sign(&Ping(42), &key());
        signed.body[0] ^= 1;
        assert!(matches!(signed.verify(), Err(Error::BadSignature)));
    }

    #[test]
    fn kind_is_bound_into_signature() {
        let ping = Signed::sign(&Ping(42), &key());
        let as_pong: Signed<Pong> =
            postcard::from_bytes(&postcard::to_stdvec(&ping).unwrap()).unwrap();
        assert!(matches!(as_pong.verify(), Err(Error::BadSignature)));
    }
}
