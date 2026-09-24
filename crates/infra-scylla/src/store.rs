//! ScyllaDB 版のストア本体。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use application::{
    CastError, ChainRead, Clock, ContestCounts, CredentialAdmin, CredentialRecord, CredentialStore,
    ElectionAuditEntry, ElectionStateSnapshot, ElectionStateStore, LeaseStore, RegistryEntry,
    SealStore, StoreError, VoteStore, VoterRoll,
};
use async_trait::async_trait;
use domain::{
    Anchor, Ballot, Block, ContestId, DistrictId, ElectionPhase, Period, ShardId, VoterId,
};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::ExecutionError;
use scylla::response::query_result::{QueryResult, QueryRowsResult};
use scylla::statement::Consistency;
use scylla::statement::batch::{Batch, BatchType};
use scylla::statement::prepared::PreparedStatement;
use scylla::value::{CqlValue, Row};

use crate::convert::{
    AnchorRow, BallotTuple, BlockRow, anchor_from_row, anchor_to_values, ballot_from_tuple,
    block_from_row, block_to_values, minute_bucket, pool_delete_timestamp,
};

/// DB 操作を再試行する最大回数（タイムアウトなど一時的な失敗のため）。
const MAX_ATTEMPTS: u32 = 5;
/// プールから削除する票を 1 バッチにまとめる最大件数（バッチのサイズ警告を避ける）。
const DELETE_BATCH: usize = 50;

/// 接続設定。
#[derive(Debug, Clone)]
pub struct ScyllaConfig {
    /// 接続先ノード（`host:port`）。
    pub nodes: Vec<String>,
    /// スキーマ投入済みのキースペース。
    pub keyspace: String,
    /// シャード数。api / sealer と同じ値にする。
    pub shard_count: NonZeroU16,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(
        "ScyllaDB に接続できません（キースペースが無い場合は docs/schema.cql を投入してください）: {0}"
    )]
    Connect(String),
    #[error("スキーマが想定と異なります（docs/schema.cql を投入してください）: {0}")]
    Schema(String),
    /// クラスタ共通の設定（シャード数）が、既に登録された値と食い違っている。
    #[error("クラスタ設定が一致しません: {0}")]
    ClusterConfig(String),
}

/// 一度だけ prepare して使い回す文。
struct Statements {
    insert_participation: PreparedStatement,
    select_attempt: PreparedStatement,
    delete_participation: PreparedStatement,
    select_voted: PreparedStatement,
    insert_pool: PreparedStatement,
    select_pool_head: PreparedStatement,
    select_pool_keys: PreparedStatement,
    count_pool: PreparedStatement,
    delete_pool: PreparedStatement,
    select_head: PreparedStatement,
    select_block: PreparedStatement,
    insert_block: PreparedStatement,
    select_block_hash: PreparedStatement,
    acquire_lease: PreparedStatement,
    renew_lease: PreparedStatement,
    release_lease: PreparedStatement,
    select_lease_owner: PreparedStatement,
    insert_anchor: PreparedStatement,
    select_latest_anchor: PreparedStatement,
    select_anchors: PreparedStatement,
    select_blocks_top: PreparedStatement,
    select_blocks_before: PreparedStatement,
    insert_signer: PreparedStatement,
    select_signer: PreparedStatement,
    insert_config: PreparedStatement,
    select_config: PreparedStatement,
    select_participation_contests: PreparedStatement,
    select_pool_contests: PreparedStatement,
    select_credential: PreparedStatement,
    insert_credential: PreparedStatement,
    delete_credential: PreparedStatement,
    select_registry: PreparedStatement,
    upsert_registry: PreparedStatement,
    put_roll: PreparedStatement,
    select_roll: PreparedStatement,
    insert_election_state: PreparedStatement,
    select_election_state: PreparedStatement,
    update_election_period: PreparedStatement,
    update_election_phase: PreparedStatement,
    update_election_phase_open: PreparedStatement,
    update_election_phase_closing: PreparedStatement,
    insert_election_audit: PreparedStatement,
    select_election_audit: PreparedStatement,
}

/// アンカーの `scope`（現在は 1 本のみ）。
const ANCHOR_SCOPE: &str = "chain";
/// 選挙状態の `scope`（現在は 1 選挙のみ。原則17）。
const ELECTION_SCOPE: &str = "election";
/// 署名鍵の `key_id`（現在は 1 つのみ）。
const SIGNER_KEY_ID: &str = "current";
const SHARD_COUNT_KEY: &str = "shard_count";

const BLOCK_COLUMNS: &str = "height, format_version, prev_hash, merkle_root, ballot_count, \
                             sealed_at_minute, block_hash, signature, ballots";

pub struct ScyllaStore {
    session: Session,
    stmts: Statements,
    shard_count: NonZeroU16,
    /// 票の受理時刻（分）と書き込み時刻の丸めに使う。
    clock: Arc<dyn Clock>,
}

// 内部の状態（接続・票）を誤ってログに出さないよう、Debug は最小限にする。
impl fmt::Debug for ScyllaStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScyllaStore")
            .field("shard_count", &self.shard_count)
            .finish_non_exhaustive()
    }
}

/// 一時的な失敗を再試行する。再試行する操作は、繰り返しても安全（冪等）でなければならない。
/// エラーの内容は記録するが、値（投票者・候補者）は含まれない。
async fn with_retry<T, Fut>(
    operation: &'static str,
    mut op: impl FnMut() -> Fut,
) -> Result<T, StoreError>
where
    Fut: Future<Output = Result<T, ExecutionError>>,
{
    let mut delay_ms = 20u64;
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt < MAX_ATTEMPTS => {
                tracing::warn!(operation, attempt, error = %e, "DB 操作に失敗したので再試行します");
                // 同時に失敗した多数のリクエストが同じタイミングで再試行しないよう、揺らぎを付ける。
                let jitter = rand::random::<u64>() % delay_ms;
                tokio::time::sleep(Duration::from_millis(delay_ms + jitter)).await;
                delay_ms *= 2;
            }
            Err(e) => {
                tracing::error!(operation, attempt, error = %e, "DB 操作に失敗しました");
                return Err(StoreError::Unavailable);
            }
        }
    }
    Err(StoreError::Unavailable)
}

fn decode_error(what: &'static str, e: impl fmt::Display) -> StoreError {
    tracing::error!(what, error = %e, "DB の応答を解釈できません");
    StoreError::Corrupt
}

fn rows(result: QueryResult) -> Result<QueryRowsResult, StoreError> {
    result
        .into_rows_result()
        .map_err(|e| decode_error("結果が行ではありません", e))
}

