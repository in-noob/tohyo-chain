//! 封印ロジック本体。判定は `domain::seal_policy` の純粋関数に委ね、ここは状態（前回の封印時刻・
//! 投票開始時刻）とストア操作だけを担う。
//!
//! シャードごとに独立して判定し、シャードごとに**単一の** `Sealer` だけが書き込む前提。
//!
//! 経過時間の起点は max(前回の封印時刻, 投票開始時刻)（原則9。ADR 0020）。時刻は壁時計の UNIX 秒で測る
//! （投票開始時刻は、DB の選挙状態に記録された壁時計の時刻なので、同じ時計で比べる）。

use std::fmt;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use application::{Clock, SealStore, StoreError};
use domain::anchor::GENESIS_ANCHOR_PREV;
use domain::seal_policy::{SealDecision, SealPolicy, decide, decide_close, window_start};
use domain::types::unix_minutes;
use domain::{
    Anchor, AnchorError, HeadsChange, SealError, ShardHead, ShardId, Signer, build_anchor,
    compare_heads, genesis, seal_block,
};
use shared_types::hex;

use crate::clock::MonotonicClock;

/// 封印のきっかけ。ログの `trigger=` に出る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 未封印が `seal.max_ballots` 件に達した。
    Count,
    /// 前回の封印（または投票開始）から `seal.interval_secs` 秒以上経ち、未封印が
    /// `seal.min_ballots_after_interval` 件以上ある。
    Time,
    /// 投票終了（締切の手続き）の中での、残りの封印。
    Close,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Count => "count",
            Self::Time => "time",
            Self::Close => "close",
        })
    }
}

/// 1 回の封印の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealEvent {
    pub shard: ShardId,
    pub height: u64,
    pub count: usize,
    pub trigger: Trigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Seal(#[from] SealError),
    #[error("シャード {0} のチェーンが初期化されていません（init が必要）")]
    NotInitialized(u16),
    /// リースの有効期限が切れた（または失った）ので、書き込まずに中止した。
    #[error("シャード {0} のリースを失いました")]
    LeaseLost(u16),
    #[error(transparent)]
    Anchor(#[from] AnchorError),
    /// 直前のアンカーと矛盾する head（巻き戻し・分岐の疑い）。新しいアンカーで上書きしない。
    #[error("直前のアンカーと矛盾する head です（チェーンの巻き戻し・分岐の疑い）: {0}")]
    AnchorInconsistent(&'static str),
    /// 既にあるチェーンの選挙定義のハッシュが、手元の seed のハッシュと違う（init の後に seed を書き換えた。ADR 0025）。
    #[error(
        "シャード {shard} のチェーンの選挙定義のハッシュ（{}）が、手元の選挙データ（seed）のハッシュ（{}）と一致しません。\
         init の後に選挙データを書き換えた可能性があります。seed を init の時点の内容に戻すか、選挙をやり直すなら \
         scripts/db_reset.sh --all で作り直してください",
        hex::encode(chain),
        hex::encode(seed)
    )]
    ElectionMismatch {
        shard: u16,
        chain: domain::Hash32,
        seed: domain::Hash32,
    },
}

/// [`Sealer::finalize_anchor`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalAnchor {
    /// 直前のアンカー以降に変化があったので、最後のアンカーを最新の状態に追いつかせるために作った。
    Created(Anchor),
    /// 変化がなく、最後のアンカー（まだ無ければ、ジェネシスだけの状態）が最新の状態を指している。新しく作らない。
    UpToDate,
    /// 全シャードにチェーンが揃っていない、または同じ番号のアンカーを他のプロセスに先に書かれた。
    Unavailable,
}

/// 書き込み（ブロックの `commit`）の直前に確認する「リースがまだ有効か」の判定。
/// `false` なら書き込まずに `LeaseLost` で中止する。リースを使わない実行では常に `true`。
pub type LeaseGuard<'a> = &'a (dyn Fn() -> bool + Send + Sync);

fn always_valid() -> bool {
    true
}

/// `tick` / `close_flush` の結果。一部のシャードが失敗しても、他のシャードの処理は続ける。
#[derive(Debug, Default)]
pub struct TickOutcome {
    pub events: Vec<SealEvent>,
    pub errors: Vec<SealerError>,
}

