//! 管理用リスナー（公開用のポートとは別の `TcpListener` で立てる。既定 `admin.bind` =
//! `127.0.0.1:18081`）。トークンは `secrets/`（または環境変数）から読む。公開用のポートには、
//! これらのエンドポイントを置かない（ロードバランサーには公開用ポートだけを登録する前提。原則17）。
//!
//! `scripts/election.sh` が、この API を curl で呼ぶ。

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Json, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use domain::{ElectionPhase, Period, ShardId};
use serde::{Deserialize, Serialize};
use shared_types::hex;

use crate::state::AppState;

/// `Authorization: Bearer <admin.token>` の確認。成功したら、監査ログの `actor` に使う短い印
/// （トークンの下 8 桁）を持つ。
pub struct AdminAuth {
    pub actor: String,
}

impl FromRequestParts<Arc<AppState>> for AdminAuth {
    type Rejection = (StatusCode, Json<AdminError>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let Some(expected) = &state.admin_token else {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "admin.token が設定されていません",
            ));
        };
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match token {
            Some(t) if t == expected.expose().as_str() => {
                let tail: String = t
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                Ok(Self {
                    actor: format!("admin:{tail}"),
                })
            }
            _ => Err(error(StatusCode::UNAUTHORIZED, "トークンが不正です")),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    error: String,
}

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<AdminError>) {
    (
        status,
        Json(AdminError {
            error: message.to_string(),
        }),
    )
}

/// 管理用リスナーの `Router`。公開用の `routes::app` とは別に組み立て、別の `TcpListener` で serve する。
pub fn admin_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/admin/v1/election", get(get_election))
        .route("/admin/v1/election/schedule", post(schedule))
        .route("/admin/v1/election/open", post(open_now))
        .route("/admin/v1/election/close", post(close_now))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HeadSummary {
    shard: u16,
    height: Option<u64>,
    block_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuditEntryDto {
    at_unix_secs: i64,
    from: String,
    to: String,
    actor: String,
    /// `transition`（状態の遷移）/ `revote_key_destroyed`（締切の手続きの中で、再投票の鍵を破棄した。ADR 0022）。
    event: String,
}

#[derive(Debug, Serialize)]
struct ElectionStatus {
    phase: String,
    opens_at: Option<i64>,
    closes_at: Option<i64>,
    closing_started_at: Option<i64>,
    now: i64,
    /// シャードごとの未封印の票の件数（添字がシャード番号。票の中身は含まない）。
    pending_by_shard: Vec<usize>,
    heads: Vec<HeadSummary>,
    /// 変更の新しい順、最大 5 件。
    recent_audit: Vec<AuditEntryDto>,
}

async fn election_status(
    state: &Arc<AppState>,
) -> Result<ElectionStatus, (StatusCode, Json<AdminError>)> {
    let unavailable = |what: &str| error(StatusCode::SERVICE_UNAVAILABLE, what);
    let snapshot = state
        .election_state
        .get()
        .await
        .map_err(|_| unavailable("選挙状態を取得できません"))?;
    let pending_by_shard = state
        .voting
        .pending_by_shard()
        .await
        .map_err(|_| unavailable("未封印の件数を取得できません"))?;
    let mut heads = Vec::with_capacity(usize::from(state.shard_count.get()));
    for shard in 0..state.shard_count.get() {
        let head = state
            .chains
            .head(ShardId(shard))
            .await
            .map_err(|_| unavailable("チェーンの先頭を取得できません"))?;
        heads.push(HeadSummary {
            shard,
            height: head.as_ref().map(|b| b.header.height),
            block_hash: head.as_ref().map(|b| hex::encode(&b.block_hash)),
        });
    }
    let recent_audit = state
        .election_state
        .recent_audit(5)
        .await
        .map_err(|_| unavailable("監査ログを取得できません"))?
        .into_iter()
        .map(|e| AuditEntryDto {
            at_unix_secs: e.at_unix_secs,
            from: e.from.as_str().to_string(),
            to: e.to.as_str().to_string(),
            actor: e.actor,
            event: e.event.as_str().to_string(),
        })
        .collect();
    Ok(ElectionStatus {
        phase: snapshot.phase.as_str().to_string(),
        opens_at: snapshot.period.opens_at,
        closes_at: snapshot.period.closes_at,
        closing_started_at: snapshot.closing_started_at,
        now: i64::try_from(state.clock.now_unix_secs()).unwrap_or(i64::MAX),
        pending_by_shard,
        heads,
        recent_audit,
    })
}

/// `GET /admin/v1/election`: 状態・期間・シャードごとの未封印件数・最後のブロック・監査ログの直近 5 件。
async fn get_election(
    _: AdminAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<AdminError>)> {
    Ok(Json(election_status(&state).await?))
}

#[derive(Debug, Deserialize)]
struct ScheduleRequest {
    /// RFC 3339（秒まで、タイムゾーンのオフセット必須）。
    opens_at: String,
    closes_at: String,
}

/// `POST /admin/v1/election/schedule`: `scheduled` の間だけ、期間を設定する。
async fn schedule(
    _: AdminAuth,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ScheduleRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<AdminError>)> {
    let bad = |msg: String| error(StatusCode::BAD_REQUEST, &msg);
    let opens_at =
        app_config::parse_rfc3339(&req.opens_at).map_err(|e| bad(format!("opens_at: {e}")))?;
    let closes_at =
        app_config::parse_rfc3339(&req.closes_at).map_err(|e| bad(format!("closes_at: {e}")))?;
    if opens_at >= closes_at {
        return Err(bad("opens_at は closes_at より前にしてください".to_string()));
    }
    let now = i64::try_from(state.clock.now_unix_secs()).unwrap_or(i64::MAX);
    let ok = state
        .election_state
        .schedule(
            Period {
                opens_at: Some(opens_at),
                closes_at: Some(closes_at),
            },
            now,
        )
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "選挙状態を更新できません"))?;
    if !ok {
        return Err(error(
            StatusCode::CONFLICT,
            "schedule は scheduled の状態でだけ実行できます",
        ));
    }
    Ok(Json(election_status(&state).await?))
}

