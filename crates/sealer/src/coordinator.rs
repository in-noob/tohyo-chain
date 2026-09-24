//! リースを取って担当シャードを決め、担当シャードだけを封印する（独立した sealer プロセス用）。
//!
//! - シャードごとに `shard-<n>`、アンカー作成の担当に `anchor` のリース（TTL 付き）を取る。
//! - リースは TTL の 1/3 ごとに更新する。更新できなければ**そのシャードの処理を直ちに止める**。
//! - 更新が通らない状態が続く場合に備え、有効期限を単調時計で持つ。ブロックを書く直前に必ず確認し、
//!   切れていれば書かない（ネットワーク分断や停止から復帰した古い sealer が書かないようにする）。
//! - 最後の砦: ブロックの追加は `INSERT IF NOT EXISTS`（shard, height が主キー）なので、同じ高さに
//!   2 つ目のブロックは置けない。競り負けたら `recover` して次の周期に持ち越す。
//! - 新規取得は 1 周期（TTL/3）に 1 シャードまで。複数の sealer が同時に起動しても、担当がおおむね分かれる。
//!   （負荷の再分散はしない。）

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use application::{ElectionStateSnapshot, ElectionStateStore, LeaseStore, StoreError};
use domain::{
    Anchor, ElectionPhase, ElectionRules, ShardId, automatic_transition, voting_started_at,
};

use crate::clock::MonotonicClock;
use crate::schedule::AnchorSchedule;
use crate::sealer::{FinalAnchor, SealEvent, Sealer, SealerError};

/// アンカー作成の担当を決めるリースの名前。
pub const ANCHOR_LEASE: &str = "anchor";

fn shard_lease(shard: u16) -> String {
    format!("shard-{shard}")
}

#[derive(Debug, Clone)]
pub struct LeaseConfig {
    /// このプロセスを識別する名前（リースの owner）。プロセスごとに一意にする。
    pub owner: String,
    pub ttl: Duration,
}

/// 保持しているリース 1 つ分の状態（時刻は単調時計の値）。
#[derive(Debug, Clone, Copy)]
struct Held {
    /// この時刻までは、リースを持っているとみなしてよい。
    valid_until: Duration,
    /// この時刻以降に更新する。
    next_renew: Duration,
}

/// `step` / `shutdown` の結果。
#[derive(Debug, Default)]
pub struct StepOutcome {
    pub events: Vec<SealEvent>,
    pub anchors: Vec<Anchor>,
    /// 今回取得したシャード。
    pub acquired: Vec<u16>,
    /// 今回失った（有効期限切れ・他に取られた）シャード。
    pub lost: Vec<u16>,
    pub errors: Vec<SealerError>,
}

pub struct Coordinator {
    sealer: Sealer,
    leases: Arc<dyn LeaseStore>,
    config: LeaseConfig,
    mono: Arc<dyn MonotonicClock>,
    shard_count: u16,
    held: BTreeMap<u16, Held>,
    anchor_lease: Option<Held>,
    next_acquire: Duration,
    anchors: AnchorSchedule,
    /// 選挙状態（原則17）。
    election: Arc<dyn ElectionStateStore>,
    /// 締切の手続きの待ち時間（`election.state_cache_secs + api.request_timeout_secs`）。
    election_grace: Duration,
    /// open に遷移させるときに固定する選挙のルール（設定 `vote.allow_blank`。原則19）。
    rules: ElectionRules,
    /// closing を検知してからの、締切の手続きの期限（単調時計）。アンカー担当のときだけ使う。
    closing_deadline: Option<Duration>,
}

impl Coordinator {
    /// `anchor_interval`: アンカーを作るかどうかを判定する間隔（`seal.interval_secs` に連動させる）。
    /// 判定のたびに、直前のアンカー以降に先頭ブロックが変わっていなければ、作らない。
    ///
    /// `election` / `election_grace`: アンカーのリースを持っている間だけ、選挙状態（scheduled → open →
    /// closing → closed）の自動遷移と締切の手続きを行う（原則17）。`election_grace` は、closing を検知
    /// してから、全シャードのフラッシュを確認し始めるまでの待ち時間。`rules` は、open に遷移させるときに
    /// 選挙状態へ固定する選挙のルール（原則19）。
    pub fn new(
        sealer: Sealer,
        leases: Arc<dyn LeaseStore>,
        config: LeaseConfig,
        anchor_interval: Duration,
        election: Arc<dyn ElectionStateStore>,
        election_grace: Duration,
        rules: ElectionRules,
    ) -> Self {
        let mono = sealer.clock();
        let shard_count = sealer.shard_count();
        Self {
            sealer,
            leases,
            config,
            next_acquire: mono.elapsed(),
            mono,
            shard_count,
            held: BTreeMap::new(),
            anchor_lease: None,
            anchors: AnchorSchedule::new(anchor_interval),
            election,
            election_grace,
            rules,
            closing_deadline: None,
        }
    }