/// LWT（`IF ...`）の結果から `[applied]` を読む。
/// 適用されたときは `[applied]` だけ、されなかったときは既存の行の列も付いてくるので、
/// 列数に依存しない `Row` で先頭の列だけを見る。
fn applied(result: QueryResult) -> Result<bool, StoreError> {
    let row: Row = rows(result)?
        .first_row()
        .map_err(|e| decode_error("LWT の結果", e))?;
    match row.columns.first() {
        Some(Some(CqlValue::Boolean(applied))) => Ok(*applied),
        _ => Err(StoreError::Corrupt),
    }
}

impl ScyllaStore {
    /// 接続して、使う文を prepare する。キースペースやテーブルが無ければエラー。
    pub async fn connect(
        config: &ScyllaConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConnectError> {
        // キースペース名は CQL の識別子として埋め込むので、接続する前に検査する。
        crate::keyspace::validate_keyspace(&config.keyspace).map_err(ConnectError::Connect)?;
        // `USE <keyspace>` は使わない（prepared statement との併用は Cassandra が警告するアンチパターン）。
        let session = SessionBuilder::new()
            .known_nodes(&config.nodes)
            .build()
            .await
            .map_err(|e| ConnectError::Connect(e.to_string()))?;
        ensure_keyspace_exists(&session, &config.keyspace).await?;
        ensure_schema_current(&session, &config.keyspace).await?;

        let quorum = Consistency::LocalQuorum;
        // LWT で書いた行を確実に読むため、participation / blocks の照会は SERIAL で行う。
        let serial = Consistency::Serial;
        // すべての CQL は完全修飾名（`{ks}.テーブル`）で書き、キースペース名は設定値から埋め込む。
        let ks = config.keyspace.as_str();
        let p = |cql: String, consistency| prepare(&session, cql.replace("{ks}", ks), consistency);
        let stmts = Statements {
            insert_participation: p(
                "INSERT INTO {ks}.participation (voter_id, contest_id, attempt) VALUES (?, ?, ?) IF NOT EXISTS".into(),
                quorum,
            )
            .await?,
            select_attempt: p(
                "SELECT attempt FROM {ks}.participation WHERE voter_id = ? AND contest_id = ?".into(),
                serial,
            )
            .await?,
            delete_participation: p(
                "DELETE FROM {ks}.participation WHERE voter_id = ? AND contest_id = ? IF attempt = ?".into(),
                quorum,
            )
            .await?,
            select_voted: p(
                "SELECT contest_id FROM {ks}.participation WHERE voter_id = ?".into(),
                serial,
            )
            .await?,
            // 書き込み時刻を分に丸めて指定する（participation の LWT とマイクロ秒で突き合わせられないように）。
            insert_pool: p(
                "INSERT INTO {ks}.ballot_pool (shard, received_minute, ballot_id, contest_id, candidate_id) \
                 VALUES (?, ?, ?, ?, ?) USING TIMESTAMP ?"
                    .into(),
                quorum,
            )
            .await?,
            select_pool_head: p(
                "SELECT received_minute, ballot_id, contest_id, candidate_id FROM {ks}.ballot_pool \
                 WHERE shard = ? LIMIT ?"
                    .into(),
                quorum,
            )
            .await?,
            select_pool_keys: p(
                "SELECT received_minute, ballot_id FROM {ks}.ballot_pool WHERE shard = ?".into(),
                quorum,
            )
            .await?,
            count_pool: p(
                "SELECT COUNT(*) FROM {ks}.ballot_pool WHERE shard = ?".into(),
                quorum,
            )
            .await?,
            delete_pool: p(
                "DELETE FROM {ks}.ballot_pool USING TIMESTAMP ? WHERE shard = ? AND received_minute = ? AND ballot_id = ?".into(),
                quorum,
            )
            .await?,
            select_head: p(
                format!("SELECT {BLOCK_COLUMNS} FROM {{ks}}.blocks WHERE shard = ? LIMIT 1"),
                quorum,
            )
            .await?,
            select_block: p(
                format!("SELECT {BLOCK_COLUMNS} FROM {{ks}}.blocks WHERE shard = ? AND height = ?"),
                quorum,
            )
            .await?,
            insert_block: p(
                "INSERT INTO {ks}.blocks (shard, height, format_version, prev_hash, merkle_root, \
                 ballot_count, sealed_at_minute, block_hash, signature, ballots) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) IF NOT EXISTS"
                    .into(),
                quorum,
            )
            .await?,
            select_block_hash: p(
                "SELECT block_hash FROM {ks}.blocks WHERE shard = ? AND height = ?".into(),
                serial,
            )
            .await?,
            // リース: 期限（TTL）は取得・更新のたびに付け直す。
            acquire_lease: p(
                "INSERT INTO {ks}.sealer_lease (lease_name, owner) VALUES (?, ?) IF NOT EXISTS USING TTL ?".into(),
                quorum,
            )
            .await?,
            renew_lease: p(
                "UPDATE {ks}.sealer_lease USING TTL ? SET owner = ? WHERE lease_name = ? IF owner = ?".into(),
                quorum,
            )
            .await?,
            release_lease: p(
                "DELETE FROM {ks}.sealer_lease WHERE lease_name = ? IF owner = ?".into(),
                quorum,
            )
            .await?,
            select_lease_owner: p(
                "SELECT owner FROM {ks}.sealer_lease WHERE lease_name = ?".into(),
                serial,
            )
            .await?,
            insert_anchor: p(
                "INSERT INTO {ks}.anchors (scope, seq, anchor_minute, prev_anchor_hash, heads, anchor_hash, signature) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) IF NOT EXISTS"
                    .into(),
                quorum,
            )
            .await?,
            select_latest_anchor: p(
                "SELECT seq, anchor_minute, prev_anchor_hash, heads, anchor_hash, signature \
                 FROM {ks}.anchors WHERE scope = ? LIMIT 1"
                    .into(),
                quorum,
            )
            .await?,
            select_anchors: p(
                "SELECT seq, anchor_minute, prev_anchor_hash, heads, anchor_hash, signature \
                 FROM {ks}.anchors WHERE scope = ? LIMIT ?"
                    .into(),
                quorum,
            )
            .await?,
            select_blocks_top: p(
                format!("SELECT {BLOCK_COLUMNS} FROM {{ks}}.blocks WHERE shard = ? LIMIT ?"),
                quorum,
            )
            .await?,
            select_blocks_before: p(
                format!(
                    "SELECT {BLOCK_COLUMNS} FROM {{ks}}.blocks WHERE shard = ? AND height < ? LIMIT ?"
                ),
                quorum,
            )
            .await?,
            insert_signer: p(
                "INSERT INTO {ks}.signer_keys (key_id, public_key) VALUES (?, ?) IF NOT EXISTS".into(),
                quorum,
            )
            .await?,
            select_signer: p(
                "SELECT public_key FROM {ks}.signer_keys WHERE key_id = ?".into(),
                serial,
            )
            .await?,
            insert_config: p(
                "INSERT INTO {ks}.cluster_config (key, value) VALUES (?, ?) IF NOT EXISTS".into(),
                quorum,
            )
            .await?,
            select_config: p(
                "SELECT value FROM {ks}.cluster_config WHERE key = ?".into(),
                serial,
            )
            .await?,
            // 監査用: participation / プールを全体（プールはシャードごと）走査して、投票用紙別に数える。
            select_participation_contests: p(
                "SELECT contest_id FROM {ks}.participation".into(),
                quorum,
            )
            .await?,
            select_pool_contests: p(
                "SELECT contest_id FROM {ks}.ballot_pool WHERE shard = ?".into(),
                quorum,
            )
            .await?,
            // 認証情報（auth.mode=db）。ログイン ID の一意性は LWT で守る。
            select_credential: p(
                "SELECT password_hash, voter_id FROM {ks}.credentials WHERE login_id = ?".into(),
                quorum,
            )
            .await?,
            insert_credential: p(
                "INSERT INTO {ks}.credentials (login_id, password_hash, voter_id) VALUES (?, ?, ?) IF NOT EXISTS".into(),
                quorum,
            )
            .await?,
            delete_credential: p(
                "DELETE FROM {ks}.credentials WHERE login_id = ?".into(),
                quorum,
            )
            .await?,
            select_registry: p(
                "SELECT voter_id, login_id FROM {ks}.voter_registry WHERE external_id = ?".into(),
                quorum,
            )
            .await?,
            upsert_registry: p(
                "INSERT INTO {ks}.voter_registry (external_id, voter_id, login_id) VALUES (?, ?, ?)".into(),
                quorum,
            )
            .await?,
            put_roll: p(
                "INSERT INTO {ks}.voter_roll (voter_id, districts) VALUES (?, ?)".into(),
                quorum,
            )
            .await?,
            select_roll: p(
                "SELECT districts FROM {ks}.voter_roll WHERE voter_id = ?".into(),
                quorum,
            )
            .await?,
            insert_election_state: p(
                "INSERT INTO {ks}.election_state (scope, phase, opens_at, closes_at, opened_at, closing_started_at) \
                 VALUES (?, 'scheduled', ?, ?, null, null) IF NOT EXISTS"
                    .into(),
                quorum,
            )
            .await?,
            select_election_state: p(
                "SELECT phase, opens_at, closes_at, opened_at, closing_started_at FROM {ks}.election_state WHERE scope = ?"
                    .into(),
                serial,
            )
            .await?,
            update_election_period: p(
                "UPDATE {ks}.election_state SET opens_at = ?, closes_at = ? WHERE scope = ? IF phase = 'scheduled'"
                    .into(),
                quorum,
            )
            .await?,
            update_election_phase: p(
                "UPDATE {ks}.election_state SET phase = ? WHERE scope = ? IF phase = ?".into(),
                quorum,
            )
            .await?,
            update_election_phase_open: p(
                "UPDATE {ks}.election_state SET phase = ?, opened_at = ? WHERE scope = ? IF phase = ?"
                    .into(),
                quorum,
            )
            .await?,
            update_election_phase_closing: p(
                "UPDATE {ks}.election_state SET phase = ?, closing_started_at = ? WHERE scope = ? IF phase = ?"
                    .into(),
                quorum,
            )
            .await?,
            insert_election_audit: p(
                "INSERT INTO {ks}.election_audit (scope, at_unix_secs, id, from_phase, to_phase, actor) \
                 VALUES (?, ?, ?, ?, ?, ?)"
                    .into(),
                quorum,
            )
            .await?,
            select_election_audit: p(
                "SELECT at_unix_secs, from_phase, to_phase, actor FROM {ks}.election_audit \
                 WHERE scope = ? LIMIT ?"
                    .into(),
                quorum,
            )
            .await?,
        };

        let store = Self {
            session,
            stmts,
            shard_count: config.shard_count,
            clock,
        };
        store.check_cluster_config().await?;
        Ok(store)
    }

    /// シャード数を DB に登録する（最初の 1 回）。既に別の値が登録されていれば接続を拒否する。
    ///
    /// api と sealer で `shard.count` が食い違うと、sealer が処理しないシャードに票が入って
    /// 封印されなくなる。それを起動時に検出する。
    async fn check_cluster_config(&self) -> Result<(), ConnectError> {
        let wanted = self.shard_count.get().to_string();
        let map = |e: StoreError| ConnectError::ClusterConfig(e.to_string());
        let inserted = with_retry("cluster_config", || {
            self.session.execute_unpaged(
                &self.stmts.insert_config,
                (SHARD_COUNT_KEY, wanted.as_str()),
            )
        })
        .await
        .map_err(map)?;
        if applied(inserted).map_err(map)? {
            return Ok(());
        }
        let stored = with_retry("cluster_config", || {
            self.session
                .execute_unpaged(&self.stmts.select_config, (SHARD_COUNT_KEY,))
        })
        .await
        .map_err(map)?;
        let stored = rows(stored)
            .and_then(|r| {
                r.maybe_first_row::<(Option<String>,)>()
                    .map_err(|e| decode_error("cluster_config", e))
            })
            .map_err(map)?;
        match stored {
            Some((Some(value),)) if value == wanted => Ok(()),
            Some((value,)) => Err(ConnectError::ClusterConfig(format!(
                "shard.count={wanted} ですが、DB には {} が登録されています（api と sealer で同じ値にしてください）",
                value.unwrap_or_default()
            ))),
            None => Err(ConnectError::ClusterConfig(
                "cluster_config を読めません".to_string(),
            )),
        }
    }

    /// 投票用紙の ID の列を、投票用紙ごとに数える。
    fn count_contests(
        result: QueryResult,
        into: &mut BTreeMap<ContestId, u64>,
    ) -> Result<(), StoreError> {
        for row in rows(result)?
            .rows::<(String,)>()
            .map_err(|e| decode_error("contest_id", e))?
        {
            let (contest,) = row.map_err(|e| decode_error("contest_id", e))?;
            let contest = ContestId::parse(&contest).map_err(|_| StoreError::Corrupt)?;
            *into.entry(contest).or_default() += 1;
        }
        Ok(())
    }

    fn shard_index(&self, shard: ShardId) -> Result<i32, StoreError> {
        if shard.0 < self.shard_count.get() {
            Ok(i32::from(shard.0))
        } else {
            Err(StoreError::InvalidShard)
        }
    }

    async fn read_head(&self, shard: i32) -> Result<Option<Block>, StoreError> {
        let result = with_retry("head", || {
            self.session
                .execute_unpaged(&self.stmts.select_head, (shard,))
        })
        .await?;
        decode_block(result)
    }

    async fn read_block(&self, shard: i32, height: i64) -> Result<Option<Block>, StoreError> {
        let result = with_retry("block", || {
            self.session
                .execute_unpaged(&self.stmts.select_block, (shard, height))
        })
        .await?;
        decode_block(result)
    }

    /// プールの全行のキー（`received_minute`, `ballot_id`）。ブロックの票から削除対象の行を特定する。
    async fn pool_keys(&self, shard: i32) -> Result<Vec<(i64, Vec<u8>)>, StoreError> {
        let result = with_retry("pool keys", || {
            self.session
                .execute_unpaged(&self.stmts.select_pool_keys, (shard,))
        })
        .await?;
        rows(result)?
            .rows::<(i64, Vec<u8>)>()
            .map_err(|e| decode_error("プールのキー", e))?
            .map(|row| row.map_err(|e| decode_error("プールのキー", e)))
            .collect()
    }

    async fn delete_pool_rows(
        &self,
        shard: i32,
        keys: &[(i64, Vec<u8>)],
    ) -> Result<(), StoreError> {
        // 同じパーティション（shard）への削除なので、バッチは原子的に適用される。
        for chunk in keys.chunks(DELETE_BATCH) {
            let mut batch = Batch::new(BatchType::Unlogged);
            for _ in chunk {
                batch.append_statement(self.stmts.delete_pool.clone());
            }
            batch.set_consistency(Consistency::LocalQuorum);
            batch.set_is_idempotent(true);
            // 削除の書き込み時刻は、行の INSERT より新しくなるよう行ごとに明示する（時計のずれで
            // 削除が無効にならないように）。
            let values = chunk
                .iter()
                .map(|(minute, id)| {
                    Ok((
                        pool_delete_timestamp(*minute)?,
                        shard,
                        *minute,
                        id.as_slice(),
                    ))
                })
                .collect::<Result<Vec<(i64, i32, i64, &[u8])>, StoreError>>()?;
            with_retry("pool delete", || self.session.batch(&batch, &values)).await?;
        }
        Ok(())
    }

    /// ブロックに封印済みの票を、プールから取り除く（冪等。既に無ければ何もしない）。
    async fn cleanup_pool(&self, shard: i32, block: &Block) -> Result<(), StoreError> {
        if block.ballots.is_empty() {
            return Ok(());
        }
        let sealed: HashSet<&[u8]> = block
            .ballots
            .iter()
            .map(|b| b.ballot_id.0.as_slice())
            .collect();
        let victims: Vec<(i64, Vec<u8>)> = self
            .pool_keys(shard)
            .await?
            .into_iter()
            .filter(|(_, id)| sealed.contains(id.as_slice()))
            .collect();
        self.delete_pool_rows(shard, &victims).await
    }

    /// participation の LWT が既存の行に負けたとき、その行が「自分の先の試行」のものかを調べる。
    async fn attempt_is_ours(
        &self,
        voter: &VoterId,
        contest: &str,
        attempt: &[u8],
    ) -> Result<bool, StoreError> {
        let result = with_retry("participation attempt", || {
            self.session
                .execute_unpaged(&self.stmts.select_attempt, (voter.as_str(), contest))
        })
        .await?;
        let existing = rows(result)?
            .maybe_first_row::<(Option<Vec<u8>>,)>()
            .map_err(|e| decode_error("attempt", e))?;
        Ok(matches!(existing, Some((Some(stored),)) if stored == attempt))
    }

    /// participation を取り消す（自分の `attempt` の行だけ。ベストエフォート）。
    async fn compensate(&self, voter: &VoterId, contest: &str, attempt: &[u8]) {
        let outcome = with_retry("participation compensation", || {
            self.session.execute_unpaged(
                &self.stmts.delete_participation,
                (voter.as_str(), contest, attempt),
            )
        })
        .await;
        if outcome.is_err() {
            // 補償に失敗すると「投票済みだが票がない」状態が残り得る（秘密投票のため突き合わせて復旧できない）。
            tracing::error!("投票の取り消し（補償）に失敗しました");
        }
    }
}

/// キースペースが存在すること（無ければ、スキーマの投入を促すエラー）。
async fn ensure_keyspace_exists(session: &Session, keyspace: &str) -> Result<(), ConnectError> {
    let missing = || {
        ConnectError::Connect(format!(
            "キースペース {keyspace} が存在しません（docs/schema.cql を、このキースペース名で投入してください）"
        ))
    };
    // 引用符なしの識別子は小文字に正規化されて保存される。
    let result = session
        .query_unpaged(
            "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
            (keyspace.to_ascii_lowercase(),),
        )
        .await
        .map_err(|e| ConnectError::Connect(e.to_string()))?;
    let found = result
        .into_rows_result()
        .map_err(|e| ConnectError::Connect(e.to_string()))?
        .maybe_first_row::<(String,)>()
        .map_err(|e| ConnectError::Connect(e.to_string()))?;
    found.map(|_| ()).ok_or_else(missing)
}

/// スキーマが現行（票の `contest_id` が文字列。ブロックの形式の版 2）であること。旧いスキーマ（`int`）のまま
/// 動かすと、型が合わずに実行時に失敗するので、接続時に、作り直しの手順つきで知らせる。
async fn ensure_schema_current(session: &Session, keyspace: &str) -> Result<(), ConnectError> {
    let result = session
        .query_unpaged(
            "SELECT type FROM system_schema.columns \
             WHERE keyspace_name = ? AND table_name = 'participation' AND column_name = 'contest_id'",
            (keyspace.to_ascii_lowercase(),),
        )
        .await
        .map_err(|e| ConnectError::Connect(e.to_string()))?;
    let column_type = result
        .into_rows_result()
        .map_err(|e| ConnectError::Connect(e.to_string()))?
        .maybe_first_row::<(String,)>()
        .map_err(|e| ConnectError::Connect(e.to_string()))?;
    match column_type {
        Some((t,)) if t == "text" => Ok(()),
        Some((t,)) => Err(ConnectError::Schema(format!(
            "キースペース {keyspace} のスキーマが古いです（participation.contest_id の型が {t}。現行は text）。\
             ID を文字列にしたため（ブロックの形式の版 2）、旧いスキーマ・旧いチェーンとは互換性がありません。\
             キースペースを DROP して、docs/schema.cql を投入し直してください（例: DROP KEYSPACE {keyspace}）。"
        ))),
        // テーブルが無い: スキーマの投入漏れ（準備の段階で、分かりやすいエラーになる）。
        None => Ok(()),
    }
}

async fn prepare(
    session: &Session,
    cql: String,
    consistency: Consistency,
) -> Result<PreparedStatement, ConnectError> {
    let mut statement = session
        .prepare(cql)
        .await
        .map_err(|e| ConnectError::Schema(e.to_string()))?;
    statement.set_consistency(consistency);
    // どの文も、再試行しても安全（冪等、または `attempt` で結果を判別できる）。
    statement.set_is_idempotent(true);
    Ok(statement)
}

fn decode_block(result: QueryResult) -> Result<Option<Block>, StoreError> {
    rows(result)?
        .maybe_first_row::<BlockRow>()
        .map_err(|e| decode_error("ブロック", e))?
        .map(block_from_row)
        .transpose()
}

#[async_trait]
impl VoteStore for ScyllaStore {
    /// 1. participation に LWT（`IF NOT EXISTS`）で記録する。既にあれば `AlreadyVoted`。
    /// 2. 票をプールに追加する（書き込み時刻は分に丸める）。失敗したら 1 を取り消す。
    ///
    /// 票を先に入れると、途中で落ちたときに二重投票が起き得るので、participation を先にする。
    async fn cast(&self, voter: &VoterId, shard: ShardId, ballot: Ballot) -> Result<(), CastError> {
        let shard = self.shard_index(shard)?;
        let contest = ballot.contest_id.as_str();
        let candidate_id = ballot.candidate_id.as_str();
        let (minute, timestamp_micros) = minute_bucket(self.clock.now_unix_secs())?;
        // participation 専用の乱数。ballot_id とは無関係にして、両者を結び付けない。
        let attempt: [u8; 16] = rand::random();

        let inserted = with_retry("participation LWT", || {
            self.session.execute_unpaged(
                &self.stmts.insert_participation,
                (voter.as_str(), contest, attempt.as_slice()),
            )
        })
        .await?;
        if !applied(inserted)? {
            // 既存の行が、タイムアウトして結果が分からなかった自分の先の試行なら、適用済みとして続ける。
            if !self.attempt_is_ours(voter, contest, &attempt).await? {
                return Err(CastError::AlreadyVoted);
            }
        }

        let ballot_id = ballot.ballot_id.0;
        let pooled = with_retry("pool insert", || {
            self.session.execute_unpaged(
                &self.stmts.insert_pool,
                (
                    shard,
                    minute,
                    ballot_id.as_slice(),
                    contest,
                    candidate_id,
                    timestamp_micros,
                ),
            )
        })
        .await;
        if let Err(e) = pooled {
            self.compensate(voter, contest, &attempt).await;
            return Err(e.into());
        }
        Ok(())
    }

