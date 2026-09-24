//! Coordinator（リース・担当シャード・アンカー）のテスト。時計は `ManualClock` で進めるので決定的。
//! リースは、同じ時計で期限を判定する偽のストアを使う。

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use application::{
    ChainRead, Clock, ElectionStateStore, LeaseStore, SealStore, StoreError, VoteStore,
};
use async_trait::async_trait;
use domain::seal_policy::SealPolicy;
use domain::{
    Ballot, BallotId, Block, CandidateId, ContestId, Ed25519Signer, ElectionPhase, Period, ShardId,
    VoterId, verify_anchor, verify_anchor_link, verify_chain,
};
use infra_memory::InMemoryStore;
use sealer::{
    ANCHOR_LEASE, Coordinator, LeaseConfig, ManualClock, MonotonicClock, Sealer, SealerError,
    StepOutcome, Trigger,
};

/// 選挙状態の判定に使う、明示的に進める壁時計。
struct ManualWall(std::sync::atomic::AtomicU64);

impl ManualWall {
    fn new(now: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(now))
    }

    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for ManualWall {
    fn now_unix_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// 同じ `ManualClock` で期限を判定する偽のリースストア。
struct FakeLeases {
    clock: Arc<ManualClock>,
    leases: Mutex<HashMap<String, (String, Duration)>>,
    /// true なら、更新（renew）がストアの障害で失敗する。
    fail_renew: AtomicBool,
}

impl FakeLeases {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            leases: Mutex::new(HashMap::new()),
            fail_renew: AtomicBool::new(false),
        }
    }

    /// 期限を待たずに、別の owner がリースを奪った状況を作る。
    fn steal(&self, name: &str, owner: &str, ttl: Duration) {
        let expires = self.clock.elapsed() + ttl;
        self.leases
            .lock()
            .expect("test lock")
            .insert(name.to_string(), (owner.to_string(), expires));
    }

    fn owner_of(&self, name: &str) -> Option<String> {
        let now = self.clock.elapsed();
        self.leases
            .lock()
            .expect("test lock")
            .get(name)
            .filter(|(_, expires)| *expires > now)
            .map(|(owner, _)| owner.clone())
    }
}

#[async_trait]
impl LeaseStore for FakeLeases {
    async fn try_acquire(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        let now = self.clock.elapsed();
        let mut leases = self.leases.lock().expect("test lock");
        match leases.get(name) {
            Some((_, expires)) if *expires > now => Ok(false),
            _ => {
                leases.insert(name.to_string(), (owner.to_string(), now + ttl));
                Ok(true)
            }
        }
    }

