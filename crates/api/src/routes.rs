//! ルーティングとハンドラ。

use std::sync::Arc;

use application::Credentials;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use domain::{CandidateId, ContestId, ShardId, VoteGate, vote_gate};
use serde::Deserialize;
use serde_json::{Value, json};
use shared_types::hex;
use shared_types::{
    AnchorDto, AnchorsResponse, AuditCountsResponse, BallotStatusDto, BallotStatusResponse,
    BlocksPageResponse, CandidateDto, CandidatesResponse, ChainsResponse, ContestCountsDto,
    ElectionStatusResponse, HeadDto, HeadRefDto, LoginRequest, LoginResponse, ShardSummaryDto,
    VoteRequest, VoteResponse,
};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth::AuthedVoter;
use crate::chain_view::{self, REVALIDATE};
use crate::error::{ApiError, add_error_messages};
use crate::state::AppState;

pub fn app(state: Arc<AppState>) -> Router {
    let request_timeout = state.request_timeout;
    let api = Router::new()
        .route("/login", post(login))
        .route("/election-status", get(election_status))
        .route("/ballot-status", get(ballot_status))
        .route(
            "/contests/{election_id}/{district_id}/candidates",
            get(candidates),
        )
        .route("/contests/{election_id}/{district_id}/vote", post(vote))
        // 封印済みチェーンは誰でも検証できる公開データなので、認証は付けない。
        .route("/chains", get(chains_index))
        .route("/chains/{shard}/head", get(chain_head))
        .route("/chains/{shard}/blocks", get(chain_blocks))
        .route("/chains/{shard}/blocks/{height}", get(chain_block))
        .route("/anchors", get(anchors_list))
        .route("/anchors/latest", get(latest_anchor))
        .route("/audit/counts", get(audit_counts));

    let router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/v1", api);
    // 開発用のデバッグ機能は feature "dev-tools" の中に閉じ込める（原則10）。
    #[cfg(feature = "dev-tools")]
    let router = router
        .route("/debug/pool", get(crate::debug::pool))
        .route("/debug/tamper", post(crate::debug::tamper));

    router
        .layer(middleware::map_response_with_state(
            state.clone(),
            add_error_messages,
        ))
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
        .layer(TraceLayer::new_for_http())
}

/// `GET /api/v1/election-status`（認証不要）: ログイン画面・進捗画面が、期間と今の状態を表示する
/// （原則17・18）。
async fn election_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ElectionStatusResponse>, ApiError> {
    let now = state.clock.now_unix_secs();
    let snapshot = state.election_gate.snapshot(now).await?;
    Ok(Json(ElectionStatusResponse {
        phase: snapshot.phase.as_str().to_string(),
        opens_at: snapshot.period.opens_at,
        closes_at: snapshot.period.closes_at,
        now: i64::try_from(now).unwrap_or(i64::MAX),
        display_timezone: state.display_timezone.name.to_string(),
        display_timezone_offset_secs: state.display_timezone.offset_secs,
    }))
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    // マイナンバー欄は Authenticator に渡すだけで、ここでは保存もログ出力もしない。
    let voter = state
        .auth
        .authenticate(Credentials {
            voter_id: req.login_id,
            password: req.password,
            my_number: req.my_number,
        })
        .await?;
    let issued = state.sessions.issue(&voter, state.clock.now_unix_secs());
    Ok(Json(LoginResponse {
        token: issued.token,
        expires_in_secs: issued.expires_in_secs,
    }))
}

/// パスの `{election_id}/{district_id}` から、投票用紙の ID を作る。形式が不正なら、存在しないものとして 404。
fn contest_from_path(election_id: &str, district_id: &str) -> Result<ContestId, ApiError> {
    ContestId::parse(&format!("{election_id}/{district_id}")).map_err(|_| ApiError::NotFound)
}

/// `GET /api/v1/ballot-status`: ログイン中の有権者に関係する投票用紙だけを、表示順で返す。
/// 投票する順番は、この並びで固定（画面は、先頭の未投票のものへ自動で進む）。
async fn ballot_status(
    State(state): State<Arc<AppState>>,
    AuthedVoter(voter): AuthedVoter,
) -> Result<Json<BallotStatusResponse>, ApiError> {
    let ballots = state
        .voting
        .ballot_status(&voter)
        .await?
        .into_iter()
        .map(|b| BallotStatusDto {
            contest_id: b.contest_id.to_string(),
            name: b.name,
            election_type: b.election_type.to_string(),
            type_name: b.type_name,
            method: match b.method {
                domain::VotingMethod::SingleChoice => shared_types::VotingMethod::SingleChoice,
            },
            voted: b.voted,
        })
        .collect();
    Ok(Json(BallotStatusResponse { ballots }))
}