    async fn voted_contests(&self, voter: &VoterId) -> Result<Vec<ContestId>, StoreError> {
        let result = with_retry("voted contests", || {
            self.session
                .execute_unpaged(&self.stmts.select_voted, (voter.as_str(),))
        })
        .await?;
        rows(result)?
            .rows::<(String,)>()
            .map_err(|e| decode_error("投票済みの投票用紙", e))?
            .map(|row| {
                let (contest,) = row.map_err(|e| decode_error("投票済みの投票用紙", e))?;
                ContestId::parse(&contest).map_err(|_| StoreError::Corrupt)
            })
            .collect()
    }

    async fn pending_by_shard(&self) -> Result<Vec<usize>, StoreError> {
        let mut counts = Vec::with_capacity(usize::from(self.shard_count.get()));
        for shard in 0..self.shard_count.get() {
            counts.push(self.pending_len(ShardId(shard)).await?);
        }
        Ok(counts)
    }

    /// participation とプールを、それぞれ投票用紙別に数える（突き合わせない）。
    /// 全体を 1 回で読むので、監査用途以外で頻繁に呼ばないこと。
    async fn audit_counts(&self) -> Result<Vec<ContestCounts>, StoreError> {
        let mut participation = BTreeMap::new();
        let result = with_retry("audit participation", || {
            self.session
                .execute_unpaged(&self.stmts.select_participation_contests, &[])
        })
        .await?;
        Self::count_contests(result, &mut participation)?;

        let mut pending = BTreeMap::new();
        for shard in 0..self.shard_count.get() {
            let shard = i32::from(shard);
            let result = with_retry("audit pool", || {
                self.session
                    .execute_unpaged(&self.stmts.select_pool_contests, (shard,))
            })
            .await?;
            Self::count_contests(result, &mut pending)?;
        }

        let contests: BTreeSet<ContestId> = participation
            .keys()
            .chain(pending.keys())
            .cloned()
            .collect();
        Ok(contests
            .into_iter()
            .map(|contest| ContestCounts {
                participation: participation.get(&contest).copied().unwrap_or(0),
                pending: pending.get(&contest).copied().unwrap_or(0),
                contest,
            })
            .collect())
    }
}

#[async_trait]
impl ChainRead for ScyllaStore {
    async fn head(&self, shard: ShardId) -> Result<Option<Block>, StoreError> {
        match self.shard_index(shard) {
            Ok(shard) => self.read_head(shard).await,
            Err(_) => Ok(None),
        }
    }