    async fn renew(&self, name: &str, owner: &str, ttl: Duration) -> Result<bool, StoreError> {
        if self.fail_renew.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        let now = self.clock.elapsed();
        let mut leases = self.leases.lock().expect("test lock");
        match leases.get_mut(name) {
            Some((current, expires)) if current == owner && *expires > now => {
                *expires = now + ttl;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError> {
        let mut leases = self.leases.lock().expect("test lock");
        if leases
            .get(name)
            .is_some_and(|(current, _)| current == owner)
        {
            leases.remove(name);
        }
        Ok(())
    }
}

const TTL: Duration = Duration::from_secs(6); // 更新は 2 秒ごと、保持してよい期限は取得から 4 秒
const S: fn(u64) -> Duration = Duration::from_secs;

struct World {
    store: Arc<InMemoryStore>,
    clock: Arc<ManualClock>,
    wall: Arc<ManualWall>,
    leases: Arc<FakeLeases>,
    signer: Arc<Ed25519Signer>,
    shards: u16,
}

fn world(shards: u16) -> World {
    let clock = Arc::new(ManualClock::new());
    World {
        store: Arc::new(InMemoryStore::new(
            NonZeroU16::new(shards).expect("non-zero"),
        )),
        leases: Arc::new(FakeLeases::new(clock.clone())),
        clock,
        wall: Arc::new(ManualWall::new(1_800_000_000)),
        signer: Arc::new(Ed25519Signer::from_seed(&[9u8; 32])),
        shards,
    }
}

impl World {
    fn sealer(&self) -> Sealer {
        Sealer::new(
            self.store.clone(),
            self.signer.clone(),
            self.wall.clone(),
            self.clock.clone(),
            SealPolicy::new(100, 10).expect("valid policy"),
            NonZeroU16::new(self.shards).expect("non-zero"),
        )
    }

    fn coordinator(&self, id: &str, anchor_interval: Duration) -> Coordinator {
        self.coordinator_with_grace(id, anchor_interval, Duration::from_secs(1))
    }

    fn coordinator_with_grace(
        &self,
        id: &str,
        anchor_interval: Duration,
        election_grace: Duration,
    ) -> Coordinator {
        Coordinator::new(
            self.sealer(),
            self.leases.clone(),
            LeaseConfig {
                owner: id.to_string(),
                ttl: TTL,
            },
            anchor_interval,
            // 既存のテストは `ensure_initialized` していないので `Unavailable`（election_duty /
            // flush_if_closing は no-op）。原則17 のテストは、別途 `store.ensure_initialized` を呼ぶ。
            self.store.clone(),
            election_grace,
        )
    }

    fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    async fn cast(&self, shard: u16, first: u32, n: u32) {
        for i in first..first + n {
            let mut id = [0u8; 16];
            id[..4].copy_from_slice(&i.to_be_bytes());
            let ballot = Ballot {
                ballot_id: BallotId::from_random_bytes(id),
                contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
                candidate_id: CandidateId::parse(&format!("shugiin_smd.13.01.c{}", 1 + i % 4))
                    .expect("valid"),
            };
            let voter = VoterId::new(&format!("voter-{shard}-{i}")).expect("valid");
            self.store
                .cast(&voter, ShardId(shard), ballot)
                .await
                .expect("cast");
        }
    }

    async fn pending(&self, shard: u16) -> usize {
        self.store
            .pending_len(ShardId(shard))
            .await
            .expect("pending")
    }

    async fn head_height(&self, shard: u16) -> Option<u64> {
        self.store
            .head(ShardId(shard))
            .await
            .expect("head")
            .map(|b| b.header.height)
    }

    async fn chain(&self, shard: u16) -> Vec<Block> {
        let top = self.head_height(shard).await.expect("initialized");
        let mut blocks = Vec::new();
        for h in 0..=top {
            blocks.push(
                self.store
                    .block(ShardId(shard), h)
                    .await
                    .expect("block")
                    .expect("exists"),
            );
        }
        blocks
    }
}

fn summary(out: &StepOutcome) -> Vec<(u16, u64, usize, Trigger)> {
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    out.events
        .iter()
        .map(|e| (e.shard.0, e.height, e.count, e.trigger))
        .collect()
}

/// 2 秒（更新間隔）ごとに全 coordinator を `step` しながら、`secs` 秒ぶん時計を進める。
/// 長い時間を 1 度に進めると、保持者自身のリースも期限切れになる（実運用では常に更新している）。
async fn drive(w: &World, coordinators: &mut [&mut Coordinator], secs: u64) -> Vec<StepOutcome> {
    let mut outcomes = Vec::new();
    for _ in 0..secs / 2 {
        for c in coordinators.iter_mut() {
            outcomes.push(c.step().await);
        }
        w.advance(S(2));
    }
    outcomes
}

// --- リースの取得 ---

#[tokio::test]
async fn acquires_one_shard_per_round_and_initializes_it() {
    let w = world(4);
    let mut a = w.coordinator("sealer-a", S(600));

    let out = a.step().await;
    assert_eq!(out.acquired, vec![0]);
    assert_eq!(a.held_shards(), vec![0]);
    assert!(a.holds_anchor_lease());
    // 取得したシャードだけがジェネシスで初期化される。
    assert_eq!(w.head_height(0).await, Some(0));
    assert_eq!(w.head_height(1).await, None);

    // 同じ周期（更新間隔 2 秒）の中では、新しく取らない。
    w.advance(S(1));
    assert!(a.step().await.acquired.is_empty());

    for (t, expected) in [(2, vec![1]), (4, vec![2]), (6, vec![3])] {
        w.advance(S(t) - w.clock.elapsed());
        assert_eq!(a.step().await.acquired, expected, "t={t}");
    }
    assert_eq!(a.held_shards(), vec![0, 1, 2, 3]);
    // 更新し続けているので、期限をはるかに超えても保持している。
    for _ in 0..20 {
        w.advance(S(2));
        assert!(a.step().await.lost.is_empty());
    }
    assert_eq!(a.held_shards(), vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn two_coordinators_split_the_shards() {
    let w = world(4);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(600)),
        w.coordinator("sealer-b", S(600)),
    );
    for _ in 0..2 {
        a.step().await;
        b.step().await;
        w.advance(S(2));
    }
    assert_eq!(a.held_shards(), vec![0, 2]);
    assert_eq!(b.held_shards(), vec![1, 3]);
    // アンカー担当は 1 つだけ。
    assert!(a.holds_anchor_lease() ^ b.holds_anchor_lease());
    for shard in 0..4 {
        assert!(w.leases.owner_of(&format!("shard-{shard}")).is_some());
    }
}

// --- 担当シャードだけを封印する ---

#[tokio::test]
async fn seals_only_the_shards_it_holds() {
    let w = world(2);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(600)),
        w.coordinator("sealer-b", S(600)),
    );
    a.step().await; // a: shard 0
    b.step().await; // b: shard 1
    assert_eq!((a.held_shards(), b.held_shards()), (vec![0], vec![1]));

    w.cast(0, 0, 100).await;
    w.cast(1, 1_000, 100).await;
    assert_eq!(summary(&a.step().await), vec![(0, 1, 100, Trigger::Count)]);
    assert_eq!(summary(&b.step().await), vec![(1, 1, 100, Trigger::Count)]);
    assert_eq!((w.pending(0).await, w.pending(1).await), (0, 0));
}

#[tokio::test]
async fn unheld_shards_are_never_sealed() {
    let w = world(2);
    let mut a = w.coordinator("sealer-a", S(600));
    a.step().await; // shard 0 だけ取得
    w.cast(1, 0, 100).await; // 担当していないシャードに 100 票
    w.advance(S(1));
    assert_eq!(summary(&a.step().await), vec![]);
    assert_eq!(w.pending(1).await, 100);
    assert_eq!(w.head_height(1).await, None);
}

// --- リースを失ったら即停止 ---

#[tokio::test]
async fn losing_the_lease_stops_the_shard_immediately() {
    let w = world(1);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(600)),
        w.coordinator("sealer-b", S(600)),
    );
    a.step().await;
    assert_eq!(a.held_shards(), vec![0]);

    // 100 票が溜まった（次の周期なら件数で封印される）状態で、別の sealer にリースを奪われる。
    w.cast(0, 0, 100).await;
    w.leases.steal("shard-0", "sealer-b", TTL);
    w.advance(S(2)); // 更新の時期
    let out = a.step().await;
    assert_eq!(out.lost, vec![0]);
    assert_eq!(
        summary(&out),
        vec![],
        "リースを失ったシャードは、同じ周期でも封印しない"
    );
    assert!(a.held_shards().is_empty());
    assert_eq!((w.head_height(0).await, w.pending(0).await), (Some(0), 100));

    // 以後も封印しない。新しい保持者（b）が、期限切れの後に取得して、すぐ封印する。
    w.advance(S(1));
    assert_eq!(summary(&a.step().await), vec![]);
    w.advance(S(10)); // steal から 6 秒（TTL）を過ぎる
    let out = b.step().await;
    assert_eq!(out.acquired, vec![0]);
    assert_eq!(summary(&out), vec![(0, 1, 100, Trigger::Count)]);
    assert_eq!(b.held_shards(), vec![0]);
}

