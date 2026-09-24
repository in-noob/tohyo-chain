//! sealer の振る舞いテスト。時計は `ManualClock` で進めるので sleep を使わず決定的。
//!
//! 封印の経過時間は壁時計（UNIX 秒）で測る（ADR 0020）。`ManualClock::with_wall_base` を壁時計と単調時計の
//! 両方に渡すので、`advance` で両方が同じだけ進む。

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use application::{ChainRead, Clock, ElectionStateStore, SealStore, StoreError, VoteStore};
use async_trait::async_trait;
use domain::anchor::GENESIS_ANCHOR_PREV;
use domain::encoding::ballot_order_key;
use domain::seal_policy::SealPolicy;
use domain::{
    Ballot, BallotId, Block, CandidateId, ContestId, Ed25519Signer, ElectionPhase, ElectionRules,
    Period, ShardHead, ShardId, VoterId, build_anchor, verify_anchor, verify_anchor_link,
    verify_chain,
};
use infra_memory::InMemoryStore;
use sealer::{FinalAnchor, ManualClock, SealEvent, Sealer, SealerError, TickOutcome, Trigger};
use std::sync::Mutex;

/// テストの壁時計の起点（2027-01-15 相当。分単位に丸めると 30_000_000）。
const WALL_BASE: u64 = 1_800_000_000;

/// 固定の壁時計（`spy_sealer` 用）。
struct FixedWall;

impl Clock for FixedWall {
    fn now_unix_secs(&self) -> u64 {
        WALL_BASE
    }
}

struct Fixture {
    store: Arc<InMemoryStore>,
    clock: Arc<ManualClock>,
    signer: Arc<Ed25519Signer>,
    sealer: Sealer,
}

/// `max_ballots` 件で封印、`interval_secs` 秒以上経って `min_ballots` 件以上で全件封印。
fn fixture(shards: u16, max_ballots: usize, interval_secs: u64, min_ballots: usize) -> Fixture {
    let shard_count = NonZeroU16::new(shards).expect("non-zero");
    let store = Arc::new(InMemoryStore::new(shard_count));
    let clock = Arc::new(ManualClock::with_wall_base(WALL_BASE));
    let signer = Arc::new(Ed25519Signer::from_seed(&[9u8; 32]));
    let sealer = Sealer::new(
        store.clone(),
        signer.clone(),
        clock.clone(),
        clock.clone(),
        SealPolicy::new(max_ballots, interval_secs, min_ballots).expect("valid policy"),
        shard_count,
    );
    Fixture {
        store,
        clock,
        signer,
        sealer,
    }
}

/// `first_id` から `n` 票を、投票者ごとに別々に投じる（東京 1 区の小選挙区）。
async fn cast(store: &InMemoryStore, shard: u16, first_id: u32, n: u32) {
    for i in first_id..first_id + n {
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&i.to_be_bytes());
        let ballot = Ballot {
            ballot_id: BallotId::from_random_bytes(id),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
            candidate_id: CandidateId::parse(&format!("shugiin_smd.13.01.c{}", 1 + i % 4))
                .expect("valid"),
            revote: None,
        };
        let voter = VoterId::new(&format!("voter-{shard}-{i}")).expect("valid");
        store
            .cast(&voter, ShardId(shard), ballot)
            .await
            .expect("cast");
    }
}

fn summary(outcome: &TickOutcome) -> Vec<(u16, u64, usize, Trigger)> {
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome
        .events
        .iter()
        .map(|e: &SealEvent| (e.shard.0, e.height, e.count, e.trigger))
        .collect()
}

async fn pending(store: &InMemoryStore, shard: u16) -> usize {
    store.pending_len(ShardId(shard)).await.expect("pending")
}

async fn head_height(store: &InMemoryStore, shard: u16) -> u64 {
    store
        .head(ShardId(shard))
        .await
        .expect("head")
        .expect("initialized")
        .header
        .height
}

async fn chain(store: &InMemoryStore, shard: u16) -> Vec<Block> {
    let top = head_height(store, shard).await;
    let mut blocks = Vec::new();
    for h in 0..=top {
        blocks.push(
            store
                .block(ShardId(shard), h)
                .await
                .expect("block")
                .expect("exists"),
        );
    }
    blocks
}

const MS: fn(u64) -> Duration = Duration::from_millis;
const SECS: fn(u64) -> Duration = Duration::from_secs;

