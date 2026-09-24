//! 開発用のデバッグ機能（feature "dev-tools" のときだけコンパイルされる）。
//!
//! 返すのはシャードごとの未封印件数だけで、票の中身や投票者の情報は一切返さない。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use domain::ShardId;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Serialize)]
pub struct ShardPending {
    shard: usize,
    pending: usize,
}

#[derive(Serialize)]
pub struct PoolCounts {
    shards: Vec<ShardPending>,
    total: usize,
}

/// `GET /debug/pool`
pub async fn pool(State(state): State<Arc<AppState>>) -> Result<Json<PoolCounts>, ApiError> {
    let counts = state.voting.pending_by_shard().await?;
    let total = counts.iter().sum();
    let shards = counts
        .into_iter()
        .enumerate()
        .map(|(shard, pending)| ShardPending { shard, pending })
        .collect();
    Ok(Json(PoolCounts { shards, total }))
}

/// `POST /debug/tamper` のリクエスト（すべて任意）。
#[derive(Debug, Default, Deserialize)]
pub struct TamperRequest {
    shard: Option<u16>,
    height: Option<u64>,
    index: Option<usize>,
}

#[derive(Serialize)]
pub struct TamperedAt {
    shard: u16,
    height: u64,
    index: usize,
}

#[derive(Serialize)]
pub struct TamperResponse {
    tampered: TamperedAt,
}

/// `POST /debug/tamper`: 改ざんデモ用に、メモリ上の封印済みの票を 1 件書き換える。
///
/// 省略時の対象は「シャード 0 の、票を持つ最新ブロックの 0 番目」。
/// 返すのは書き換えた場所だけで、票の中身は返さない。
pub async fn tamper(
    State(state): State<Arc<AppState>>,
    body: Option<Json<TamperRequest>>,
) -> Result<Json<TamperResponse>, ApiError> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    // 改ざんデモはメモリ上のストアだけが対象（scylla では未対応）。
    let store = state.dev_store.as_ref().ok_or(ApiError::NotSupported)?;
    let at = store
        .tamper_ballot(req.shard.map(ShardId), req.height, req.index)
        .map_err(|_| ApiError::NotFound)?;
    tracing::warn!(
        shard = at.shard.0,
        height = at.height,
        index = at.index,
        "票を改ざんしました（dev-tools）"
    );
    Ok(Json(TamperResponse {
        tampered: TamperedAt {
            shard: at.shard.0,
            height: at.height,
            index: at.index,
        },
    }))
}
