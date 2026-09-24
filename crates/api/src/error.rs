//! API のエラー応答。`error` は機械可読なコード、`message` は利用者向けの文言（呼び名は `labels` から作る）。
//! voter_id / candidate_id は含めない。

use application::{AuthError, ServiceError, StoreError};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use shared_types::ErrorResponse;

use std::sync::Arc;

use crate::state::{ApiLabels, AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiError {
    Unauthorized,
    NotFound,
    AlreadyVoted,
    /// 再投票を認めない選挙で、再投票が指定された（ADR 0022）。
    RevoteNotAllowed,
    /// 再投票の上限（`max` 回）に達している。
    RevoteLimitReached {
        max: u32,
    },
    /// 同時に送られた別の再投票が先に受理された。
    RevoteConflict,
    /// まだ投票していない投票用紙に、再投票が指定された。
    NotVotedYet,
    InvalidCandidate,
    /// 白票を受け付けない選挙（open の時点で固定した `vote.allow_blank` が偽）で、白票が指定された。
    BlankNotAllowed,
    /// 名簿で、この有権者に属さない投票用紙（名簿に無い有権者を含む）。
    NotEligible,
    Unavailable,
    /// 選んだ保存先では未対応の機能（例: scylla での改ざんデモ）。
    NotSupported,
    /// まだ開始していない（scheduled、または open だが開始時刻前。原則18）。
    VotingNotStarted,
    /// 締切の手続き中（closing）。
    VotingClosing,
    /// 終了している（closed、または open だが終了時刻以後）。
    VotingClosed,
}

impl ApiError {
    pub(crate) fn parts(self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::AlreadyVoted => (StatusCode::CONFLICT, "already_voted"),
            Self::RevoteNotAllowed => (StatusCode::CONFLICT, "revote_not_allowed"),
            Self::RevoteLimitReached { .. } => (StatusCode::CONFLICT, "revote_limit_reached"),
            Self::RevoteConflict => (StatusCode::CONFLICT, "revote_conflict"),
            Self::NotVotedYet => (StatusCode::CONFLICT, "not_voted"),
            Self::InvalidCandidate => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_candidate"),
            Self::BlankNotAllowed => (StatusCode::UNPROCESSABLE_ENTITY, "blank_not_allowed"),
            Self::NotEligible => (StatusCode::FORBIDDEN, "not_eligible"),
            Self::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Self::NotSupported => (StatusCode::NOT_IMPLEMENTED, "not_supported"),
            Self::VotingNotStarted => (StatusCode::FORBIDDEN, "voting_not_started"),
            Self::VotingClosing => (StatusCode::FORBIDDEN, "voting_closing"),
            Self::VotingClosed => (StatusCode::FORBIDDEN, "voting_closed"),
        }
    }

    /// 利用者向けの文言。呼び名・投票期間のメッセージは、設定の `labels`。
    pub(crate) fn message(self, labels: &ApiLabels) -> String {
        let ballot_item = &labels.ballot_item;
        match self {
            // ログインの失敗は、ID が存在しない場合もパスワード違いの場合も、この同じ文言（区別させない）。
            Self::Unauthorized => {
                "認証に失敗しました。ログイン ID とパスワードを確認してください。".to_string()
            }
            Self::NotFound => format!("{ballot_item}が見つかりません。"),
            Self::AlreadyVoted => format!("この{ballot_item}には投票済みです。"),
            Self::RevoteNotAllowed => {
                format!("この選挙では、{ballot_item}の投票をやり直せません。")
            }
            Self::RevoteLimitReached { max } => labels
                .revote_limit_reached
                .replace("{max}", &max.to_string()),
            Self::RevoteConflict => {
                "同時に送られた別のやり直しと重なりました。画面を開き直してください。".to_string()
            }
            Self::NotVotedYet => format!("この{ballot_item}には、まだ投票していません。"),
            Self::InvalidCandidate => format!("指定した候補者は、この{ballot_item}にいません。"),
            Self::BlankNotAllowed => format!("この選挙では、{}は選べません。", labels.blank_name),
            Self::NotEligible => format!("この{ballot_item}は、あなたの投票対象ではありません。"),
            Self::Unavailable => {
                "サービスが利用できません。しばらくしてからもう一度お試しください。".to_string()
            }
            Self::NotSupported => "この機能は、現在の設定では使えません。".to_string(),
            Self::VotingNotStarted => labels.voting_not_started_message.clone(),
            Self::VotingClosing => labels.voting_closing_message.clone(),
            Self::VotingClosed => labels.voting_closed_message.clone(),
        }
    }
}

impl IntoResponse for ApiError {
    /// 文言はここでは作らない（設定の `labels` はアプリケーション状態にある）。エラーの種類を拡張として
    /// 載せておき、`add_error_messages`（ミドルウェア）が本文を組み立てる。
    fn into_response(self) -> Response {
        let (status, code) = self.parts();
        let body = ErrorResponse {
            error: code.to_string(),
            message: String::new(),
        };
        let mut response = (status, Json(body)).into_response();
        response.extensions_mut().insert(self);
        response
    }
}

/// `ApiError` の応答に、利用者向けの `message`（`labels` から作る）を載せる。
pub async fn add_error_messages(
    State(state): State<Arc<AppState>>,
    response: Response,
) -> Response {
    let Some(error) = response.extensions().get::<ApiError>().copied() else {
        return response;
    };
    let (status, code) = error.parts();
    let body = ErrorResponse {
        error: code.to_string(),
        message: error.message(&state.labels),
    };
    let mut response = (status, Json(body)).into_response();
    // エラー（404 を含む）を、CDN やブラウザに保存させない: 後から、同じ URL が成功するようになることがある
    // （まだ封印されていないブロックなど）。
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(crate::chain_view::NO_STORE),
    );
    response
}

impl ApiError {
    /// 投票・再投票の失敗。上限のメッセージには、固定した選挙のルールの `max_revotes` を使う。
    pub(crate) fn from_service(e: ServiceError, rules: domain::ElectionRules) -> Self {
        match e {
            ServiceError::RevoteLimitReached => Self::RevoteLimitReached {
                max: rules.max_revotes,
            },
            other => other.into(),
        }
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::ContestNotFound => Self::NotFound,
            ServiceError::InvalidCandidate => Self::InvalidCandidate,
            ServiceError::BlankNotAllowed => Self::BlankNotAllowed,
            ServiceError::AlreadyVoted => Self::AlreadyVoted,
            ServiceError::RevoteNotAllowed => Self::RevoteNotAllowed,
            ServiceError::RevoteLimitReached => Self::RevoteLimitReached { max: 0 },
            ServiceError::RevoteConflict => Self::RevoteConflict,
            ServiceError::NotVotedYet => Self::NotVotedYet,
            ServiceError::NotEligible => Self::NotEligible,
            ServiceError::Unavailable => {
                tracing::error!("サービスが利用できません");
                Self::Unavailable
            }
            ServiceError::RevoteKeyUnavailable => {
                tracing::error!(
                    "再投票の鍵（secrets/revote_key）がありません。再投票を認める選挙の投票を受け付けられません"
                );
                Self::Unavailable
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(_: StoreError) -> Self {
        tracing::error!("ストアが利用できません");
        Self::Unavailable
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials => Self::Unauthorized,
            AuthError::Unavailable => {
                tracing::error!("認証基盤が利用できません");
                Self::Unavailable
            }
        }
    }
}