pub struct Sealer {
    store: Arc<dyn SealStore>,
    signer: Arc<dyn Signer + Send + Sync>,
    /// 封印の経過時間の判定と、ブロックの `sealed_at_minute` 用の壁時計（UNIX 秒）。
    wall: Arc<dyn Clock>,
    /// リース・アンカーの周期・締切の待ち時間を測る単調時計。
    mono: Arc<dyn MonotonicClock>,
    policy: SealPolicy,
    /// 選挙定義のハッシュ（`domain::election_definition_hash`）。ジェネシスに入れ、既にあるチェーンとは照合する。
    election_hash: domain::Hash32,
    /// シャードごとの、経過時間の起点の材料。添字がシャード番号。
    shards: Vec<ShardClock>,
    /// 投票開始時刻（UNIX 秒）。選挙状態が open になるまでは `None`（[`Sealer::set_voting_started_at`]）。
    voting_started_at: Option<u64>,
}

/// 1 つのシャードの、経過時間の起点の材料。
#[derive(Debug, Clone, Copy)]
struct ShardClock {
    /// 前回、票を封印した時刻（UNIX 秒）。まだ封印していなければ `None`。
    last_sealed_at: Option<u64>,
    /// このシャードを担当し始めた時刻（UNIX 秒）。投票開始時刻が分からない（選挙状態を使わない実行）ときの、
    /// 投票開始時刻の代わり。
    taken_at: u64,
}

impl Sealer {
    pub fn new(
        store: Arc<dyn SealStore>,
        signer: Arc<dyn Signer + Send + Sync>,
        wall: Arc<dyn Clock>,
        mono: Arc<dyn MonotonicClock>,
        policy: SealPolicy,
        shard_count: NonZeroU16,
        election_hash: domain::Hash32,
    ) -> Self {
        let now = wall.now_unix_secs();
        Self {
            store,
            signer,
            wall,
            mono,
            policy,
            election_hash,
            shards: vec![
                ShardClock {
                    last_sealed_at: None,
                    taken_at: now,
                };
                usize::from(shard_count.get())
            ],
            voting_started_at: None,
        }
    }

    fn shards(&self) -> impl Iterator<Item = ShardId> + use<> {
        (0..self.shards.len() as u16).map(ShardId)
    }

    /// 投票開始時刻（UNIX 秒）を設定する。呼び出し側（runner / Coordinator）が、選挙状態を読むたびに
    /// `domain::voting_started_at` の結果を渡す（まだ open でなければ `None`）。
    pub fn set_voting_started_at(&mut self, started_at: Option<i64>) {
        self.voting_started_at = started_at.and_then(|t| u64::try_from(t).ok());
    }

    /// 現在の単調時計の値。
    pub fn now(&self) -> Duration {
        self.mono.elapsed()
    }

    /// 判定に使う単調時計（リースの有効期限の確認にも同じものを使う）。
    pub fn clock(&self) -> Arc<dyn MonotonicClock> {
        self.mono.clone()
    }

    /// 担当するシャード数。
    pub fn shard_count(&self) -> u16 {
        self.shards.len() as u16
    }

    /// 壁時計（`wall.now_unix_secs()`）。選挙状態の自動遷移の判定に使う。
    pub fn wall_now_unix_secs(&self) -> u64 {
        self.wall.now_unix_secs()
    }

    /// 全シャードの未封印の票の件数の合計。締切の手続きで「未封印が 0 件」を確認するのに使う。
    pub async fn total_pending(&self) -> Result<usize, SealerError> {
        let mut total = 0usize;
        for shard in self.shards() {
            total += self.store.pending_len(shard).await?;
        }
        Ok(total)
    }

    /// 署名の公開鍵を DB に登録する。別の鍵が登録済みなら `Conflict`（1 本のチェーンに別の鍵の署名が
    /// 混ざるのを防ぐ）。同じ鍵の再登録は成功する。
    pub async fn register_signer(&self) -> Result<(), SealerError> {
        self.store.register_signer(self.signer.public_key()).await?;
        Ok(())
    }

    /// 既にある全シャードのチェーン（先頭ブロックの値。ジェネシスから引き継いでいる）が、手元の seed の選挙定義のハッシュと
    /// 同じことを確かめる（ADR 0025）。チェーンがまだ無いシャードは飛ばす。独立した sealer が起動時に呼び、違えば起動を拒否する
    /// （リースを取った後の `init_shard` でも確かめるが、そこでの失敗はリースを返して続けるだけなので、先に止める）。
    pub async fn check_existing_chains(&self) -> Result<(), SealerError> {
        for shard in self.shards() {
            if let Some(head) = self.store.head(shard).await?
                && head.header.election_hash != self.election_hash
            {
                return Err(SealerError::ElectionMismatch {
                    shard: shard.0,
                    chain: head.header.election_hash,
                    seed: self.election_hash,
                });
            }
        }
        Ok(())
    }

