//! `Authorization: Bearer <token>` からログイン中の投票者を取り出す抽出器。

use std::sync::Arc;

use application::PasswordChecker;
use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use domain::VoterId;

use crate::error::ApiError;
use crate::state::AppState;

pub struct AuthedVoter(pub VoterId);

impl FromRequestParts<Arc<AppState>> for AuthedVoter {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;
        state
            .sessions
            .verify(token, state.clock.now_unix_secs())
            .map(Self)
            .map_err(|_| ApiError::Unauthorized)
    }
}

/// パスワードの照合（Argon2 は重い計算）を、`spawn_blocking` に載せて、他のリクエストを止めないようにする。
#[derive(Debug, Default, Clone, Copy)]
pub struct BlockingChecker;

#[async_trait]
impl PasswordChecker for BlockingChecker {
    async fn check(&self, phc: &str, password: &str) -> bool {
        // 所有権を移して、ブロッキング用のスレッドで照合する（借用したままでは、`'static` を要求される）。
        let (phc, password) = (phc.to_string(), password.to_string());
        tokio::task::spawn_blocking(move || {
            application::credentials::verify_password(&phc, &password)
        })
        .await
        .unwrap_or(false)
    }
}