#[tokio::test]
async fn init_creates_one_genesis_per_shard_and_is_idempotent() {
    let mut f = fixture(3, 100, 10, 10);
    f.sealer.init().await.expect("init");
    f.sealer.init().await.expect("init again");
    for shard in 0..3 {
        let blocks = chain(&f.store, shard).await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].header.height, 0);
        assert!(blocks[0].ballots.is_empty());
        assert_eq!(blocks[0].header.sealed_at_minute, 30_000_000);
    }
}

#[tokio::test]
async fn waits_below_limit_and_before_expiry() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 99).await;
    f.clock.advance(MS(9_999));
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    assert_eq!(pending(&f.store, 0).await, 99);

    // 100 件目が届いた瞬間に、経過時間に関係なく件数で封印される。
    cast(&f.store, 0, 99, 1).await;
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 100, Trigger::Count)]
    );
}

#[tokio::test]
async fn two_hundred_fifty_ballots_seal_100_100_then_50_after_the_interval() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 250).await;

    // 1 回の tick で 100・100 と連続して封印され、残り 50 件は待つ。
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 100, Trigger::Count), (0, 2, 100, Trigger::Count)]
    );
    assert_eq!(pending(&f.store, 0).await, 50);
    assert_eq!(summary(&f.sealer.tick().await), vec![]);

    // 前回の封印（t=0 秒）から 10 秒未満は待ち、ちょうど 10 秒で全件封印。
    f.clock.advance(MS(9_900));
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    f.clock.advance(MS(100));
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 3, 50, Trigger::Time)]
    );
    assert_eq!(pending(&f.store, 0).await, 0);

    // その後は、25 秒経っても（0 件なので待つだけで）ブロックはできない。
    for _ in 0..25 {
        f.clock.advance(SECS(1));
        assert_eq!(summary(&f.sealer.tick().await), vec![]);
    }
    assert_eq!(head_height(&f.store, 0).await, 3);
}

#[tokio::test]
async fn below_the_minimum_waits_past_the_interval_and_the_minimum_th_ballot_seals_at_once() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 9).await;
    // 9 件のまま 20 秒（間隔の 2 倍）経っても封印しない。
    for _ in 0..20 {
        f.clock.advance(SECS(1));
        assert_eq!(summary(&f.sealer.tick().await), vec![]);
    }
    assert_eq!(pending(&f.store, 0).await, 9);
    // 10 件目が届くと、すぐに全 10 件を封印する（窓は、待っている間にリセットされていない）。
    cast(&f.store, 0, 9, 1).await;
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 10, Trigger::Time)]
    );
}

#[tokio::test]
async fn window_is_measured_from_the_last_seal() {
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");

    // t=5 秒で件数封印 → 経過時間の起点は 5 秒から。
    f.clock.advance(SECS(5));
    cast(&f.store, 0, 0, 100).await;
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 100, Trigger::Count)]
    );

    // 起点 0 なら t=10 で満了してしまうが、起点は 5 なので t=14 でも待つ。
    cast(&f.store, 0, 100, 1).await;
    f.clock.advance(SECS(5)); // t=10
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    f.clock.advance(SECS(4)); // t=14
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    f.clock.advance(SECS(1)); // t=15
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 2, 1, Trigger::Time)]
    );
}

#[tokio::test]
async fn zero_ballots_past_the_interval_make_no_block_and_do_not_reset_the_window() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");

    // 0 件のまま t=10: ブロックは作らない。窓もリセットしない（原則9）。
    f.clock.advance(SECS(10));
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    assert_eq!(head_height(&f.store, 0).await, 0);

    // 直後に 10 件届いたら、（窓の起点は t=0 のままなので）すぐに封印する。
    f.clock.advance(SECS(1));
    cast(&f.store, 0, 0, 10).await;
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 10, Trigger::Time)]
    );
}

#[tokio::test]
async fn the_voting_start_is_the_origin_of_the_elapsed_time() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    // 担当し始めて（t=0）から 100 秒後に、投票が始まった。
    f.clock.advance(SECS(100));
    f.sealer
        .set_voting_started_at(Some(i64::try_from(WALL_BASE + 100).expect("fits")));
    cast(&f.store, 0, 0, 10).await;
    // 担当し始めた時刻から測れば満了しているが、起点は投票開始（t=100）なので、t=109 までは待つ。
    f.clock.advance(SECS(9)); // t=109
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    f.clock.advance(SECS(1)); // t=110
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 10, Trigger::Time)]
    );
}