    /// 今保持しているシャード（昇順）。
    pub fn held_shards(&self) -> Vec<u16> {
        self.held.keys().copied().collect()
    }

    pub fn holds_anchor_lease(&self) -> bool {
        self.anchor_lease.is_some()
    }

    fn renew_every(&self) -> Duration {
        self.config.ttl / 3
    }

    /// 取得・更新した時刻 `t0` から、保持してよい期限。サーバ側の期限は `t0 + ttl` 以降なので、
    /// TTL の 1/3 の余裕（時計の誤差・処理の遅れ）を見込んで、早めに手放す。
    fn held_from(&self, t0: Duration) -> Held {
        Held {
            valid_until: t0 + self.config.ttl - self.renew_every(),
            next_renew: t0 + self.renew_every(),
        }
    }

    /// 1 周期分の処理: 選挙状態の読み込み（投票開始時刻 = 経過時間の起点）→ リースの更新 → 新規取得
    /// （1 周期 1 シャードまで）→ 担当シャードの封印 → closing なら担当シャードを直ちにフラッシュ → アンカー →
    /// 選挙状態の遷移・締切の手続き。
    pub async fn step(&mut self) -> StepOutcome {
        let mut out = StepOutcome::default();
        let now = self.mono.elapsed();
        let snapshot = match self.election.get().await {
            Ok(snapshot) => Some(snapshot),
            // 未初期化（`ensure_initialized` が呼ばれていない）: この選挙は状態機械を使わない。
            Err(StoreError::Unavailable) => None,
            Err(e) => {
                tracing::warn!(error = %e, "選挙状態の取得に失敗しました");
                out.errors.push(e.into());
                None
            }
        };
        if let Some(snapshot) = &snapshot {
            self.sealer
                .set_voting_started_at(voting_started_at(snapshot.period, snapshot.opened_at));
        }
        self.renew_leases(now, &mut out).await;
        self.acquire_round(now, &mut out).await;
        self.seal_held(&mut out).await;
        if snapshot
            .as_ref()
            .is_some_and(|s| s.phase == ElectionPhase::Closing)
        {
            self.close_flush_held(&mut out).await;
        }
        self.anchor_duty(now, &mut out).await;
        if let Some(snapshot) = snapshot {
            self.election_duty(now, snapshot, &mut out).await;
        }
        out
    }

    async fn renew_leases(&mut self, now: Duration, out: &mut StepOutcome) {
        let owner = self.config.owner.clone();
        let ttl = self.config.ttl;

        for shard in self.held_shards() {
            let Some(held) = self.held.get(&shard).copied() else {
                continue;
            };
            if now >= held.valid_until {
                self.lose(shard, "有効期限が切れました", out);
                continue;
            }
            if now < held.next_renew {
                continue;
            }
            match self.leases.renew(&shard_lease(shard), &owner, ttl).await {
                Ok(true) => {
                    let renewed = self.held_from(now);
                    self.held.insert(shard, renewed);
                }
                Ok(false) => self.lose(shard, "他の sealer に取得されました", out),
                Err(e) => {
                    // 期限までは次の周期で再試行する。期限が切れれば上で手放す。
                    tracing::warn!(shard, error = %e, "リースの更新に失敗しました");
                    out.errors.push(e.into());
                }
            }
        }

        if let Some(held) = self.anchor_lease {
            if now >= held.valid_until {
                self.lose_anchor("有効期限が切れました");
            } else if now >= held.next_renew {
                match self.leases.renew(ANCHOR_LEASE, &owner, ttl).await {
                    Ok(true) => self.anchor_lease = Some(self.held_from(now)),
                    Ok(false) => self.lose_anchor("他の sealer に取得されました"),
                    Err(e) => {
                        tracing::warn!(error = %e, "アンカー担当のリースの更新に失敗しました");
                        out.errors.push(e.into());
                    }
                }
            }
        }
    }

