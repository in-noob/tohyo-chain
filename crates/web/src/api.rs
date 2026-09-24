//! `/api/v1` のクライアント。開発時は `trunk serve` のプロキシが `/api` を API サーバへ転送する
//! ので、URL は相対パスにする（オリジンを焼き込まない）。
//!
//! トークンや投票内容をログに出さない。

use gloo_net::http::Request;
use serde::de::DeserializeOwned;
use shared_types::{
    AnchorsResponse, BallotStatusResponse, BlockDto, BlocksPageResponse, CandidatesResponse,
    ChainsResponse, ElectionStatusResponse, ErrorResponse, LoginRequest, LoginResponse,
    VoteRequest,
};

use crate::error::{ApiFailure, classify_error, classify_status};

const BASE: &str = "/api/v1";

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// リクエストを送り、成功ならボディを JSON として読む。
async fn send_json<T: DeserializeOwned>(
    request: Result<Request, gloo_net::Error>,
) -> Result<T, ApiFailure> {
    // リクエストを組み立てられないのはクライアント側の不具合（想定外）。
    let request = request.map_err(|_| ApiFailure::Unexpected(0))?;
    let response = request.send().await.map_err(|_| ApiFailure::Network)?;
    let status = response.status();
    if let Some(failure) = classify_status(status) {
        return Err(failure);
    }
    response
        .json::<T>()
        .await
        .map_err(|_| ApiFailure::Unexpected(status))
}

/// ログイン。パスワードは `auth.mode=db` のときだけ使われる。マイナンバー欄は、API が受け取って破棄する。
pub async fn login(
    login_id: &str,
    password: Option<&str>,
    my_number: Option<&str>,
) -> Result<LoginResponse, ApiFailure> {
    let body = LoginRequest {
        login_id: login_id.to_string(),
        password: password.map(str::to_string),
        my_number: my_number.map(str::to_string),
    };
    send_json(Request::post(&format!("{BASE}/login")).json(&body)).await
}

/// 選挙状態（scheduled/open/closing/closed）と投票の受付期間（認証不要。原則17・18）。
/// ログイン画面・進捗画面が、期間と今の状態を表示するのに使う。
pub async fn election_status() -> Result<ElectionStatusResponse, ApiFailure> {
    send_json(Request::get(&format!("{BASE}/election-status")).build()).await
}

/// 有権者に関係する投票用紙だけが、表示順（固定）で返る。
pub async fn ballot_status(token: &str) -> Result<BallotStatusResponse, ApiFailure> {
    send_json(
        Request::get(&format!("{BASE}/ballot-status"))
            .header("Authorization", &bearer(token))
            .build(),
    )
    .await
}

/// `contest_id` は `{election_id}/{district_id}`（パスの 2 つのセグメントになる）。
pub async fn candidates(token: &str, contest_id: &str) -> Result<CandidatesResponse, ApiFailure> {
    send_json(
        Request::get(&format!("{BASE}/contests/{contest_id}/candidates"))
            .header("Authorization", &bearer(token))
            .build(),
    )
    .await
}

/// 投票する。`revote` は、やり直しのときだけ、画面が見たこの投票用紙の受理済みの票の数（ADR 0022）。
/// 成功（201）のとき `Ok(())`。成功の応答本文は読まない（`ballot_id` などは返らない）。失敗は、エラー応答の
/// `error`（コード）でも区別する（409 の、投票済み・やり直しの競合・上限）。
pub async fn vote(
    token: &str,
    contest_id: &str,
    candidate_id: &str,
    revote: Option<u32>,
) -> Result<(), ApiFailure> {
    let request = Request::post(&format!("{BASE}/contests/{contest_id}/vote"))
        .header("Authorization", &bearer(token))
        .json(&VoteRequest {
            candidate_id: candidate_id.to_string(),
            revote,
        })
        .map_err(|_| ApiFailure::Unexpected(0))?;
    let response = request.send().await.map_err(|_| ApiFailure::Network)?;
    let status = response.status();
    if classify_status(status).is_none() {
        return Ok(());
    }
    let code = response.json::<ErrorResponse>().await.ok().map(|e| e.error);
    Err(classify_error(status, code.as_deref()).unwrap_or(ApiFailure::Unexpected(status)))
}

// ---------------------------------------------------------------------------
// ブロックチェーンのビューア（公開データ。ログイン不要）
// ---------------------------------------------------------------------------

/// ブロックの一覧の、1 ページの件数。
pub const BLOCKS_PER_PAGE: usize = 20;
/// アンカーの一覧の件数。
pub const ANCHORS_LIMIT: usize = 50;

/// シャードの一覧と、それぞれの先頭ブロック。
pub async fn chains() -> Result<ChainsResponse, ApiFailure> {
    send_json(Request::get(&format!("{BASE}/chains")).build()).await
}

/// ブロックの要約を新しい順に 1 ページ分。`before_height` より低い高さのものを返す。
pub async fn blocks_page(
    shard: u16,
    before_height: Option<u64>,
) -> Result<BlocksPageResponse, ApiFailure> {
    let mut url = format!("{BASE}/chains/{shard}/blocks?limit={BLOCKS_PER_PAGE}");
    if let Some(before) = before_height {
        url.push_str(&format!("&before_height={before}"));
    }
    send_json(Request::get(&url).build()).await
}

/// ブロックの詳細。票の中身は、API が公開してよいときだけ入っている（`ballots_revealed`）。
pub async fn block(shard: u16, height: u64) -> Result<BlockDto, ApiFailure> {
    send_json(Request::get(&format!("{BASE}/chains/{shard}/blocks/{height}")).build()).await
}

/// アンカーを新しい順に。
pub async fn anchors() -> Result<AnchorsResponse, ApiFailure> {
    send_json(Request::get(&format!("{BASE}/anchors?limit={ANCHORS_LIMIT}")).build()).await
}
