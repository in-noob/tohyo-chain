//! ブロック署名。`Signer` / `Verifier` トレイトと Ed25519 実装。
//!
//! トレイトは生のバイト列だけを扱い、ed25519-dalek の型を公開 API に出さない。
//! これにより、署名方式の差し替えやテスト用のスタブが作りやすい。

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};

use crate::types::SignatureBytes;

pub type PublicKeyBytes = [u8; 32];

/// 署名の検証に失敗した（または公開鍵が不正だった）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("署名が不正です")]
pub struct SignatureError;

pub trait Signer {
    fn sign(&self, message: &[u8]) -> SignatureBytes;
    fn public_key(&self) -> PublicKeyBytes;
}

pub trait Verifier {
    fn verify(&self, message: &[u8], signature: &SignatureBytes) -> Result<(), SignatureError>;
}

/// Ed25519 の署名者。秘密鍵の種（32 バイト）は呼び出し側が用意する。
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(seed),
        }
    }

    /// 対応する検証者を作る。
    pub fn verifier(&self) -> Ed25519Verifier {
        Ed25519Verifier {
            key: self.key.verifying_key(),
        }
    }
}

impl Signer for Ed25519Signer {
    fn sign(&self, message: &[u8]) -> SignatureBytes {
        self.key.sign(message).to_bytes()
    }

    fn public_key(&self) -> PublicKeyBytes {
        self.key.verifying_key().to_bytes()
    }
}

pub struct Ed25519Verifier {
    key: VerifyingKey,
}

impl Ed25519Verifier {
    pub fn from_public_key(bytes: &PublicKeyBytes) -> Result<Self, SignatureError> {
        VerifyingKey::from_bytes(bytes)
            .map(|key| Self { key })
            .map_err(|_| SignatureError)
    }
}

impl Verifier for Ed25519Verifier {
    fn verify(&self, message: &[u8], signature: &SignatureBytes) -> Result<(), SignatureError> {
        // strict: 非正準な署名（malleability）を拒否する。
        let sig = ed25519_dalek::Signature::from_bytes(signature);
        self.key
            .verify_strict(message, &sig)
            .map_err(|_| SignatureError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let signer = Ed25519Signer::from_seed(&[7u8; 32]);
        let verifier = signer.verifier();
        let sig = signer.sign(b"tohyo-chain");
        assert_eq!(verifier.verify(b"tohyo-chain", &sig), Ok(()));
    }

    #[test]
    fn rejects_wrong_message_signature_and_key() {
        let signer = Ed25519Signer::from_seed(&[7u8; 32]);
        let verifier = signer.verifier();
        let sig = signer.sign(b"tohyo-chain");
        assert_eq!(verifier.verify(b"tohyo-chaiN", &sig), Err(SignatureError));

        let mut bad = sig;
        bad[10] ^= 1;
        assert_eq!(verifier.verify(b"tohyo-chain", &bad), Err(SignatureError));

        let other = Ed25519Signer::from_seed(&[8u8; 32]).verifier();
        assert_eq!(other.verify(b"tohyo-chain", &sig), Err(SignatureError));
    }

    #[test]
    fn verifier_from_public_key_matches() {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let verifier = Ed25519Verifier::from_public_key(&signer.public_key()).expect("valid key");
        assert_eq!(verifier.verify(b"m", &signer.sign(b"m")), Ok(()));
    }
}