    async fn acquire_round(&mut self, now: Duration, out: &mut StepOutcome) {
        if now < self.next_acquire {
            return;
        }
        self.next_acquire = now + self.renew_every();
        let owner = self.config.owner.clone();
        let ttl = self.config.ttl;

        // シャードのリース: 未保持のものを先頭から試し、1 つ取れたらこの周期は終わり。
        for shard in 0..self.shard_count {
            if self.held.contains_key(&shard) {
                continue;
            }
            match self
                .leases
                .try_acquire(&shard_lease(shard), &owner, ttl)
                .await
            {
                Ok(false) => continue,
                Ok(true) => {
                    // 取得直後に、復旧・ジェネシス作成・封印窓の初期化を行う。
                    match self.sealer.init_shard(ShardId(shard)).await {
                        Ok(()) => {
                            let held = self.held_from(now);
                            self.held.insert(shard, held);
                            out.acquired.push(shard);
                            tracing::info!(shard, owner = %owner, "リースを取得しました");
                        }
                        Err(e) => {
                            tracing::error!(shard, error = %e, "シャードの初期化に失敗したのでリースを返します");
                            out.errors.push(e);
                            if let Err(e) = self.leases.release(&shard_lease(shard), &owner).await {
                                tracing::warn!(shard, error = %e, "リースの解放に失敗しました");
                            }
                        }
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(shard, error = %e, "リースの取得に失敗しました");
                    out.errors.push(e.into());
                    break;
                }
            }
        }

        // アンカー担当のリース。
        if self.anchor_lease.is_none() {
            match self.leases.try_acquire(ANCHOR_LEASE, &owner, ttl).await {
                Ok(true) => {
                    self.anchor_lease = Some(self.held_from(now));
                    self.anchors.restart(now);
                    tracing::info!(owner = %owner, "アンカー担当のリースを取得しました");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "アンカー担当のリースの取得に失敗しました");
                    out.errors.push(e.into());
                }
            }
        }
    }

    async fn seal_held(&mut self, out: &mut StepOutcome) {
        for shard in self.held_shards() {
            let Some(held) = self.held.get(&shard).copied() else {
                continue;
            };
            // ブロックを書く直前に、有効期限を（その時点の時計で）確認する。
            let mono = self.mono.clone();
            let until = held.valid_until;
            let guard = move || mono.elapsed() < until;
            match self
                .sealer
                .tick_shard(ShardId(shard), &guard, &mut out.events)
                .await
            {
                Ok(()) => {}
                Err(SealerError::LeaseLost(_)) => {
                    self.lose(shard, "封印の直前に有効期限が切れました", out);
                }
                Err(SealerError::Store(StoreError::Conflict)) => {
                    // 同じ高さのブロックを他のプロセスに先に書かれた。復旧して次の周期に持ち越す。
                    tracing::warn!(shard, "封印が競合しました。復旧して再試行します");
                    if let Err(e) = self.sealer.init_shard(ShardId(shard)).await {
                        out.errors.push(e);
                    }
                }
                Err(e) => {
                    tracing::error!(shard, error = %e, "封印に失敗しました");
                    out.errors.push(e);
                }
            }
        }
    }