    async fn block(&self, shard: ShardId, height: u64) -> Result<Option<Block>, StoreError> {
        let (Ok(shard), Ok(height)) = (self.shard_index(shard), i64::try_from(height)) else {
            return Ok(None);
        };
        self.read_block(shard, height).await
    }

    async fn latest_anchor(&self) -> Result<Option<Anchor>, StoreError> {
        let result = with_retry("latest anchor", || {
            self.session
                .execute_unpaged(&self.stmts.select_latest_anchor, (ANCHOR_SCOPE,))
        })
        .await?;
        rows(result)?
            .maybe_first_row::<AnchorRow>()
            .map_err(|e| decode_error("アンカー", e))?
            .map(anchor_from_row)
            .transpose()
    }

    /// 範囲読み取り（クラスタリング順が高さの降順なので、そのまま新しい順）。
    async fn blocks_before(
        &self,
        shard: ShardId,
        before_height: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Block>, StoreError> {
        // CQL の `LIMIT 0` は不正なので、0 件は問い合わせずに返す。
        let (Ok(shard), true) = (self.shard_index(shard), limit > 0) else {
            return Ok(Vec::new());
        };
        let limit = i32::try_from(limit).unwrap_or(i32::MAX);
        let result = match before_height {
            None => {
                with_retry("blocks", || {
                    self.session
                        .execute_unpaged(&self.stmts.select_blocks_top, (shard, limit))
                })
                .await?
            }
            Some(before) => {
                // `height` は bigint。i64 に収まらない値は、「すべてのブロックより先」と同じ。
                let before = i64::try_from(before).unwrap_or(i64::MAX);
                with_retry("blocks", || {
                    self.session
                        .execute_unpaged(&self.stmts.select_blocks_before, (shard, before, limit))
                })
                .await?
            }
        };
        rows(result)?
            .rows::<BlockRow>()
            .map_err(|e| decode_error("ブロック", e))?
            .map(|row| block_from_row(row.map_err(|e| decode_error("ブロック", e))?))
            .collect()
    }

    async fn latest_anchors(&self, limit: usize) -> Result<Vec<Anchor>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i32::try_from(limit).unwrap_or(i32::MAX);
        let result = with_retry("anchors", || {
            self.session
                .execute_unpaged(&self.stmts.select_anchors, (ANCHOR_SCOPE, limit))
        })
        .await?;
        rows(result)?
            .rows::<AnchorRow>()
            .map_err(|e| decode_error("アンカー", e))?
            .map(|row| anchor_from_row(row.map_err(|e| decode_error("アンカー", e))?))
            .collect()
    }