#[tokio::test]
async fn a_lease_that_cannot_be_renewed_is_dropped_at_its_deadline_and_never_used_after() {
    let w = world(1);
    let mut a = w.coordinator("sealer-a", S(600));
    a.step().await; // 取得（保持してよい期限は 4 秒まで）
    w.cast(0, 0, 100).await;

    // ストアの障害で更新できない。期限（4 秒）までは保持しているとみなすが、期限を過ぎたら止める。
    w.leases.fail_renew.store(true, Ordering::SeqCst);
    w.advance(S(2));
    let out = a.step().await; // 更新に失敗（警告）。期限内なので、封印は行われる。
    assert!(!out.errors.is_empty());
    assert_eq!(a.held_shards(), vec![0]);
    assert_eq!(
        out.events.len(),
        1,
        "期限内はリースを持っているものとして封印する"
    );

    w.cast(0, 1_000, 100).await;
    w.advance(S(2)); // t=4: 期限
    let out = a.step().await;
    assert_eq!(out.lost, vec![0]);
    assert!(out.events.is_empty(), "期限を過ぎたら、封印せずに手放す");
    assert_eq!(w.pending(0).await, 100);
}

#[tokio::test]
async fn the_lease_is_checked_right_before_every_commit() {
    let w = world(1);
    let mut sealer = w.sealer();
    sealer.init().await.expect("init");
    w.cast(0, 0, 100).await;

    // リースが無効だと、件数に達していても書き込まずに中止する（票はプールに残る）。
    let mut events = Vec::new();
    let err = sealer
        .tick_shard(ShardId(0), &|| false, &mut events)
        .await
        .expect_err("guard is false");
    assert_eq!(err, SealerError::LeaseLost(0));
    assert!(events.is_empty());
    assert_eq!((w.head_height(0).await, w.pending(0).await), (Some(0), 100));

    // 有効なら封印される。
    sealer
        .tick_shard(ShardId(0), &|| true, &mut events)
        .await
        .expect("sealed");
    assert_eq!(events.len(), 1);
    assert_eq!(w.pending(0).await, 0);
}