    async fn anchor_duty(&mut self, now: Duration, out: &mut StepOutcome) {
        if self.anchor_lease.is_none() || !self.anchors.due(now) {
            return;
        }
        match self.sealer.anchor().await {
            Ok(Some(anchor)) => out.anchors.push(anchor),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "アンカーの作成に失敗しました");
                out.errors.push(e);
            }
        }
    }

    /// 選挙状態が closing のときに呼ぶ: 担当（アンカー担当かどうかを問わない）シャードを直ちにフラッシュする
    /// （原則9・17: 投票終了の手続きの中でだけ、残っている票を、持っているシャードごとに封印する）。
    async fn close_flush_held(&mut self, out: &mut StepOutcome) {
        for shard in self.held_shards() {
            let Some(held) = self.held.get(&shard).copied() else {
                continue;
            };
            let mono = self.mono.clone();
            let until = held.valid_until;
            let guard = move || mono.elapsed() < until;
            match self
                .sealer
                .close_flush_shard(ShardId(shard), &guard, &mut out.events)
                .await
            {
                Ok(()) => {}
                Err(SealerError::LeaseLost(_)) => {
                    self.lose(shard, "締切のフラッシュの直前に有効期限が切れました", out);
                }
                Err(e) => {
                    tracing::error!(shard, error = %e, "締切のフラッシュに失敗しました");
                    out.errors.push(e);
                }
            }
        }
    }

    /// アンカーのリースを持っている間だけ: 選挙状態の自動遷移（scheduled→open、open→closing）と、
    /// closing→closed の締切の手続き（原則17）。
    async fn election_duty(
        &mut self,
        now: Duration,
        snapshot: ElectionStateSnapshot,
        out: &mut StepOutcome,
    ) {
        if self.anchor_lease.is_none() {
            return;
        }
        let wall_now = i64::try_from(self.sealer.wall_now_unix_secs()).unwrap_or(i64::MAX);
        let actor = format!("sealer:{}", self.config.owner);

        match snapshot.phase {
            ElectionPhase::Scheduled | ElectionPhase::Open => {
                self.closing_deadline = None;
                let Some(next) = automatic_transition(snapshot.phase, snapshot.period, wall_now)
                else {
                    return;
                };
                match self
                    .election
                    .transition(snapshot.phase, next, self.rules, &actor, wall_now)
                    .await
                {
                    Ok(true) => {
                        tracing::info!(from = %snapshot.phase, to = %next, "選挙状態を自動で遷移しました");
                        if next == ElectionPhase::Closing {
                            self.closing_deadline = Some(now + self.election_grace);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "選挙状態の遷移に失敗しました");
                        out.errors.push(e.into());
                    }
                }
            }
            ElectionPhase::Closing => {
                let deadline = *self
                    .closing_deadline
                    .get_or_insert_with(|| now + self.election_grace);
                if now < deadline {
                    return;
                }
                match self.sealer.total_pending().await {
                    Ok(0) => {
                        match self.sealer.finalize_anchor().await {
                            Ok(FinalAnchor::Created(anchor)) => out.anchors.push(anchor),
                            Ok(FinalAnchor::UpToDate | FinalAnchor::Unavailable) => {}
                            Err(e) => {
                                tracing::error!(error = %e, "最終アンカーの作成に失敗しました");
                                out.errors.push(e);
                                return;
                            }
                        }
                        match self
                            .election
                            .transition(
                                ElectionPhase::Closing,
                                ElectionPhase::Closed,
                                self.rules,
                                &actor,
                                wall_now,
                            )
                            .await
                        {
                            Ok(true) => {
                                tracing::info!(
                                    "選挙状態を closed にしました（締切の手続きが完了しました）"
                                );
                                self.closing_deadline = None;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "選挙状態の closed への遷移に失敗しました");
                                out.errors.push(e.into());
                            }
                        }
                    }
                    Ok(pending) => {
                        tracing::debug!(pending, "締切の手続き: 未封印が残っているので待ちます");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "未封印の件数の確認に失敗しました");
                        out.errors.push(e);
                    }
                }
            }
            ElectionPhase::Closed => {}
        }
    }

    fn lose(&mut self, shard: u16, reason: &str, out: &mut StepOutcome) {
        if self.held.remove(&shard).is_some() {
            tracing::warn!(
                shard,
                reason,
                "リースを失いました。このシャードの処理を止めます"
            );
            out.lost.push(shard);
        }
    }

    fn lose_anchor(&mut self, reason: &str) {
        if self.anchor_lease.take().is_some() {
            tracing::warn!(reason, "アンカー担当のリースを失いました");
        }
    }

    /// 正常停止（SIGTERM）: 票のフラッシュはしない（原則9。未封印の票はストアに残り、引き継いだ sealer が
    /// 封印ルールに従って封印する）。アンカー担当なら最終アンカー（変化がなければ確認するだけ）を済ませて、
    /// リースを解放する（別の sealer がすぐ引き継げるように）。
    pub async fn shutdown(&mut self) -> StepOutcome {
        let mut out = StepOutcome::default();
        let owner = self.config.owner.clone();
        for shard in self.held_shards() {
            if let Err(e) = self.leases.release(&shard_lease(shard), &owner).await {
                tracing::warn!(shard, error = %e, "リースの解放に失敗しました");
            } else {
                tracing::info!(shard, "リースを解放しました");
            }
            self.held.remove(&shard);
        }
        // 最終アンカー: 停止時点の最新の状態を、最後のアンカーが指している状態にする。変化がなければ、
        // 確認するだけで、新しいアンカーは作らない。
        if self.anchor_lease.is_some() {
            match self.sealer.finalize_anchor().await {
                Ok(FinalAnchor::Created(anchor)) => out.anchors.push(anchor),
                Ok(FinalAnchor::UpToDate | FinalAnchor::Unavailable) => {}
                Err(e) => {
                    tracing::error!(error = %e, "最終アンカーの確認に失敗しました");
                    out.errors.push(e);
                }
            }
        }
        if self.anchor_lease.take().is_some()
            && let Err(e) = self.leases.release(ANCHOR_LEASE, &owner).await
        {
            tracing::warn!(error = %e, "アンカー担当のリースの解放に失敗しました");
        }
        out
    }
}