#[tokio::test]
async fn after_a_restart_the_last_seal_is_taken_from_the_head_block_never_earlier() {
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");
    // t=5 秒で 1 ブロック封印（分に丸めると 30_000_000 分 = WALL_BASE）。
    f.clock.advance(SECS(5));
    cast(&f.store, 0, 0, 100).await;
    f.sealer.tick().await;

    // 同じストアを、別の sealer（再起動・引き継ぎ）が担当する。前回の封印の正確な時刻は知らないので、
    // 先頭ブロックの分の最後の秒（WALL_BASE + 59）を前回の封印時刻とする。
    let mut next = Sealer::new(
        f.store.clone(),
        f.signer.clone(),
        f.clock.clone(),
        f.clock.clone(),
        SealPolicy::new(100, 10, 1).expect("valid policy"),
        NonZeroU16::new(1).expect("non-zero"),
    );
    next.set_voting_started_at(Some(i64::try_from(WALL_BASE).expect("fits")));
    next.init_shard(ShardId(0)).await.expect("init shard");
    cast(&f.store, 0, 100, 1).await;
    // 実際の封印（t=5）から 10 秒の t=15 では、まだ封印しない（早くは封印しない側に寄せる）。
    f.clock.advance(SECS(10)); // t=15
    assert_eq!(summary(&next.tick().await), vec![]);
    f.clock.advance(SECS(53)); // t=68 = 59 + 9
    assert_eq!(summary(&next.tick().await), vec![]);
    f.clock.advance(SECS(1)); // t=69 = 59 + 10
    assert_eq!(summary(&next.tick().await), vec![(0, 2, 1, Trigger::Time)]);
}

#[tokio::test]
async fn sealed_ballots_leave_the_pool_and_are_hash_ordered_and_chain_verifies() {
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 100).await;
    let arrived: Vec<Ballot> = f.store.ballots_in_shard(ShardId(0));
    f.sealer.tick().await;
    cast(&f.store, 0, 100, 7).await;
    f.clock.advance(SECS(10));
    f.sealer.tick().await;

    // 封印済みの票はプールから消えている。
    assert_eq!(pending(&f.store, 0).await, 0);

    let blocks = chain(&f.store, 0).await;
    assert_eq!(blocks.len(), 3);
    for block in &blocks[1..] {
        let keys: Vec<_> = block
            .ballots
            .iter()
            .map(|b| ballot_order_key(&b.ballot_id))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "ハッシュ昇順ではない");
    }
    // ブロックの票は到着した票と同じ集合（順序だけが異なる）。
    assert_ne!(blocks[1].ballots, arrived);
    let mut sealed = blocks[1].ballots.clone();
    let mut expected = arrived;
    let key = |b: &Ballot| b.ballot_id;
    sealed.sort_by_key(key);
    expected.sort_by_key(key);
    assert_eq!(sealed, expected);

    // チェーン全体の検証（署名・ハッシュ・Merkle・連続性）。
    assert_eq!(verify_chain(&blocks, &f.signer.verifier()), Ok(()));
}

#[tokio::test]
async fn shards_are_independent() {
    let mut f = fixture(2, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 1, 0, 100).await;
    cast(&f.store, 0, 1_000, 40).await;

    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(1, 1, 100, Trigger::Count)]
    );
    assert_eq!(head_height(&f.store, 0).await, 0);
    assert_eq!(pending(&f.store, 0).await, 40);

    // シャード 0 は自分の起点（t=0）から 10 秒で封印する。高さはシャードごとに数える。
    f.clock.advance(SECS(10));
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 40, Trigger::Time)]
    );
    assert_eq!(head_height(&f.store, 1).await, 1);
}

#[tokio::test]
async fn close_flush_seals_the_remainder_regardless_of_time_and_count() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 3).await;
    // 間隔も最小件数も満たしていないが、投票終了の手続きは全件を封印する。
    assert_eq!(
        summary(&f.sealer.close_flush().await),
        vec![(0, 1, 3, Trigger::Close)]
    );
    assert_eq!(pending(&f.store, 0).await, 0);

    // 0 件なら何もしない（空のブロックを作らない）。
    assert_eq!(summary(&f.sealer.close_flush().await), vec![]);
    assert_eq!(head_height(&f.store, 0).await, 1);
}