/// `POST /admin/v1/election/open`: `scheduled` → `open`（期間の判定より前に手動で開ける）。
async fn open_now(
    admin: AdminAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<AdminError>)> {
    transition_now(
        &state,
        ElectionPhase::Scheduled,
        ElectionPhase::Open,
        &admin.actor,
    )
    .await?;
    Ok(Json(election_status(&state).await?))
}

/// `POST /admin/v1/election/close`: `open` → `closing`。その後の待ち時間・全シャードのフラッシュ・
/// 未封印 0 件の確認・最終アンカー・`closed` への遷移は、アンカーのリースを持つ sealer が自動で行う
/// （手動操作は、この 1 段の遷移だけ）。
async fn close_now(
    admin: AdminAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<AdminError>)> {
    transition_now(
        &state,
        ElectionPhase::Open,
        ElectionPhase::Closing,
        &admin.actor,
    )
    .await?;
    Ok(Json(election_status(&state).await?))
}

async fn transition_now(
    state: &Arc<AppState>,
    from: ElectionPhase,
    to: ElectionPhase,
    actor: &str,
) -> Result<(), (StatusCode, Json<AdminError>)> {
    let now = i64::try_from(state.clock.now_unix_secs()).unwrap_or(i64::MAX);
    let ok = state
        .election_state
        // open --now のときは、この api の設定の選挙のルールを固定する（原則19）。
        .transition(from, to, state.configured_rules, actor, now)
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "選挙状態を更新できません"))?;
    if ok {
        Ok(())
    } else {
        Err(error(
            StatusCode::CONFLICT,
            &format!("{} の状態でだけ実行できます", from.as_str()),
        ))
    }
}