    async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError> {
        let result = with_retry("signer key", || {
            self.session
                .execute_unpaged(&self.stmts.select_signer, (SIGNER_KEY_ID,))
        })
        .await?;
        rows(result)?
            .maybe_first_row::<(Vec<u8>,)>()
            .map_err(|e| decode_error("署名鍵", e))?
            .map(|(key,)| key.as_slice().try_into().map_err(|_| StoreError::Corrupt))
            .transpose()
    }
}

#[async_trait]
impl SealStore for ScyllaStore {
    async fn pending_len(&self, shard: ShardId) -> Result<usize, StoreError> {
        let shard = self.shard_index(shard)?;
        let result = with_retry("pool count", || {
            self.session
                .execute_unpaged(&self.stmts.count_pool, (shard,))
        })
        .await?;
        let (count,) = rows(result)?
            .first_row::<(i64,)>()
            .map_err(|e| decode_error("件数", e))?;
        usize::try_from(count).map_err(|_| StoreError::Corrupt)
    }

    async fn peek_pending(&self, shard: ShardId, n: usize) -> Result<Vec<Ballot>, StoreError> {
        let shard = self.shard_index(shard)?;
        let limit = i32::try_from(n).map_err(|_| StoreError::Corrupt)?;
        let result = with_retry("pool peek", || {
            self.session
                .execute_unpaged(&self.stmts.select_pool_head, (shard, limit))
        })
        .await?;
        rows(result)?
            .rows::<(i64, Vec<u8>, String, String)>()
            .map_err(|e| decode_error("プールの票", e))?
            .map(|row| {
                let (_minute, id, contest, candidate) =
                    row.map_err(|e| decode_error("プールの票", e))?;
                ballot_from_tuple(&(id, contest, candidate))
            })
            .collect()
    }