async fn candidates(
    State(state): State<Arc<AppState>>,
    AuthedVoter(voter): AuthedVoter,
    Path((election_id, district_id)): Path<(String, String)>,
) -> Result<Json<CandidatesResponse>, ApiError> {
    let contest = contest_from_path(&election_id, &district_id)?;
    let candidates = state
        .voting
        .candidates(&voter, &contest)
        .await?
        .into_iter()
        .map(|c| CandidateDto {
            candidate_id: c.id.to_string(),
            name: c.name,
            party: c.party,
        })
        .collect();
    Ok(Json(CandidatesResponse { candidates }))
}

async fn vote(
    State(state): State<Arc<AppState>>,
    AuthedVoter(voter): AuthedVoter,
    Path((election_id, district_id)): Path<(String, String)>,
    Json(req): Json<VoteRequest>,
) -> Result<(StatusCode, Json<VoteResponse>), ApiError> {
    // 投票を受け付けてよいのは、状態が open で、かつ 開始時刻 <= 現在時刻 < 終了時刻 のときだけ（原則18）。
    let now = state.clock.now_unix_secs();
    let snapshot = state.election_gate.snapshot(now).await?;
    #[allow(clippy::cast_possible_wrap)]
    let now_i64 = now as i64;
    match vote_gate(snapshot.phase, snapshot.period, now_i64) {
        VoteGate::Accept => {}
        VoteGate::NotStarted => return Err(ApiError::VotingNotStarted),
        VoteGate::Closing => return Err(ApiError::VotingClosing),
        VoteGate::Ended => return Err(ApiError::VotingClosed),
    }

    let contest = contest_from_path(&election_id, &district_id)?;
    // 候補者 ID の形式が不正なら、その投票用紙に存在しない候補者として扱う。
    let candidate =
        CandidateId::parse(&req.candidate_id).map_err(|_| ApiError::InvalidCandidate)?;
    state.voting.cast_vote(&voter, contest, candidate).await?;
    // 秘密投票: ここでは投票者も候補者もログに出さない。
    tracing::debug!("投票を受理しました");
    Ok((
        StatusCode::CREATED,
        Json(VoteResponse {
            status: "accepted".to_string(),
        }),
    ))
}

/// `GET /api/v1/chains/{shard}/head`
async fn chain_head(
    State(state): State<Arc<AppState>>,
    Path(shard): Path<u16>,
) -> Result<Json<HeadDto>, ApiError> {
    let head = state
        .chains
        .head(ShardId(shard))
        .await?
        .ok_or(ApiError::NotFound)?;
    // 公開鍵は sealer が DB に登録したもの（api は署名鍵を持たない）。チェーンがあれば登録済みのはず。
    let signer_public_key = state
        .chains
        .signer_public_key()
        .await?
        .ok_or(ApiError::Unavailable)?;
    Ok(Json(HeadDto {
        shard,
        height: head.header.height,
        block_hash: hex::encode(&head.block_hash),
        ballot_count: head.header.ballot_count,
        sealed_at_minute: head.header.sealed_at_minute,
        signer_public_key: hex::encode(&signer_public_key),
    }))
}

/// `Cache-Control` を付ける。
fn cached<T: IntoResponse>(cache_control: &'static str, body: T) -> impl IntoResponse {
    (
        [(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        )],
        body,
    )
}

/// シャード番号が、設定のシャード数の範囲内か。範囲外は、存在しないシャード（404）。
fn checked_shard(state: &AppState, shard: u16) -> Result<ShardId, ApiError> {
    if shard < state.shard_count.get() {
        Ok(ShardId(shard))
    } else {
        Err(ApiError::NotFound)
    }
}

/// `GET /api/v1/chains`: シャードの一覧と、それぞれの先頭ブロック。先頭は伸びるので、毎回確認させる。
async fn chains_index(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let mut shards = Vec::with_capacity(usize::from(state.shard_count.get()));
    for shard in 0..state.shard_count.get() {
        let head = state.chains.head(ShardId(shard)).await?;
        shards.push(ShardSummaryDto {
            shard,
            head: head.as_ref().map(chain_view::block_summary),
        });
    }
    let signer_public_key = state
        .chains
        .signer_public_key()
        .await?
        .map(|key| hex::encode(&key));
    Ok(cached(
        REVALIDATE,
        Json(ChainsResponse {
            shards,
            signer_public_key,
        }),
    ))
}