#[tokio::test]
async fn close_flush_keeps_blocks_within_the_limit() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 250).await;
    assert_eq!(
        summary(&f.sealer.close_flush().await),
        vec![
            (0, 1, 100, Trigger::Count),
            (0, 2, 100, Trigger::Count),
            (0, 3, 50, Trigger::Close)
        ]
    );
    assert_eq!(pending(&f.store, 0).await, 0);
}

#[tokio::test]
async fn failure_in_one_shard_does_not_block_others() {
    // init しないと head がなく、シャード 0 の封印は失敗する。シャード 1 はジェネシスを手で用意する。
    let mut f = fixture(2, 100, 10, 10);
    let genesis = domain::genesis(&*f.signer, 1);
    f.store
        .commit(ShardId(1), genesis, 0)
        .await
        .expect("genesis");
    cast(&f.store, 0, 0, 100).await;
    cast(&f.store, 1, 500, 100).await;

    let outcome = f.sealer.tick().await;
    assert_eq!(outcome.errors, vec![SealerError::NotInitialized(0)]);
    assert_eq!(
        outcome
            .events
            .iter()
            .map(|e| (e.shard.0, e.height, e.count))
            .collect::<Vec<_>>(),
        vec![(1, 1, 100)]
    );
    // 失敗したシャードの票は失われない。
    assert_eq!(pending(&f.store, 0).await, 100);
}

#[test]
fn trigger_labels_match_log_format() {
    assert_eq!(Trigger::Count.to_string(), "count");
    assert_eq!(Trigger::Time.to_string(), "time");
    assert_eq!(Trigger::Close.to_string(), "close");
}

#[tokio::test(start_paused = true)]
async fn spawned_task_seals_periodically_and_does_not_flush_on_shutdown() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    let (store, signer) = (f.store.clone(), f.signer.clone());
    let handle = sealer::spawn(
        f.sealer,
        sealer::DEFAULT_TICK,
        Duration::from_secs(600),
        f.store.clone(),
        Duration::from_secs(1),
        ElectionRules {
            allow_blank: true,
            ..ElectionRules::default()
        },
        Arc::new(application::RevoteKeyVault::none()),
    );

    // 100 件到着 → 次の周期（200ms 以内）で件数封印される。
    cast(&store, 0, 0, 100).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(head_height(&store, 0).await, 1);
    assert_eq!(pending(&store, 0).await, 0);

    // 30 件だけ届いた状態で停止（SIGTERM 相当）→ フラッシュしない（原則9）。票はストアに残る。
    cast(&store, 0, 100, 30).await;
    handle.shutdown().await.expect("shutdown");
    let blocks = chain(&store, 0).await;
    assert_eq!(blocks.len(), 2);
    assert_eq!(pending(&store, 0).await, 30);
    assert_eq!(verify_chain(&blocks, &signer.verifier()), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn spawned_task_flushes_only_in_the_closing_procedure() {
    let mut f = fixture(1, 100, 600, 10);
    f.sealer.init().await.expect("init");
    let store = f.store.clone();
    let clock = f.clock.clone();
    let opens_at = i64::try_from(WALL_BASE).expect("fits");
    store
        .ensure_initialized(Period {
            opens_at: Some(opens_at),
            closes_at: None,
        })
        .await
        .expect("init election");
    let handle = sealer::spawn(
        f.sealer,
        sealer::DEFAULT_TICK,
        Duration::from_secs(600),
        f.store.clone(),
        Duration::from_secs(1),
        ElectionRules {
            allow_blank: true,
            ..ElectionRules::default()
        },
        Arc::new(application::RevoteKeyVault::none()),
    );
    run_spawned_for(&clock, 1).await;
    assert_eq!(store.get().await.expect("state").phase, ElectionPhase::Open);

    // 5 票: 件数も時間も満たさないので、待つ。
    cast(&store, 0, 0, 5).await;
    run_spawned_for(&clock, 5).await;
    assert_eq!(head_height(&store, 0).await, 0);

    // close --now 相当（open → closing）: 締切の手続きの中で、5 件のブロック（trigger=close）ができ、closed になる。
    assert!(
        store
            .transition(
                ElectionPhase::Open,
                ElectionPhase::Closing,
                ElectionRules {
                    allow_blank: true,
                    ..ElectionRules::default()
                },
                "test",
                opens_at + 6
            )
            .await
            .expect("close")
    );
    run_spawned_for(&clock, 3).await;
    assert_eq!(head_height(&store, 0).await, 1);
    assert_eq!(chain(&store, 0).await[1].ballots.len(), 5);
    assert_eq!(pending(&store, 0).await, 0);
    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closed
    );
    handle.shutdown().await.expect("shutdown");
}