    /// 1. 高さ・`prev_hash` の連続性と、ブロックの票がプールにあることを検証する。
    /// 2. ブロックを LWT（`IF NOT EXISTS`）で追加する。
    /// 3. プールから票を削除する。
    ///
    /// 2 と 3 の間で落ちても、`recover` と、同じブロックでの `commit` の再試行で復旧できる
    /// （既存のブロックのハッシュが同じなら、再試行とみなして 3 だけを行う）。
    async fn commit(
        &self,
        shard: ShardId,
        block: Block,
        consumed: usize,
    ) -> Result<(), StoreError> {
        let shard = self.shard_index(shard)?;
        let values = block_to_values(&block)?;

        // 同じ高さに既にブロックがある: 同一なら再試行（プールの後始末だけ）、別物なら矛盾。
        if let Some(existing) = self.read_block(shard, values.height).await? {
            if existing.block_hash == block.block_hash {
                return self.cleanup_pool(shard, &block).await;
            }
            return Err(StoreError::Conflict);
        }

        let continues = match self.read_head(shard).await? {
            None => block.header.height == 0,
            Some(head) => {
                head.header.height.checked_add(1) == Some(block.header.height)
                    && block.header.prev_hash == head.block_hash
            }
        };
        if !continues || consumed != block.ballots.len() {
            return Err(StoreError::Conflict);
        }
        if !block.ballots.is_empty() {
            let present: HashSet<Vec<u8>> = self
                .pool_keys(shard)
                .await?
                .into_iter()
                .map(|(_, id)| id)
                .collect();
            if !block
                .ballots
                .iter()
                .all(|b| present.contains(b.ballot_id.0.as_slice()))
            {
                return Err(StoreError::Conflict);
            }
        }

        let inserted = with_retry("block LWT", || {
            self.session.execute_unpaged(
                &self.stmts.insert_block,
                (
                    shard,
                    values.height,
                    values.format_version,
                    values.prev_hash.as_slice(),
                    values.merkle_root.as_slice(),
                    values.ballot_count,
                    values.sealed_at_minute,
                    values.block_hash.as_slice(),
                    values.signature.as_slice(),
                    &values.ballots as &Vec<BallotTuple>,
                ),
            )
        })
        .await?;
        if !applied(inserted)? {
            // 負けた: 既存のブロックが自分のもの（タイムアウトで結果が不明だった先の試行）なら成功として続ける。
            let existing = with_retry("block hash", || {
                self.session
                    .execute_unpaged(&self.stmts.select_block_hash, (shard, values.height))
            })
            .await?;
            let stored = rows(existing)?
                .maybe_first_row::<(Vec<u8>,)>()
                .map_err(|e| decode_error("block_hash", e))?;
            if !matches!(stored, Some((hash,)) if hash == values.block_hash) {
                return Err(StoreError::Conflict);
            }
        }
        self.cleanup_pool(shard, &block).await
    }

