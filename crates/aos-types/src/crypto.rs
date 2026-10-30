//! ECDSA P-256. The signed message is hashed once with SHA-256.
//! Signature bytes are raw `r || s` (64 bytes). Public keys are uncompressed
//! SEC1 (65 bytes).

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};

pub fn sign(seed32: &[u8; 32], message: &[u8]) -> Result<[u8; 64], ()> {
    let sk = SigningKey::from_slice(seed32).map_err(|_| ())?;
    let sig: Signature = sk.sign(message);
    let bytes = sig.to_bytes();
    let mut out = [0u8; 64];
    out.copy_from_slice(&bytes);
    Ok(out)
}

pub fn public_uncompressed(seed32: &[u8; 32]) -> Result<[u8; 65], ()> {
    let sk = SigningKey::from_slice(seed32).map_err(|_| ())?;
    let vk = VerifyingKey::from(&sk);
    let enc = vk.to_encoded_point(false);
    let b = enc.as_bytes();
    if b.len() != 65 {
        return Err(());
    }
    let mut out = [0u8; 65];
    out.copy_from_slice(b);
    Ok(out)
}

pub fn verify(public_key: &[u8], message: &[u8], sig64: &[u8]) -> bool {
    if sig64.len() != 64 {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_sec1_bytes(public_key) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(sig64) else {
        return false;
    };
    vk.verify(message, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let seed = [7u8; 32];
        let msg = b"approve-me";
        let pk = public_uncompressed(&seed).unwrap();
        let sig = sign(&seed, msg).unwrap();
        assert!(verify(&pk, msg, &sig));
        assert!(!verify(&pk, b"other", &sig));
    }
}