// --- 引き継ぎ ---

#[tokio::test]
async fn another_sealer_takes_over_after_expiry_and_the_chain_stays_linear() {
    let w = world(2);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(600)),
        w.coordinator("sealer-b", S(600)),
    );
    // a が 2 シャードとも取得する（b はまだ起動していない）。
    for _ in 0..2 {
        a.step().await;
        w.advance(S(2));
    }
    assert_eq!(a.held_shards(), vec![0, 1]);

    w.cast(0, 0, 100).await;
    w.cast(1, 1_000, 100).await;
    assert_eq!(a.step().await.events.len(), 2);
    // 溜まっている途中（未封印 40 票ずつ）で、a が突然死ぬ（以後 step しない・解放もしない）。
    w.cast(0, 2_000, 40).await;
    w.cast(1, 3_000, 40).await;

    // 期限（最後の更新 t=4 から 6 秒 = t=10）が切れるまで、b は取得できない。
    w.advance(S(1)); // t=5
    assert!(b.step().await.acquired.is_empty());
    w.advance(S(1)); // t=6（以降、b は 2 秒ごとに step する）
    let mut all = Vec::new();
    let outcomes = drive(&w, &mut [&mut b], 24).await;
    for out in &outcomes {
        all.extend(summary(out));
    }
    assert_eq!(
        b.held_shards(),
        vec![0, 1],
        "期限切れの後、b が両方のシャードを引き継ぐ"
    );
    // 引き継いだ（t=10 と t=12）後、窓（10 秒）が満了して、未封印の 40 票が封印される。
    assert_eq!(
        all,
        vec![(0, 2, 40, Trigger::Time), (1, 2, 40, Trigger::Time)]
    );

    // 分岐なし: チェーンは 1 本で、票は 1 回ずつしか封印されていない。
    let verifier = w.signer.verifier();
    for shard in 0..2 {
        let blocks = w.chain(shard).await;
        assert_eq!(verify_chain(&blocks, &verifier), Ok(()));
        let total: usize = blocks.iter().map(|b| b.ballots.len()).sum();
        assert_eq!(total, 140);
        let mut ids: Vec<_> = blocks
            .iter()
            .flat_map(|b| b.ballots.iter().map(|x| x.ballot_id))
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 140, "同じ票が 2 回封印されていない");
    }
}

// --- 正常停止 ---

#[tokio::test]
async fn graceful_shutdown_flushes_and_releases_the_leases() {
    let w = world(2);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(600)),
        w.coordinator("sealer-b", S(600)),
    );
    a.step().await; // shard 0 とアンカー担当
    w.cast(0, 0, 37).await; // 窓は満了しておらず、件数にも達していない

    let out = a.shutdown().await;
    assert_eq!(summary(&out), vec![(0, 1, 37, Trigger::Flush)]);
    assert!(a.held_shards().is_empty() && !a.holds_anchor_lease());
    assert_eq!(w.pending(0).await, 0);

    // 解放されたので、別の sealer が期限を待たずにすぐ取得できる。
    let out = b.step().await;
    assert_eq!(out.acquired, vec![0]);
    assert!(b.holds_anchor_lease());
}

// --- アンカー ---
//
// 「データに更新がない場合は、ブロックチェーンに何も追加しない」: アンカーは、直前のアンカー以降に、
// いずれかのシャードの先頭ブロックが変わったときだけ作る（変わっていなければ、タイマーだけ進めて skip）。