    /// 全シャードの復旧とジェネシスの作成を行う（プロセス内で全シャードを扱う実行用）。
    /// 署名鍵も登録する。
    pub async fn init(&mut self) -> Result<(), SealerError> {
        self.register_signer().await?;
        for shard in self.shards() {
            self.init_shard(shard).await?;
        }
        Ok(())
    }

    /// 1 つのシャードを担当し始めるときの初期化（リースを取得した直後に呼ぶ）。
    ///
    /// 前回の封印の途中（ブロック追加後、プール削除前）で落ちていれば `recover` が票の二重封印を
    /// 防ぎ、チェーンが既にあればジェネシスは作らない。
    ///
    /// 前回の封印時刻は、チェーンの先頭ブロック（票を含むもの）の `sealed_at_minute` から求める（別のプロセスが
    /// 封印した後に引き継いだ場合や、再起動した場合）。時刻は分単位に丸めて保存しているので、その分の最後の秒
    /// （`分 * 60 + 59`）を使う。実際の封印時刻より遅い側に寄せることで、`seal.interval_secs` より早く時間による
    /// 封印をすることはない（遅れは最大 59 秒）。
    ///
    /// ジェネシスには、選挙定義のハッシュを入れる。チェーンが既にあれば、先頭ブロックの値（ジェネシスから引き継いだもの）が
    /// 手元の seed のハッシュと同じことを確かめ、違えば `ElectionMismatch`（違う選挙定義のまま封印しない。ADR 0025）。
    pub async fn init_shard(&mut self, shard: ShardId) -> Result<(), SealerError> {
        if usize::from(shard.0) >= self.shards.len() {
            return Err(StoreError::InvalidShard.into());
        }
        self.store.recover(shard).await?;
        if self.store.head(shard).await?.is_none() {
            let minute = unix_minutes(self.wall.now_unix_secs());
            let block = genesis(&*self.signer, minute, self.election_hash);
            match self.store.commit(shard, block, 0).await {
                Ok(()) => {}
                // 別のプロセスが同時にジェネシスを作った（追加で競り負けた）。チェーンがあれば成功。
                Err(StoreError::Conflict) if self.store.head(shard).await?.is_some() => {}
                Err(e) => return Err(e.into()),
            }
        }
        let head = self
            .store
            .head(shard)
            .await?
            .ok_or(SealerError::NotInitialized(shard.0))?;
        if head.header.election_hash != self.election_hash {
            return Err(SealerError::ElectionMismatch {
                shard: shard.0,
                chain: head.header.election_hash,
                seed: self.election_hash,
            });
        }
        let last_sealed_at = (head.header.height > 0).then(|| {
            head.header
                .sealed_at_minute
                .saturating_mul(60)
                .saturating_add(59)
        });
        self.shards[usize::from(shard.0)] = ShardClock {
            last_sealed_at,
            taken_at: self.wall.now_unix_secs(),
        };
        Ok(())
    }

