//! 認証のスタブ実装（原則7: 認証はスコープ外）。

use async_trait::async_trait;
use domain::VoterId;

use crate::ports::{AuthError, Authenticator, Credentials};

/// 入力 ID をそのまま `voter_id` として採用する。マイナンバー欄は受け取って破棄する。
#[derive(Debug, Default, Clone, Copy)]
pub struct StubAuthenticator;

#[async_trait]
impl Authenticator for StubAuthenticator {
    async fn authenticate(&self, credentials: Credentials) -> Result<VoterId, AuthError> {
        // 所有権ごと受け取り、マイナンバーは使わずにここで破棄（drop）する。
        let Credentials {
            voter_id,
            password: _,
            my_number: _,
        } = credentials;
        VoterId::new(&voter_id).map_err(|_| AuthError::InvalidCredentials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn adopts_input_id_as_voter_id_and_discards_my_number() {
        let voter = StubAuthenticator
            .authenticate(Credentials {
                voter_id: "alice".to_string(),
                password: Some("ignored".to_string()),
                my_number: Some("123456789012".to_string()),
            })
            .await
            .expect("valid");
        assert_eq!(voter.as_str(), "alice");
    }

    #[tokio::test]
    async fn rejects_malformed_voter_id() {
        let err = StubAuthenticator
            .authenticate(Credentials {
                voter_id: "a.b".to_string(),
                password: None,
                my_number: None,
            })
            .await
            .expect_err("invalid");
        assert_eq!(err, AuthError::InvalidCredentials);
    }

    #[test]
    fn credentials_debug_redacts_my_number() {
        let c = Credentials {
            voter_id: "alice".to_string(),
            password: Some("secret-pass".to_string()),
            my_number: Some("123456789012".to_string()),
        };
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("123456789012") && !shown.contains("secret-pass"),
            "{shown}"
        );
    }
}