#[tokio::test]
async fn the_anchor_holder_creates_a_linked_signed_anchor_only_when_heads_change() {
    let w = world(2);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(10)),
        w.coordinator("sealer-b", S(10)),
    );
    let mut anchors = Vec::new();
    let mut b_anchors = 0;
    // `secs` 秒ぶん、2 秒刻みで a・b を動かして、作られたアンカーを集める。
    macro_rules! run {
        ($secs:expr) => {
            for _ in 0..$secs / 2 {
                anchors.extend(a.step().await.anchors);
                b_anchors += b.step().await.anchors.len();
                w.advance(S(2));
            }
        };
    }

    // 票が 1 件も無い間は、間隔（10 秒）が何度来ても、アンカーは作られない（ジェネシスだけの状態）。
    run!(50);
    assert!(a.holds_anchor_lease());
    assert!(anchors.is_empty());
    assert_eq!(w.store.latest_anchor().await, Ok(None));

    // shard 0 に票が届き、窓の満了で封印されると、その次の間隔でアンカーが 1 つ作られる。
    w.cast(0, 0, 5).await;
    run!(24);
    assert_eq!(anchors.len(), 1);
    // 変化がない間は、何度間隔が来ても増えない。
    run!(50);
    assert_eq!(anchors.len(), 1);
    // shard 1 に票が届く → 2 つ目。
    w.cast(1, 0, 3).await;
    run!(24);
    assert_eq!(anchors.len(), 2);
    run!(30);
    assert_eq!(anchors.len(), 2);
    // shard 0 にもう一度 → 3 つ目。
    w.cast(0, 5, 2).await;
    run!(24);
    assert_eq!(anchors.len(), 3);
    run!(30);
    assert_eq!(anchors.len(), 3);

    assert_eq!(b_anchors, 0, "アンカー担当（a）以外は作らない");
    let verifier = w.signer.verifier();
    for anchor in &anchors {
        assert_eq!(verify_anchor(anchor, &verifier), Ok(()));
        let shards: Vec<u16> = anchor.heads.iter().map(|h| h.shard).collect();
        assert_eq!(shards, vec![0, 1], "全シャードの head を含む");
    }
    let heights: Vec<Vec<u64>> = anchors
        .iter()
        .map(|a| a.heads.iter().map(|h| h.height).collect())
        .collect();
    assert_eq!(heights, vec![vec![1, 0], vec![1, 1], vec![2, 1]]);
    assert_eq!(anchors[0].seq, 1);
    assert_eq!(verify_anchor_link(&anchors[0], &anchors[1]), Ok(()));
    assert_eq!(verify_anchor_link(&anchors[1], &anchors[2]), Ok(()));
    // 保存されており、head は実際のチェーンと一致する。
    let latest = w
        .store
        .latest_anchor()
        .await
        .expect("latest")
        .expect("some");
    assert_eq!(latest, anchors[2]);
    for head in &latest.heads {
        let block = w
            .store
            .head(ShardId(head.shard))
            .await
            .expect("head")
            .expect("some");
        assert_eq!(
            (block.header.height, block.block_hash),
            (head.height, head.block_hash)
        );
    }
}

#[tokio::test]
async fn nothing_is_added_when_there_is_no_data_for_a_long_time() {
    // 票が 0 件のまま、窓の満了と、アンカーの間隔が何度来ても、ブロックもアンカーも増えない。
    // 停止（0 件の締切フラッシュ + 最終アンカーの確認）でも、増えない。
    let w = world(2);
    let mut a = w.coordinator("sealer-a", S(10));
    for out in drive(&w, &mut [&mut a], 600).await {
        assert!(out.events.is_empty() && out.anchors.is_empty());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
    }
    for shard in 0..2 {
        assert_eq!(w.head_height(shard).await, Some(0), "ジェネシスだけ");
    }
    assert_eq!(w.store.latest_anchor().await, Ok(None));

    let out = a.shutdown().await;
    assert!(
        out.events.is_empty(),
        "0 件のフラッシュはブロックを作らない"
    );
    assert!(
        out.anchors.is_empty(),
        "最終アンカーも、変化がなければ作らない"
    );
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    for shard in 0..2 {
        assert_eq!(w.head_height(shard).await, Some(0));
    }
    assert_eq!(w.store.latest_anchor().await, Ok(None));
}