/// `GET /api/v1/chains/{shard}/blocks` のクエリ。空文字（`?before_height=&limit=`）は、指定なしとして読む。
#[derive(Debug, Deserialize)]
struct PageQuery {
    #[serde(default, deserialize_with = "chain_view::empty_as_none")]
    before_height: Option<u64>,
    #[serde(default, deserialize_with = "chain_view::empty_as_none")]
    limit: Option<usize>,
}

/// `GET /api/v1/chains/{shard}/blocks?before_height=&limit=`: ブロックの要約を、新しい順に返す（ページ送り）。
///
/// `before_height` より低い高さのブロックを、最大 `limit` 件（既定 20、1〜100 にそろえる）。次のページは、
/// 応答の `next_before_height` を `before_height` に渡す。票は含まない（詳細は `blocks/{height}`）。
async fn chain_blocks(
    State(state): State<Arc<AppState>>,
    Path(shard): Path<u16>,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let shard_id = checked_shard(&state, shard)?;
    let limit = chain_view::clamp_limit(query.limit);
    // 先頭の高さは、キャッシュの判定にだけ使う（応答の本文には入れない: 入れると、同じ URL の応答が変わってしまう）。
    let head = state.chains.head(shard_id).await?;
    let blocks: Vec<_> = state
        .chains
        .blocks_before(shard_id, query.before_height, limit)
        .await?
        .iter()
        .map(chain_view::block_summary)
        .collect();
    let cache =
        chain_view::blocks_page_cache(query.before_height, head.as_ref().map(|h| h.header.height));
    let next_before_height = chain_view::next_before_height(&blocks);
    Ok(cached(
        cache,
        Json(BlocksPageResponse {
            shard,
            blocks,
            next_before_height,
        }),
    ))
}

/// `GET /api/v1/chains/{shard}/blocks/{height}`: ブロックの詳細。
///
/// 票の中身は、`chain.reveal_ballots` に従う（`after_close` の締切前は返さない）。封印済みのブロックは内容が変わらないので、
/// 票を返す応答は `immutable`。伏せた応答は、締切後に変わるので `no-store`。
async fn chain_block(
    State(state): State<Arc<AppState>>,
    Path((shard, height)): Path<(u16, u64)>,
) -> Result<impl IntoResponse, ApiError> {
    let shard_id = checked_shard(&state, shard)?;
    let block = state
        .chains
        .block(shard_id, height)
        .await?
        .ok_or(ApiError::NotFound)?;
    let signer = state.chains.signer_public_key().await?;
    let revealed = state.reveal.is_revealed(state.clock.now_unix_secs());
    let detail = chain_view::block_detail(&block, revealed, &state.election, signer.as_ref());
    Ok(cached(
        chain_view::block_detail_cache(revealed),
        Json(detail),
    ))
}

/// `GET /api/v1/anchors?limit=`: アンカーを、新しい順に返す（既定 20 件、1〜100）。新しいアンカーが増えるので、毎回確認させる。
async fn anchors_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = chain_view::clamp_limit(query.limit);
    let anchors = state
        .chains
        .latest_anchors(limit)
        .await?
        .iter()
        .map(anchor_dto)
        .collect();
    Ok(cached(REVALIDATE, Json(AnchorsResponse { anchors })))
}

/// `GET /api/v1/anchors/latest`: 全シャードの head をまとめて署名した、最新のアンカー（未作成なら 404）。
async fn latest_anchor(State(state): State<Arc<AppState>>) -> Result<Json<AnchorDto>, ApiError> {
    let anchor = state
        .chains
        .latest_anchor()
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(anchor_dto(&anchor)))
}

fn anchor_dto(anchor: &domain::Anchor) -> AnchorDto {
    AnchorDto {
        seq: anchor.seq,
        anchor_minute: anchor.anchor_minute,
        prev_anchor_hash: hex::encode(&anchor.prev_anchor_hash),
        heads: anchor
            .heads
            .iter()
            .map(|h| HeadRefDto {
                shard: h.shard,
                height: h.height,
                block_hash: hex::encode(&h.block_hash),
            })
            .collect(),
        anchor_hash: hex::encode(&anchor.anchor_hash),
        signature: hex::encode(&anchor.signature),
    }
}

/// `GET /api/v1/audit/counts`: 投票用紙ごとの participation 件数と未封印の票の件数（監査用の集計値のみ。
/// 投票者や票の中身は含まない）。全体を走査するので、頻繁に呼ぶものではない。
async fn audit_counts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AuditCountsResponse>, ApiError> {
    let contests = state
        .voting
        .audit_counts()
        .await?
        .into_iter()
        .map(|c| ContestCountsDto {
            contest_id: c.contest.to_string(),
            participation: c.participation,
            pending: c.pending,
        })
        .collect();
    Ok(Json(AuditCountsResponse { contests }))
}