    async fn register_signer(&self, public_key: [u8; 32]) -> Result<(), StoreError> {
        let inserted = with_retry("signer LWT", || {
            self.session.execute_unpaged(
                &self.stmts.insert_signer,
                (SIGNER_KEY_ID, public_key.as_slice()),
            )
        })
        .await?;
        if applied(inserted)? {
            return Ok(());
        }
        // 既に登録済み: 同じ鍵なら成功、別の鍵なら矛盾。
        match self.signer_public_key().await? {
            Some(existing) if existing == public_key => Ok(()),
            _ => Err(StoreError::Conflict),
        }
    }

    async fn append_anchor(&self, anchor: &Anchor) -> Result<bool, StoreError> {
        let v = anchor_to_values(anchor)?;
        let inserted = with_retry("anchor LWT", || {
            self.session.execute_unpaged(
                &self.stmts.insert_anchor,
                (
                    ANCHOR_SCOPE,
                    v.seq,
                    v.anchor_minute,
                    v.prev_anchor_hash.as_slice(),
                    &v.heads,
                    v.anchor_hash.as_slice(),
                    v.signature.as_slice(),
                ),
            )
        })
        .await?;
        if applied(inserted)? {
            return Ok(true);
        }
        // 負けた。既存の同じ seq のアンカーが自分のもの（結果が不明だった先の試行）なら成功として扱う。
        Ok(self.latest_anchor().await?.is_some_and(|latest| {
            latest.seq == anchor.seq && latest.anchor_hash == anchor.anchor_hash
        }))
    }

    async fn recover(&self, shard: ShardId) -> Result<(), StoreError> {
        let shard = self.shard_index(shard)?;
        if let Some(head) = self.read_head(shard).await? {
            self.cleanup_pool(shard, &head).await?;
        }
        Ok(())
    }
}

/// TTL（秒）。0 秒だと即座に切れてしまうので 1 秒以上にする。
fn ttl_secs(ttl: Duration) -> Result<i32, StoreError> {
    i32::try_from(ttl.as_secs().max(1)).map_err(|_| StoreError::Corrupt)
}

#[async_trait]
impl LeaseStore for ScyllaStore {
    async fn try_acquire(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        let ttl = ttl_secs(ttl)?;
        let inserted = with_retry("lease acquire", || {
            self.session
                .execute_unpaged(&self.stmts.acquire_lease, (name, owner, ttl))
        })
        .await?;
        if applied(inserted)? {
            return Ok(true);
        }
        // 負けた。既存の行が自分のもの（結果が不明だった先の試行が適用済み）なら、取得できているので成功。
        let existing = with_retry("lease owner", || {
            self.session
                .execute_unpaged(&self.stmts.select_lease_owner, (name,))
        })
        .await?;
        let existing = rows(existing)?
            .maybe_first_row::<(Option<String>,)>()
            .map_err(|e| decode_error("リース", e))?;
        Ok(matches!(existing, Some((Some(current),)) if current == owner))
    }