#[tokio::test]
async fn an_anchor_is_judged_once_per_interval_and_only_created_when_something_changed() {
    // アンカーの間隔は 15 秒（封印の窓の 10 秒とずらす）。間隔の期限が来たときだけ、変化を判定する。
    // 変化がなければ skip（タイマーだけ進む）、変化があれば、その期限の周期で作る。
    // 期限は、`AnchorSchedule`（担当になった周期を起点に 15 秒ごと）を写して数える。
    let w = world(1);
    let mut a = w.coordinator("sealer-a", S(15));
    let mut mirror = sealer::AnchorSchedule::new(S(15));
    let mut changed = false; // 直前のアンカー以降に、ブロックが追加された
    let (mut created, mut skipped) = (0, 0);
    let mut t = 0u64;
    for round in 0..2 {
        w.cast(0, round * 10, 3).await;
        for _ in 0..40 {
            let out = a.step().await;
            if t == 0 {
                mirror.restart(S(0)); // 担当になった周期が、タイマーの起点
            }
            let due = mirror.due(S(t));
            changed |= !out.events.is_empty();
            match (due, changed) {
                (true, true) => {
                    assert_eq!(out.anchors.len(), 1, "t={t}: 期限で、変化があるので作る");
                    changed = false;
                    created += 1;
                }
                (true, false) => {
                    assert!(out.anchors.is_empty(), "t={t}: 変化がないので作らない");
                    skipped += 1;
                }
                (false, _) => {
                    assert!(out.anchors.is_empty(), "t={t}: 期限でない周期には作らない");
                }
            }
            w.advance(S(2));
            t += 2;
        }
    }
    assert_eq!(created, 2, "票を投じた 2 回だけ");
    assert!(skipped >= 5, "変化のない期限は skip される（{skipped} 回）");
}

#[tokio::test]
async fn no_anchor_until_every_shard_has_a_chain() {
    let w = world(2);
    let mut a = w.coordinator("sealer-a", S(1));
    // shard 0 だけ先に、件数（100 件）で封印される。
    w.cast(0, 0, 100).await;
    // shard 1 は、次の周期（2 秒後）まで取得されない。その前に間隔（1 秒）が来ても作らない。
    a.step().await;
    assert_eq!(w.head_height(0).await, Some(1));
    w.advance(S(1));
    let out = a.step().await;
    assert!(out.anchors.is_empty());
    assert_eq!(w.store.latest_anchor().await, Ok(None));

    w.advance(S(1));
    // shard 1 を取得（この周期の先頭で、全シャードにチェーンが揃い、間隔の期限も来る）。
    let out = a.step().await;
    assert_eq!(out.anchors.len(), 1);
    assert_eq!(out.anchors[0].heads.len(), 2);
    // その後は、変化がないので、間隔が来ても作らない。
    w.advance(S(2));
    assert!(a.step().await.anchors.is_empty());
    w.advance(S(2));
    assert!(a.step().await.anchors.is_empty());
}

#[tokio::test]
async fn the_anchor_chain_continues_when_the_holder_changes() {
    let w = world(1);
    let (mut a, mut b) = (
        w.coordinator("sealer-a", S(10)),
        w.coordinator("sealer-b", S(10)),
    );
    w.cast(0, 0, 3).await;
    let mut first = Vec::new();
    for out in drive(&w, &mut [&mut a], 12).await {
        first.extend(out.anchors);
    }
    assert_eq!(first.len(), 1);
    // 正常停止: 解放する。変化がないので、最終アンカーは作らない（最後のアンカーが最新の状態を指している）。
    let out = a.shutdown().await;
    assert!(out.anchors.is_empty());
    // 新しい票が届くと、担当を引き継いだ b が、続きのアンカーを作る。
    w.cast(0, 3, 2).await;
    let mut second = Vec::new();
    for out in drive(&w, &mut [&mut b], 24).await {
        second.extend(out.anchors);
    }
    assert!(b.holds_anchor_lease(), "アンカー担当を引き継ぐ");
    assert_eq!(second.len(), 1);
    assert_eq!(second.first().map(|a| a.seq), Some(2));
    assert_eq!(verify_anchor_link(&first[0], &second[0]), Ok(()));
}