// --- 更新がなければ、ブロックチェーンに何も追加しない ---

#[tokio::test]
async fn waiting_with_no_ballots_never_adds_a_block_or_an_anchor() {
    // ブロック: 0 件のまま何度間隔が過ぎても、ブロックを作らない。
    // アンカー: ジェネシスだけの状態は「更新なし」なので、作らない。
    let mut f = fixture(2, 100, 10, 10);
    f.sealer.init().await.expect("init");
    for _ in 0..60 {
        f.clock.advance(SECS(10));
        assert_eq!(summary(&f.sealer.tick().await), vec![]);
        assert_eq!(f.sealer.anchor().await.expect("anchor"), None);
    }
    for shard in 0..2 {
        assert_eq!(head_height(&f.store, shard).await, 0);
    }
    assert_eq!(f.store.latest_anchor().await.expect("latest"), None);
}

#[tokio::test]
async fn close_flush_with_no_ballots_adds_nothing_even_after_earlier_activity() {
    // 投票終了の手続きで、0 件ならブロックを作らない。最終アンカーも、変化がなければ作らない。
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    // まず 0 件のフラッシュ。
    assert_eq!(summary(&f.sealer.close_flush().await), vec![]);
    assert_eq!(f.sealer.finalize_anchor().await, Ok(FinalAnchor::UpToDate));
    assert_eq!(head_height(&f.store, 0).await, 0);
    assert_eq!(f.store.latest_anchor().await.expect("latest"), None);

    // 票が届いてフラッシュされ、最終アンカーができたあとの、0 件のフラッシュ。
    cast(&f.store, 0, 0, 6).await;
    assert_eq!(
        summary(&f.sealer.close_flush().await),
        vec![(0, 1, 6, Trigger::Close)]
    );
    let Ok(FinalAnchor::Created(anchor)) = f.sealer.finalize_anchor().await else {
        panic!("変化があったので、最終アンカーが作られるはず");
    };
    assert_eq!(anchor.seq, 1);
    assert_eq!(summary(&f.sealer.close_flush().await), vec![]);
    assert_eq!(f.sealer.finalize_anchor().await, Ok(FinalAnchor::UpToDate));
    assert_eq!(head_height(&f.store, 0).await, 1);
    assert_eq!(
        f.store.latest_anchor().await.expect("latest"),
        Some(anchor),
        "アンカーは増えていない"
    );
}

#[tokio::test]
async fn an_anchor_is_created_only_when_a_head_changed_since_the_previous_anchor() {
    let mut f = fixture(2, 100, 10, 1);
    f.sealer.init().await.expect("init");
    let verifier = f.signer.verifier();
    let heights =
        |anchor: &domain::Anchor| -> Vec<u64> { anchor.heads.iter().map(|h| h.height).collect() };

    // ジェネシスだけ: 何度呼んでも作らない。
    for _ in 0..3 {
        assert_eq!(f.sealer.anchor().await.expect("anchor"), None);
    }
    assert_eq!(f.store.latest_anchor().await.expect("latest"), None);

    // shard 0 にブロックが 1 つできた → アンカー 1。
    cast(&f.store, 0, 0, 3).await;
    f.clock.advance(SECS(10));
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(0, 1, 3, Trigger::Time)]
    );
    let first = f.sealer.anchor().await.expect("anchor").expect("changed");
    assert_eq!((first.seq, heights(&first)), (1, vec![1, 0]));
    assert_eq!(verify_anchor(&first, &verifier), Ok(()));

    // 変化がない → 作らない（何度でも）。保存されているのは、アンカー 1 のまま。
    for _ in 0..3 {
        assert_eq!(f.sealer.anchor().await.expect("anchor"), None);
    }
    assert_eq!(
        f.store.latest_anchor().await.expect("latest"),
        Some(first.clone())
    );

    // 0 件のまま間隔が過ぎても（ブロックが増えないので）、作らない。
    f.clock.advance(SECS(10));
    assert_eq!(summary(&f.sealer.tick().await), vec![]);
    assert_eq!(f.sealer.anchor().await.expect("anchor"), None);

    // shard 1 にブロックができた → アンカー 2（アンカー 1 の続き）。
    cast(&f.store, 1, 0, 2).await;
    f.clock.advance(SECS(10));
    assert_eq!(
        summary(&f.sealer.tick().await),
        vec![(1, 1, 2, Trigger::Time)]
    );
    let second = f.sealer.anchor().await.expect("anchor").expect("changed");
    assert_eq!((second.seq, heights(&second)), (2, vec![1, 1]));
    assert_eq!(verify_anchor(&second, &verifier), Ok(()));
    assert_eq!(verify_anchor_link(&first, &second), Ok(()));
    assert_eq!(f.sealer.anchor().await.expect("anchor"), None);
}