    /// 全シャードに封印ポリシーを適用する。ワーカーが定期的に呼ぶ。
    pub async fn tick(&mut self) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        for shard in self.shards() {
            if let Err(e) = self
                .tick_shard(shard, &always_valid, &mut outcome.events)
                .await
            {
                tracing::error!(shard = shard.0, error = %e, "封印に失敗しました");
                outcome.errors.push(e);
            }
        }
        outcome
    }

    /// 投票終了（締切の手続き）の中での、残りの封印（全シャード）。SIGTERM での停止では呼ばない（原則9）。
    pub async fn close_flush(&mut self) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        for shard in self.shards() {
            if let Err(e) = self
                .close_flush_shard(shard, &always_valid, &mut outcome.events)
                .await
            {
                tracing::error!(shard = shard.0, error = %e, "フラッシュに失敗しました");
                outcome.errors.push(e);
            }
        }
        outcome
    }

    /// 1 つのシャードに封印ポリシーを適用する。`guard` は、ブロックを書く直前に呼ぶ「リースが有効か」の判定。
    pub async fn tick_shard(
        &mut self,
        shard: ShardId,
        guard: LeaseGuard<'_>,
        events: &mut Vec<SealEvent>,
    ) -> Result<(), SealerError> {
        let index = usize::from(shard.0);
        let Some(clock) = self.shards.get(index).copied() else {
            return Err(StoreError::InvalidShard.into());
        };
        let now = self.wall.now_unix_secs();
        // 投票開始時刻が分からない（選挙状態を使わない実行）ときは、担当し始めた時刻で代用する。
        let started_at = self.voting_started_at.unwrap_or(clock.taken_at);
        loop {
            let pending = self.store.pending_len(shard).await?;
            let start = window_start(self.shards[index].last_sealed_at, started_at);
            let decision = decide(pending, start, now, &self.policy);
            let (count, trigger) = match decision {
                // 0 件のときも待つ。起点（前回の封印時刻）は動かさない（原則9）。
                SealDecision::Wait => return Ok(()),
                SealDecision::SealCount(n) => (n, Trigger::Count),
                SealDecision::SealAll => (pending, Trigger::Time),
                // `decide` は返さない（投票終了の手続きの `decide_close` だけが返す）。
                SealDecision::CloseFlush => (pending, Trigger::Close),
            };
            events.push(self.seal(shard, count, trigger, guard).await?);
            self.shards[index].last_sealed_at = Some(now);
        }
    }

    /// 1 つのシャードの、投票終了（締切の手続き）の中での封印。件数や経過時間に関係なく、残りをすべて封印する。
    /// `seal.max_ballots` 件以上残っていれば、その件数ずつ（trigger=count）、最後の残りを 1 ブロック（trigger=close）
    /// にする（ブロックの票数が `seal.max_ballots` を超えないようにするため）。0 件なら何もしない。
    pub async fn close_flush_shard(
        &mut self,
        shard: ShardId,
        guard: LeaseGuard<'_>,
        events: &mut Vec<SealEvent>,
    ) -> Result<(), SealerError> {
        let index = usize::from(shard.0);
        if index >= self.shards.len() {
            return Err(StoreError::InvalidShard.into());
        }
        loop {
            let pending = self.store.pending_len(shard).await?;
            let (count, trigger) = match decide_close(pending, &self.policy) {
                SealDecision::Wait => return Ok(()),
                SealDecision::SealCount(n) => (n, Trigger::Count),
                SealDecision::CloseFlush => (pending, Trigger::Close),
                // `decide_close` は返さない。
                SealDecision::SealAll => (pending, Trigger::Close),
            };
            events.push(self.seal(shard, count, trigger, guard).await?);
            self.shards[index].last_sealed_at = Some(self.wall.now_unix_secs());
        }
    }

    /// 全シャードの head。全シャードにチェーン（head）が揃っていなければ `None`。
    async fn current_heads(&self) -> Result<Option<Vec<ShardHead>>, SealerError> {
        let mut heads = Vec::with_capacity(self.shards.len());
        for shard in self.shards() {
            let Some(block) = self.store.head(shard).await? else {
                tracing::debug!(
                    shard = shard.0,
                    "head が無いシャードがあるので、アンカーを作りません"
                );
                return Ok(None);
            };
            heads.push(ShardHead {
                shard: shard.0,
                height: block.header.height,
                block_hash: block.block_hash,
            });
        }
        Ok(Some(heads))
    }

    /// 直前のアンカー以降にどのシャードの先頭ブロックも変わっていなければ何も作らず、変わっていれば、
    /// 全シャードの head をまとめたアンカーを作って保存する。
    ///
    /// 「データに更新がない場合は、ブロックチェーンに何も追加しない」ため、変化がないときは新しいアンカーを
    /// 作らない（DEBUG ログに `skip` と出す）。アンカーを作る間隔のタイマーは、呼び出し側（`AnchorSchedule::due`）が
    /// 期限のたびに進めるので、作らなくても次の期限は 1 間隔先になる。
    ///
    /// 作らなかったとき（全シャードにチェーン（head）が揃っていない、変化がない、同じ番号のアンカーを他の
    /// プロセスに先に書かれた）は `None`。直前のアンカーと矛盾する head（巻き戻し・分岐の疑い）はエラー。
    pub async fn anchor(&self) -> Result<Option<Anchor>, SealerError> {
        let Some(heads) = self.current_heads().await? else {
            return Ok(None);
        };
        let latest = self.store.latest_anchor().await?;
        match compare_heads(latest.as_ref(), &heads) {
            HeadsChange::Unchanged => {
                tracing::debug!(
                    last_seq = latest.as_ref().map(|a| a.seq),
                    "アンカーの作成を skip しました（直前のアンカー以降、どのシャードの先頭ブロックも変わっていません）"
                );
                Ok(None)
            }
            HeadsChange::Inconsistent(reason) => Err(SealerError::AnchorInconsistent(reason)),
            HeadsChange::Advanced => self.write_anchor(heads, latest).await,
        }
    }

    /// 最終アンカー。締切の手続き（フラッシュの後）と、停止時に呼ぶ。
    ///
    /// - 直前のアンカー以降に変化がなければ、「最後のアンカーが最新の状態を指している」ことを確認するだけで、
    ///   新しいアンカーは作らない（[`FinalAnchor::UpToDate`]）。
    /// - 変化があれば、最後のアンカーを最新の状態に追いつかせるために 1 つ作る（[`FinalAnchor::Created`]）。
    /// - 直前のアンカーと矛盾する head はエラー。
    pub async fn finalize_anchor(&self) -> Result<FinalAnchor, SealerError> {
        let Some(heads) = self.current_heads().await? else {
            return Ok(FinalAnchor::Unavailable);
        };
        let latest = self.store.latest_anchor().await?;
        match compare_heads(latest.as_ref(), &heads) {
            HeadsChange::Unchanged => {
                match &latest {
                    Some(anchor) => tracing::info!(
                        seq = anchor.seq,
                        "最終アンカー: 最後のアンカーが最新の状態を指しています（新しいアンカーは作りません）"
                    ),
                    None => tracing::info!(
                        "最終アンカー: どのシャードにもブロックが追加されていないので、アンカーはありません（作りません）"
                    ),
                }
                Ok(FinalAnchor::UpToDate)
            }
            HeadsChange::Inconsistent(reason) => Err(SealerError::AnchorInconsistent(reason)),
            HeadsChange::Advanced => Ok(self
                .write_anchor(heads, latest)
                .await?
                .map_or(FinalAnchor::Unavailable, FinalAnchor::Created)),
        }
    }

    /// `heads` を、直前のアンカー `latest` の次のアンカーとして保存する。同じ番号のアンカーを他のプロセスに
    /// 先に書かれたときは `None`。
    async fn write_anchor(
        &self,
        heads: Vec<ShardHead>,
        latest: Option<Anchor>,
    ) -> Result<Option<Anchor>, SealerError> {
        let (seq, prev) = match latest {
            Some(latest) => (
                latest.seq.checked_add(1).ok_or(StoreError::Corrupt)?,
                latest.anchor_hash,
            ),
            None => (1, GENESIS_ANCHOR_PREV),
        };
        let minute = unix_minutes(self.wall.now_unix_secs());
        let anchor = build_anchor(seq, minute, prev, heads, &*self.signer)?;
        if !self.store.append_anchor(&anchor).await? {
            tracing::debug!(seq, "同じ番号のアンカーを他のプロセスが先に作りました");
            return Ok(None);
        }
        let summary: Vec<String> = anchor
            .heads
            .iter()
            .map(|h| {
                format!(
                    "{}@{}:{}",
                    h.shard,
                    h.height,
                    hex::encode(&h.block_hash[..8])
                )
            })
            .collect();
        tracing::info!(
            seq = anchor.seq,
            anchor_hash = %hex::encode(&anchor.anchor_hash),
            heads = %summary.join(","),
            "アンカーを作成しました"
        );
        Ok(Some(anchor))
    }

    /// プール先頭（到着順）の `n` 件を 1 ブロックに封印し、プールから削除する。
    async fn seal(
        &self,
        shard: ShardId,
        n: usize,
        trigger: Trigger,
        guard: LeaseGuard<'_>,
    ) -> Result<SealEvent, SealerError> {
        // 1 回の封印（取り出し・ブロック作成・書き込み）に要した実時間。性能計測の根拠にする。
        let started = std::time::Instant::now();
        let ballots = self.store.peek_pending(shard, n).await?;
        let count = ballots.len();
        let head = self
            .store
            .head(shard)
            .await?
            .ok_or(SealerError::NotInitialized(shard.0))?;
        let minute = unix_minutes(self.wall.now_unix_secs());
        // `seal_block` が票を ballot_id のハッシュ順に並べる。
        let block = seal_block(&head, ballots, minute, &*self.signer)?;
        let height = block.header.height;
        // 書き込む直前にリースを確認する。失っていれば、何も書かずに中止する（票はプールに残る）。
        if !guard() {
            return Err(SealerError::LeaseLost(shard.0));
        }
        // ブロックの追加とプールからの削除は不可分。
        self.store.commit(shard, block, count).await?;
        tracing::info!(
            shard = shard.0,
            height,
            count,
            trigger = %trigger,
            took_ms = started.elapsed().as_millis() as u64,
            "ブロックを封印しました"
        );
        Ok(SealEvent {
            shard,
            height,
            count,
            trigger,
        })
    }
}