#[tokio::test]
async fn shutdown_of_the_anchor_holder_finalizes_only_when_something_changed() {
    let w = world(1);
    let mut a = w.coordinator("sealer-a", S(600));
    // 間隔（600 秒）はまだ来ない。停止時のフラッシュで封印された票は、最終アンカーで指される。
    w.cast(0, 0, 4).await;
    drive(&w, &mut [&mut a], 4).await;
    assert_eq!(w.head_height(0).await, Some(0), "窓は満了していない");
    let out = a.shutdown().await;
    assert_eq!(summary(&out), vec![(0, 1, 4, Trigger::Flush)]);
    assert_eq!(out.anchors.len(), 1, "変化があったので、最終アンカーを作る");
    let latest = w
        .store
        .latest_anchor()
        .await
        .expect("latest")
        .expect("some");
    assert_eq!(latest, out.anchors[0]);
    assert_eq!((latest.seq, latest.heads[0].height), (1, 1));

    // もう一度担当になって、変化なしで停止しても、最終アンカーは作らない（最新の状態を指していることを確認するだけ）。
    let mut b = w.coordinator("sealer-b", S(600));
    drive(&w, &mut [&mut b], 4).await;
    let out = b.shutdown().await;
    assert!(out.events.is_empty() && out.anchors.is_empty());
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(
        w.store.latest_anchor().await.expect("latest"),
        Some(latest),
        "アンカーは増えていない"
    );
}

#[test]
fn the_anchor_lease_name_is_stable() {
    // 他のプロセス（別バージョン）と同じ名前でリースを取り合うため、名前を固定する。
    assert_eq!(ANCHOR_LEASE, "anchor");
}

// --- 選挙状態（原則17）: アンカー担当だけが遷移を駆動し、両方が自分のシャードを直ちにフラッシュする ---

#[tokio::test]
async fn election_lifecycle_is_driven_by_the_anchor_holder_and_flushed_by_both() {
    let w = world(2);
    let (mut a, mut b) = (
        w.coordinator_with_grace("sealer-a", S(600), S(2)),
        w.coordinator_with_grace("sealer-b", S(600), S(2)),
    );
    for _ in 0..2 {
        a.step().await;
        b.step().await;
        w.advance(S(2));
    }
    assert_eq!(a.held_shards(), vec![0]);
    assert_eq!(b.held_shards(), vec![1]);
    assert!(a.holds_anchor_lease() ^ b.holds_anchor_lease());
    let holder_is_a = a.holds_anchor_lease();

    let now = i64::try_from(w.wall.now_unix_secs()).expect("fits");
    w.store
        .ensure_initialized(Period {
            opens_at: Some(now),
            closes_at: Some(now + 5),
        })
        .await
        .expect("election state init");

    // 両方が続けて step する: アンカー担当だけが scheduled -> open を行う。
    a.step().await;
    b.step().await;
    assert_eq!(
        w.store.get().await.expect("state").phase,
        ElectionPhase::Open
    );

    // 締切に進め、両方のシャードに票を用意しておく。
    w.wall.set(u64::try_from(now + 5).expect("fits"));
    w.cast(0, 0, 3).await;
    w.cast(1, 0, 2).await;

    // アンカー担当が open -> closing を検知する（この step 自身では、まだ自分のシャードを
    // フラッシュしない: flush_if_closing は election_duty より先に実行され、その時点ではまだ open）。
    let (holder, other) = if holder_is_a {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };
    holder.step().await;
    assert_eq!(
        w.store.get().await.expect("state").phase,
        ElectionPhase::Closing
    );

    // アンカー担当でなくても、closing を検知したら直ちに自分のシャードをフラッシュする。
    other.step().await;
    let other_shard = other.held_shards()[0];
    assert_eq!(w.pending(other_shard).await, 0);

    // アンカー担当自身も、次の step で自分のシャードをフラッシュする。
    holder.step().await;
    let holder_shard = holder.held_shards()[0];
    assert_eq!(w.pending(holder_shard).await, 0);
    // 猶予（2 秒）が経つまでは closed にならない。
    assert_eq!(
        w.store.get().await.expect("state").phase,
        ElectionPhase::Closing
    );

    w.advance(S(1));
    holder.step().await;
    assert_eq!(
        w.store.get().await.expect("state").phase,
        ElectionPhase::Closing,
        "猶予が経つ前に closed になってはいけません"
    );

    w.advance(S(2));
    holder.step().await;
    assert_eq!(
        w.store.get().await.expect("state").phase,
        ElectionPhase::Closed
    );

    let audit = w.store.recent_audit(10).await.expect("audit");
    assert_eq!(audit.len(), 3, "{audit:?}");
    assert!(
        audit.iter().all(|e| e.actor.starts_with("sealer:")),
        "{audit:?}"
    );
}