#[tokio::test]
async fn finalize_anchor_only_confirms_when_the_last_anchor_is_current() {
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");

    cast(&f.store, 0, 0, 3).await;
    f.clock.advance(SECS(10));
    f.sealer.tick().await;
    let anchor = f.sealer.anchor().await.expect("anchor").expect("changed");

    // 最後のアンカーが最新の状態を指している: 確認するだけで、新しく作らない。
    assert_eq!(f.sealer.finalize_anchor().await, Ok(FinalAnchor::UpToDate));
    assert_eq!(f.sealer.finalize_anchor().await, Ok(FinalAnchor::UpToDate));
    assert_eq!(
        f.store.latest_anchor().await.expect("latest"),
        Some(anchor.clone())
    );

    // アンカーのあとにブロックが増えた: 最後のアンカーを追いつかせるために、1 つ作る。
    cast(&f.store, 0, 3, 2).await;
    f.sealer.close_flush().await;
    let Ok(FinalAnchor::Created(last)) = f.sealer.finalize_anchor().await else {
        panic!("変化があったので、最終アンカーが作られるはず");
    };
    assert_eq!((last.seq, last.heads[0].height), (2, 2));
    assert_eq!(verify_anchor_link(&anchor, &last), Ok(()));
    assert_eq!(f.sealer.finalize_anchor().await, Ok(FinalAnchor::UpToDate));
    // 最終アンカーのあとは、通常のアンカーも作らない。
    assert_eq!(f.sealer.anchor().await.expect("anchor"), None);
}

#[tokio::test]
async fn an_anchor_contradicting_the_chain_is_reported_and_never_overwritten() {
    // 直前のアンカーが指す高さより、実際のチェーンが低い（巻き戻し）: 新しいアンカーで上書きせず、エラーにする。
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");
    cast(&f.store, 0, 0, 3).await;
    f.clock.advance(SECS(10));
    f.sealer.tick().await;
    let forged = build_anchor(
        1,
        30_000_000,
        GENESIS_ANCHOR_PREV,
        vec![ShardHead {
            shard: 0,
            height: 5,
            block_hash: [1u8; 32],
        }],
        &*f.signer,
    )
    .expect("valid anchor");
    assert_eq!(f.store.append_anchor(&forged).await, Ok(true));

    assert!(matches!(
        f.sealer.anchor().await,
        Err(SealerError::AnchorInconsistent(_))
    ));
    assert!(matches!(
        f.sealer.finalize_anchor().await,
        Err(SealerError::AnchorInconsistent(_))
    ));
    assert_eq!(
        f.store.latest_anchor().await.expect("latest"),
        Some(forged),
        "矛盾するアンカーを、新しいアンカーで上書きしない"
    );
}

/// `spawn` した sealer を、`secs` 秒ぶん動かす（時計は手動なので、tick の周期ごとに進める）。
async fn run_spawned_for(clock: &ManualClock, secs: u64) {
    for _ in 0..secs * 5 {
        clock.advance(sealer::DEFAULT_TICK);
        tokio::time::sleep(sealer::DEFAULT_TICK).await;
    }
}

