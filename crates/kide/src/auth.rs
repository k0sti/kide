//! The Nostr-key challenge of §4.2: a client proves it holds a key by a
//! BIP-340 signature over the server's name and the server's challenge.

use crate::error::{Result, invalid};
use crate::hash::{Hash, h};
use k256::schnorr::{Signature, SigningKey, VerifyingKey};

/// An x-only public key (BIP-340), as Nostr uses.
pub type PeerKey = [u8; 32];

/// A Nostr secret key, which answers challenges.
pub struct Secret(SigningKey);

impl Secret {
    /// `Invalid` unless `bytes` is a valid secp256k1 secret key.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Secret> {
        match SigningKey::from_slice(bytes) {
            Ok(k) => Ok(Secret(k)),
            Err(_) => invalid("not a secp256k1 secret key"),
        }
    }

    /// The secret's bytes, for a device that keeps its key.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes().into()
    }

    pub fn public(&self) -> PeerKey {
        self.0.verifying_key().to_bytes().into()
    }

    /// The signature that answers `challenge` from the server `name`.
    pub fn answer(&self, name: &str, challenge: &[u8; 32]) -> Result<[u8; 64]> {
        let d = digest(name, challenge)?;
        // Zero auxiliary randomness is allowed by BIP-340: the nonce still
        // depends on the secret key and the message.
        match self.0.sign_raw(&d.0, &[0; 32]) {
            Ok(s) => Ok(s.to_bytes()),
            Err(_) => invalid("signing failed"),
        }
    }
}

/// What a client signs: H("kide auth" ‖ 0 ‖ name ‖ 0 ‖ challenge). A name
/// holding a NUL byte is `Invalid`.
pub fn digest(name: &str, challenge: &[u8; 32]) -> Result<Hash> {
    if name.as_bytes().contains(&0) {
        return invalid("a server name with a NUL byte");
    }
    let mut m = b"kide auth\0".to_vec();
    m.extend(name.as_bytes());
    m.push(0);
    m.extend(challenge);
    Ok(h(&m))
}

/// Whether `sig` by `key` answers `challenge` from the server `name`.
pub fn verify(key: &PeerKey, name: &str, challenge: &[u8; 32], sig: &[u8; 64]) -> bool {
    let (Ok(d), Ok(k), Ok(s)) = (
        digest(name, challenge),
        VerifyingKey::from_slice(key),
        Signature::try_from(&sig[..]),
    ) else {
        return false;
    };
    k.verify_raw(&d.0, &s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(i: u8) -> Secret {
        Secret::from_bytes(&h(&[i]).0).unwrap()
    }

    #[test]
    fn a_secret_survives_its_bytes() {
        let s = secret(3);
        let back = Secret::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(back.public(), s.public());
    }

    #[test]
    fn an_answer_verifies_only_for_its_key_name_and_challenge() {
        let (s, c) = (secret(1), [7; 32]);
        let sig = s.answer("edge.example", &c).unwrap();
        assert!(verify(&s.public(), "edge.example", &c, &sig));
        assert!(
            !verify(&secret(2).public(), "edge.example", &c, &sig),
            "key"
        );
        assert!(!verify(&s.public(), "other.example", &c, &sig), "name");
        assert!(
            !verify(&s.public(), "edge.example", &[8; 32], &sig),
            "challenge"
        );
        let mut bent = sig;
        bent[10] ^= 1;
        assert!(!verify(&s.public(), "edge.example", &c, &bent), "signature");
        assert!(!verify(&[0; 32], "edge.example", &c, &sig), "not a key");
    }

    #[test]
    fn answers_are_deterministic_and_names_are_checked() {
        let s = secret(3);
        let c = [1; 32];
        assert_eq!(s.answer("a", &c).unwrap(), s.answer("a", &c).unwrap());
        assert!(s.answer("a\0b", &c).is_err());
        assert!(Secret::from_bytes(&[0; 32]).is_err(), "zero is no key");
        assert!(Secret::from_bytes(&[0xff; 32]).is_err(), "beyond the order");
    }

    /// BIP-340 test vector 0: secret key 3, zero auxiliary randomness,
    /// the all-zero message.
    #[test]
    fn bip340_vector_0() {
        let mut sk = [0; 32];
        sk[31] = 3;
        let k = SigningKey::from_slice(&sk).unwrap();
        let sig = k.sign_raw(&[0; 32], &[0; 32]).unwrap().to_bytes();
        let want = "e907831f80848d1069a5371b402410364bdf1c5f8307b0084c55f1ce2dca8215\
                    25f66a4a85ea8b71e482a74f382d2ce5ebeee8fdb2172f477df4900d310536c0";
        assert_eq!(crate::hash::to_hex(&sig), want);
    }
}