    async fn renew(&self, name: &str, owner: &str, ttl: Duration) -> Result<bool, StoreError> {
        let ttl = ttl_secs(ttl)?;
        // `IF owner = ?` なので、失っていれば（別の owner・期限切れで行がない）適用されない。
        let updated = with_retry("lease renew", || {
            self.session
                .execute_unpaged(&self.stmts.renew_lease, (ttl, owner, name, owner))
        })
        .await?;
        applied(updated)
    }

    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError> {
        with_retry("lease release", || {
            self.session
                .execute_unpaged(&self.stmts.release_lease, (name, owner))
        })
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 選挙状態（scheduled → open → closing → closed。原則17）
// ---------------------------------------------------------------------------

/// `election_state` の 1 行（phase, opens_at, closes_at, opened_at, closing_started_at）。
type ElectionStateRow = (
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn election_state_row(row: ElectionStateRow) -> Result<ElectionStateSnapshot, StoreError> {
    let (phase, opens_at, closes_at, opened_at, closing_started_at) = row;
    let phase = phase
        .as_deref()
        .and_then(ElectionPhase::parse)
        .ok_or(StoreError::Corrupt)?;
    Ok(ElectionStateSnapshot {
        phase,
        period: Period {
            opens_at,
            closes_at,
        },
        opened_at,
        closing_started_at,
    })
}

#[async_trait]
impl ElectionStateStore for ScyllaStore {
    async fn ensure_initialized(
        &self,
        period: Period,
    ) -> Result<ElectionStateSnapshot, StoreError> {
        let inserted = with_retry("election_state init", || {
            self.session.execute_unpaged(
                &self.stmts.insert_election_state,
                (ELECTION_SCOPE, period.opens_at, period.closes_at),
            )
        })
        .await?;
        if applied(inserted)? {
            return Ok(ElectionStateSnapshot {
                phase: ElectionPhase::Scheduled,
                period,
                opened_at: None,
                closing_started_at: None,
            });
        }
        self.get_election_state().await
    }

    async fn get(&self) -> Result<ElectionStateSnapshot, StoreError> {
        self.get_election_state().await
    }

    async fn schedule(&self, period: Period, _at_unix_secs: i64) -> Result<bool, StoreError> {
        let updated = with_retry("election_state schedule", || {
            self.session.execute_unpaged(
                &self.stmts.update_election_period,
                (period.opens_at, period.closes_at, ELECTION_SCOPE),
            )
        })
        .await?;
        applied(updated)
    }

    async fn transition(
        &self,
        from: ElectionPhase,
        to: ElectionPhase,
        actor: &str,
        at_unix_secs: i64,
    ) -> Result<bool, StoreError> {
        if !from.can_advance_to(to) {
            // 原則17: 1 段の順序どおりの遷移だけを許す（呼び出し側の不具合を、ここで止める）。
            return Ok(false);
        }
        let updated = if to == ElectionPhase::Open {
            with_retry("election_state transition", || {
                self.session.execute_unpaged(
                    &self.stmts.update_election_phase_open,
                    (to.as_str(), at_unix_secs, ELECTION_SCOPE, from.as_str()),
                )
            })
            .await?
        } else if to == ElectionPhase::Closing {
            with_retry("election_state transition", || {
                self.session.execute_unpaged(
                    &self.stmts.update_election_phase_closing,
                    (to.as_str(), at_unix_secs, ELECTION_SCOPE, from.as_str()),
                )
            })
            .await?
        } else {
            with_retry("election_state transition", || {
                self.session.execute_unpaged(
                    &self.stmts.update_election_phase,
                    (to.as_str(), ELECTION_SCOPE, from.as_str()),
                )
            })
            .await?
        };
        if !applied(updated)? {
            return Ok(false);
        }
        // 監査ログはベストエフォート: 失敗しても状態遷移そのものは既に成功している。
        let id: Vec<u8> = rand::random::<[u8; 8]>().to_vec();
        if let Err(e) = with_retry("election_audit insert", || {
            self.session.execute_unpaged(
                &self.stmts.insert_election_audit,
                (
                    ELECTION_SCOPE,
                    at_unix_secs,
                    id.clone(),
                    from.as_str(),
                    to.as_str(),
                    actor,
                ),
            )
        })
        .await
        {
            tracing::warn!(error = %e, "election_audit への記録に失敗しました（状態遷移は成功しています）");
        }
        Ok(true)
    }

    async fn recent_audit(&self, limit: usize) -> Result<Vec<ElectionAuditEntry>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i32::try_from(limit).unwrap_or(i32::MAX);
        let result = with_retry("election_audit select", || {
            self.session
                .execute_unpaged(&self.stmts.select_election_audit, (ELECTION_SCOPE, limit))
        })
        .await?;
        rows(result)?
            .rows::<(i64, Option<String>, Option<String>, Option<String>)>()
            .map_err(|e| decode_error("選挙状態の監査ログ", e))?
            .map(|row| {
                let (at_unix_secs, from, to, actor) =
                    row.map_err(|e| decode_error("選挙状態の監査ログ", e))?;
                let from = from
                    .as_deref()
                    .and_then(ElectionPhase::parse)
                    .ok_or(StoreError::Corrupt)?;
                let to = to
                    .as_deref()
                    .and_then(ElectionPhase::parse)
                    .ok_or(StoreError::Corrupt)?;
                Ok(ElectionAuditEntry {
                    at_unix_secs,
                    from,
                    to,
                    actor: actor.ok_or(StoreError::Corrupt)?,
                })
            })
            .collect()
    }
}

impl ScyllaStore {
    async fn get_election_state(&self) -> Result<ElectionStateSnapshot, StoreError> {
        let result = with_retry("election_state get", || {
            self.session
                .execute_unpaged(&self.stmts.select_election_state, (ELECTION_SCOPE,))
        })
        .await?;
        let row = rows(result)?
            .maybe_first_row::<ElectionStateRow>()
            .map_err(|e| decode_error("選挙状態", e))?
            .ok_or(StoreError::Unavailable)?;
        election_state_row(row)
    }
}

// ---------------------------------------------------------------------------
// 認証情報（auth.mode=db）・有権者名簿・credgen の台帳
// ---------------------------------------------------------------------------

#[async_trait]
impl CredentialStore for ScyllaStore {
    async fn find(&self, login_id: &str) -> Result<Option<CredentialRecord>, StoreError> {
        let result = with_retry("credential find", || {
            self.session
                .execute_unpaged(&self.stmts.select_credential, (login_id,))
        })
        .await?;
        let row = rows(result)?
            .maybe_first_row::<(Option<String>, Option<String>)>()
            .map_err(|e| decode_error("認証情報", e))?;
        match row {
            None => Ok(None),
            Some((Some(password_hash), Some(voter_id))) => Ok(Some(CredentialRecord {
                login_id: login_id.to_string(),
                password_hash,
                voter_id: VoterId::new(&voter_id).map_err(|_| StoreError::Corrupt)?,
            })),
            // 列が欠けた行は、壊れたデータ（認証を通さない）。
            Some(_) => Err(StoreError::Corrupt),
        }
    }
}

#[async_trait]
impl CredentialAdmin for ScyllaStore {
    async fn registry_get(&self, external_id: &str) -> Result<Option<RegistryEntry>, StoreError> {
        let result = with_retry("registry get", || {
            self.session
                .execute_unpaged(&self.stmts.select_registry, (external_id,))
        })
        .await?;
        let row = rows(result)?
            .maybe_first_row::<(Option<String>, Option<String>)>()
            .map_err(|e| decode_error("台帳", e))?;
        match row {
            None => Ok(None),
            Some((Some(voter_id), Some(login_id))) => Ok(Some(RegistryEntry {
                external_id: external_id.to_string(),
                voter_id: VoterId::new(&voter_id).map_err(|_| StoreError::Corrupt)?,
                login_id,
            })),
            Some(_) => Err(StoreError::Corrupt),
        }
    }

    async fn insert_credential(&self, record: &CredentialRecord) -> Result<bool, StoreError> {
        let inserted = with_retry("credential insert", || {
            self.session.execute_unpaged(
                &self.stmts.insert_credential,
                (
                    record.login_id.as_str(),
                    record.password_hash.as_str(),
                    record.voter_id.as_str(),
                ),
            )
        })
        .await?;
        applied(inserted)
    }

    async fn delete_credential(&self, login_id: &str) -> Result<(), StoreError> {
        with_retry("credential delete", || {
            self.session
                .execute_unpaged(&self.stmts.delete_credential, (login_id,))
        })
        .await?;
        Ok(())
    }

    async fn upsert_registry(&self, entry: &RegistryEntry) -> Result<(), StoreError> {
        with_retry("registry upsert", || {
            self.session.execute_unpaged(
                &self.stmts.upsert_registry,
                (
                    entry.external_id.as_str(),
                    entry.voter_id.as_str(),
                    entry.login_id.as_str(),
                ),
            )
        })
        .await?;
        Ok(())
    }

    async fn put_roll(&self, voter: &VoterId, districts: &[DistrictId]) -> Result<(), StoreError> {
        let districts: Vec<&str> = districts.iter().map(DistrictId::as_str).collect();
        with_retry("roll put", || {
            self.session
                .execute_unpaged(&self.stmts.put_roll, (voter.as_str(), &districts))
        })
        .await?;
        Ok(())
    }
}

/// 有権者名簿（`voter_roll` テーブル）。`auth.mode=db` のとき、api は名簿をここから引く（状態を持たない）。
#[async_trait]
impl VoterRoll for ScyllaStore {
    async fn districts_of(&self, voter: &VoterId) -> Result<Option<Vec<DistrictId>>, StoreError> {
        let result = with_retry("roll get", || {
            self.session
                .execute_unpaged(&self.stmts.select_roll, (voter.as_str(),))
        })
        .await?;
        let row = rows(result)?
            .maybe_first_row::<(Option<Vec<String>>,)>()
            .map_err(|e| decode_error("名簿", e))?;
        match row {
            None => Ok(None),
            Some((districts,)) => districts
                .unwrap_or_default()
                .iter()
                .map(|d| DistrictId::new(d).map_err(|_| StoreError::Corrupt))
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
        }
    }
}