#[tokio::test(start_paused = true)]
async fn spawned_task_adds_nothing_while_idle_and_finalizes_only_on_change() {
    let mut f = fixture(1, 100, 10, 1);
    f.sealer.init().await.expect("init");
    let (store, clock) = (f.store.clone(), f.clock.clone());
    let handle = sealer::spawn(
        f.sealer,
        sealer::DEFAULT_TICK,
        Duration::from_secs(10),
        f.store.clone(),
        Duration::from_secs(1),
        ElectionRules {
            allow_blank: true,
            ..ElectionRules::default()
        },
        Arc::new(application::RevoteKeyVault::none()),
    );

    // 票が無いまま 60 秒（封印・アンカーの間隔が 6 回ずつ）: ブロックもアンカーも増えない。
    run_spawned_for(&clock, 60).await;
    assert_eq!(head_height(&store, 0).await, 0);
    assert_eq!(store.latest_anchor().await.expect("latest"), None);

    // 1 票（最小件数 1）→ ブロック 1 つ、アンカー 1 つ。
    cast(&store, 0, 0, 1).await;
    run_spawned_for(&clock, 25).await;
    assert_eq!(head_height(&store, 0).await, 1);
    let anchor = store.latest_anchor().await.expect("latest").expect("some");
    assert_eq!(anchor.seq, 1);

    // さらに 60 秒: 増えない。
    run_spawned_for(&clock, 60).await;
    assert_eq!(head_height(&store, 0).await, 1);
    assert_eq!(
        store.latest_anchor().await.expect("latest"),
        Some(anchor.clone())
    );

    // 変化なしで停止: 最終アンカーは、何も作らない。
    handle.shutdown().await.expect("shutdown");
    assert_eq!(head_height(&store, 0).await, 1);
    assert_eq!(store.latest_anchor().await.expect("latest"), Some(anchor));
}

#[tokio::test(start_paused = true)]
async fn spawned_task_finalizes_the_anchor_on_shutdown_without_flushing() {
    let mut f = fixture(1, 100, 10, 10);
    f.sealer.init().await.expect("init");
    let store = f.store.clone();
    // アンカーの間隔（600 秒）は来ない。停止時の最終アンカーが、件数で封印されたブロックを指す。
    // 残りの 4 票はフラッシュしない（原則9）。
    let handle = sealer::spawn(
        f.sealer,
        sealer::DEFAULT_TICK,
        Duration::from_secs(600),
        f.store.clone(),
        Duration::from_secs(1),
        ElectionRules {
            allow_blank: true,
            ..ElectionRules::default()
        },
        Arc::new(application::RevoteKeyVault::none()),
    );
    cast(&store, 0, 0, 104).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    handle.shutdown().await.expect("shutdown");
    assert_eq!(head_height(&store, 0).await, 1);
    assert_eq!(pending(&store, 0).await, 4);
    let anchor = store.latest_anchor().await.expect("latest").expect("some");
    assert_eq!((anchor.seq, anchor.heads[0].height), (1, 1));
}

// --- 起動時の復旧と、ジェネシス作成の競合 ---

/// `InMemoryStore` に委譲しつつ、`recover` の呼び出しを記録し、必要なら
/// 「別のプロセスが先にジェネシスを作っていた」状況を再現するストア。
struct SpyStore {
    inner: Arc<InMemoryStore>,
    recovered: Mutex<Vec<u16>>,
    /// true なら、ジェネシスの commit の直前に別のジェネシスを書いて `Conflict` を返す。
    lose_genesis_race: bool,
    signer: Arc<Ed25519Signer>,
}

#[async_trait]
impl ChainRead for SpyStore {
    async fn head(&self, shard: ShardId) -> Result<Option<Block>, StoreError> {
        self.inner.head(shard).await
    }
    async fn block(&self, shard: ShardId, height: u64) -> Result<Option<Block>, StoreError> {
        self.inner.block(shard, height).await
    }
    async fn latest_anchor(&self) -> Result<Option<domain::Anchor>, StoreError> {
        self.inner.latest_anchor().await
    }
    async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError> {
        self.inner.signer_public_key().await
    }
}

#[async_trait]
impl SealStore for SpyStore {
    async fn pending_len(&self, shard: ShardId) -> Result<usize, StoreError> {
        self.inner.pending_len(shard).await
    }
    async fn peek_pending(&self, shard: ShardId, n: usize) -> Result<Vec<Ballot>, StoreError> {
        self.inner.peek_pending(shard, n).await
    }
    async fn commit(
        &self,
        shard: ShardId,
        block: Block,
        consumed: usize,
    ) -> Result<(), StoreError> {
        if self.lose_genesis_race && block.header.height == 0 {
            // 別のプロセスの、分が異なるジェネシスが先に書かれていた。
            let other = domain::genesis(&*self.signer, 999);
            self.inner.commit(shard, other, 0).await?;
            return Err(StoreError::Conflict);
        }
        self.inner.commit(shard, block, consumed).await
    }
    async fn recover(&self, shard: ShardId) -> Result<(), StoreError> {
        self.recovered.lock().expect("test lock").push(shard.0);
        Ok(())
    }
    async fn register_signer(&self, public_key: [u8; 32]) -> Result<(), StoreError> {
        self.inner.register_signer(public_key).await
    }
    async fn append_anchor(&self, anchor: &domain::Anchor) -> Result<bool, StoreError> {
        self.inner.append_anchor(anchor).await
    }
}

