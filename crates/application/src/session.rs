//! セッショントークン（HMAC 署名）。サーバ側にセッション状態を持たない（原則6）。
//!
//! 形式: `v1.<voter_id>.<exp_unix_secs>.<base64url(HMAC-SHA256(secret, "v1.<voter_id>.<exp>"))>`
//!
//! `voter_id` は `.` を含まない（`VoterId` の検証による）ので、`.` 区切りで曖昧さなく分割できる。
//! 署名鍵を共有する限り、どの API インスタンスでも検証できる。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use domain::VoterId;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const TOKEN_VERSION: &str = "v1";
/// 署名鍵の最小長（バイト）。
pub const MIN_SECRET_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("署名鍵は {MIN_SECRET_LEN} バイト以上が必要です")]
    SecretTooShort,
    #[error("トークンが不正です")]
    Invalid,
    #[error("トークンの有効期限が切れています")]
    Expired,
}

/// 発行したトークンと、その有効秒数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToken {
    pub token: String,
    pub expires_in_secs: u64,
}

#[derive(Clone)]
pub struct SessionSigner {
    /// 鍵を設定済みの HMAC。署名・検証のたびに複製して使う。
    mac: HmacSha256,
    ttl_secs: u64,
}

// 署名鍵を含むため、Debug では中身を出さない。
impl std::fmt::Debug for SessionSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSigner")
            .field("ttl_secs", &self.ttl_secs)
            .finish_non_exhaustive()
    }
}

impl SessionSigner {
    pub fn new(secret: &[u8], ttl_secs: u64) -> Result<Self, SessionError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(SessionError::SecretTooShort);
        }
        // HMAC は任意長の鍵を受け付けるので、ここで失敗することはない。
        let mac = HmacSha256::new_from_slice(secret).map_err(|_| SessionError::SecretTooShort)?;
        Ok(Self { mac, ttl_secs })
    }

    pub fn issue(&self, voter: &VoterId, now_unix_secs: u64) -> SessionToken {
        let expires_at = now_unix_secs.saturating_add(self.ttl_secs);
        let payload = format!("{TOKEN_VERSION}.{}.{expires_at}", voter.as_str());
        let tag = self.tag(&payload);
        SessionToken {
            token: format!("{payload}.{}", URL_SAFE_NO_PAD.encode(tag)),
            expires_in_secs: self.ttl_secs,
        }
    }

    pub fn verify(&self, token: &str, now_unix_secs: u64) -> Result<VoterId, SessionError> {
        let mut parts = token.split('.');
        let (Some(version), Some(voter), Some(exp), Some(tag_b64), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(SessionError::Invalid);
        };
        if version != TOKEN_VERSION {
            return Err(SessionError::Invalid);
        }
        let tag = URL_SAFE_NO_PAD
            .decode(tag_b64)
            .map_err(|_| SessionError::Invalid)?;

        // 署名を先に検証する（定数時間比較）。改ざんされた期限や ID を信用しない。
        let payload = format!("{version}.{voter}.{exp}");
        let mut mac = self.mac.clone();
        mac.update(payload.as_bytes());
        mac.verify_slice(&tag).map_err(|_| SessionError::Invalid)?;

        let expires_at: u64 = exp.parse().map_err(|_| SessionError::Invalid)?;
        if now_unix_secs >= expires_at {
            return Err(SessionError::Expired);
        }
        VoterId::new(voter).map_err(|_| SessionError::Invalid)
    }

    fn tag(&self, payload: &str) -> Vec<u8> {
        let mut mac = self.mac.clone();
        mac.update(payload.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn signer() -> SessionSigner {
        SessionSigner::new(SECRET, 3600).expect("valid secret")
    }

    fn voter() -> VoterId {
        VoterId::new("alice-01").expect("valid")
    }

    #[test]
    fn issue_then_verify_roundtrip() {
        let s = signer();
        let t = s.issue(&voter(), 1_000);
        assert_eq!(t.expires_in_secs, 3600);
        assert_eq!(s.verify(&t.token, 1_001), Ok(voter()));
    }

    #[test]
    fn expiry_boundary() {
        let s = signer();
        let t = s.issue(&voter(), 1_000).token;
        assert_eq!(s.verify(&t, 1_000 + 3599), Ok(voter()));
        assert_eq!(s.verify(&t, 1_000 + 3600), Err(SessionError::Expired));
    }

    #[test]
    fn rejects_tampered_voter_or_expiry() {
        let s = signer();
        let t = s.issue(&voter(), 1_000).token;
        let forged_voter = t.replacen("alice-01", "bob", 1);
        assert_eq!(s.verify(&forged_voter, 1_001), Err(SessionError::Invalid));
        let forged_exp = t.replacen("4600", "9999999999", 1);
        assert_ne!(forged_exp, t);
        assert_eq!(s.verify(&forged_exp, 1_001), Err(SessionError::Invalid));
    }

    #[test]
    fn rejects_token_signed_with_other_secret() {
        let other = SessionSigner::new(b"ffffffffffffffffffffffffffffffff", 3600).expect("valid");
        let t = other.issue(&voter(), 1_000).token;
        assert_eq!(signer().verify(&t, 1_001), Err(SessionError::Invalid));
    }

    #[test]
    fn rejects_malformed_tokens() {
        let s = signer();
        let good = s.issue(&voter(), 1_000).token;
        for bad in [
            "",
            "garbage",
            "v1.alice.100",
            "v2.alice.100.AAAA",
            "v1.alice.100.!!!!",
            &format!("{good}.extra"),
            &good.replacen("v1", "v9", 1),
        ] {
            assert_eq!(s.verify(bad, 1_001), Err(SessionError::Invalid), "{bad:?}");
        }
    }

    #[test]
    fn rejects_short_secret() {
        assert_eq!(
            SessionSigner::new(b"short", 60).err(),
            Some(SessionError::SecretTooShort)
        );
    }

    #[test]
    fn debug_does_not_expose_key() {
        let shown = format!("{:?}", signer());
        assert!(!shown.contains("0123456789abcdef"));
    }
}