fn spy_sealer(shards: u16, lose_genesis_race: bool) -> (Sealer, Arc<SpyStore>) {
    let shard_count = NonZeroU16::new(shards).expect("non-zero");
    let signer = Arc::new(Ed25519Signer::from_seed(&[9u8; 32]));
    let spy = Arc::new(SpyStore {
        inner: Arc::new(InMemoryStore::new(shard_count)),
        recovered: Mutex::new(Vec::new()),
        lose_genesis_race,
        signer: signer.clone(),
    });
    let sealer = Sealer::new(
        spy.clone(),
        signer,
        Arc::new(FixedWall),
        Arc::new(ManualClock::new()),
        SealPolicy::new(100, 10, 10).expect("valid policy"),
        shard_count,
    );
    (sealer, spy)
}

#[tokio::test]
async fn init_recovers_every_shard_before_touching_the_chain() {
    let (mut sealer, spy) = spy_sealer(3, false);
    sealer.init().await.expect("init");
    assert_eq!(*spy.recovered.lock().expect("test lock"), vec![0, 1, 2]);
    // 再起動（2 回目の init）でも復旧を呼び、ジェネシスは作り直さない。
    sealer.init().await.expect("init again");
    assert_eq!(spy.recovered.lock().expect("test lock").len(), 6);
    for shard in 0..3 {
        assert_eq!(head_height(&spy.inner, shard).await, 0);
        assert_eq!(chain(&spy.inner, shard).await.len(), 1);
    }
}

#[tokio::test]
async fn init_tolerates_losing_the_genesis_race_to_another_process() {
    let (mut sealer, spy) = spy_sealer(2, true);
    sealer
        .init()
        .await
        .expect("競り負けても、チェーンがあれば成功");
    for shard in 0..2 {
        // 勝った側のジェネシス（別の分）が採用されている。
        let blocks = chain(&spy.inner, shard).await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].header.sealed_at_minute, 999);
    }
}

#[tokio::test]
async fn init_fails_on_a_conflict_when_no_chain_exists() {
    // Conflict でチェーンも無い（本当の矛盾）なら、握りつぶさずにエラーにする。
    struct AlwaysConflict(SpyStore);
    #[async_trait]
    impl ChainRead for AlwaysConflict {
        async fn head(&self, shard: ShardId) -> Result<Option<Block>, StoreError> {
            self.0.head(shard).await
        }
        async fn block(&self, shard: ShardId, height: u64) -> Result<Option<Block>, StoreError> {
            self.0.block(shard, height).await
        }
        async fn latest_anchor(&self) -> Result<Option<domain::Anchor>, StoreError> {
            self.0.latest_anchor().await
        }
        async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError> {
            self.0.signer_public_key().await
        }
    }
    #[async_trait]
    impl SealStore for AlwaysConflict {
        async fn pending_len(&self, s: ShardId) -> Result<usize, StoreError> {
            self.0.pending_len(s).await
        }
        async fn peek_pending(&self, s: ShardId, n: usize) -> Result<Vec<Ballot>, StoreError> {
            self.0.peek_pending(s, n).await
        }
        async fn commit(&self, _: ShardId, _: Block, _: usize) -> Result<(), StoreError> {
            Err(StoreError::Conflict)
        }
        async fn register_signer(&self, k: [u8; 32]) -> Result<(), StoreError> {
            self.0.register_signer(k).await
        }
        async fn append_anchor(&self, a: &domain::Anchor) -> Result<bool, StoreError> {
            self.0.append_anchor(a).await
        }
    }
    let shard_count = NonZeroU16::MIN;
    let signer = Arc::new(Ed25519Signer::from_seed(&[9u8; 32]));
    let store = Arc::new(AlwaysConflict(SpyStore {
        inner: Arc::new(InMemoryStore::new(shard_count)),
        recovered: Mutex::new(Vec::new()),
        lose_genesis_race: false,
        signer: signer.clone(),
    }));
    let mut sealer = Sealer::new(
        store,
        signer,
        Arc::new(FixedWall),
        Arc::new(ManualClock::new()),
        SealPolicy::new(100, 10, 10).expect("valid policy"),
        shard_count,
    );
    assert_eq!(
        sealer.init().await,
        Err(SealerError::Store(StoreError::Conflict))
    );
}
