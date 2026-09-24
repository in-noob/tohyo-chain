//! memory モードの選挙状態スケジューラ（`sealer::spawn` に組み込まれた原則17の自動遷移）。
//!
//! 単調時計（`ManualClock`）は締切の手続きの待ち時間に、壁時計（`ManualWall`）は選挙状態の判定と封印の経過時間に使う。
//! 両方を明示的に進めることで、実時間の sleep に頼らず決定的にテストする。

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use application::{ChainRead, Clock, ElectionStateStore, SealStore};
use domain::seal_policy::SealPolicy;
use domain::{Ed25519Signer, ElectionPhase, Period};
use infra_memory::InMemoryStore;
use sealer::{ManualClock, Sealer};

struct ManualWall(AtomicU64);

impl ManualWall {
    fn new(now: u64) -> Self {
        Self(AtomicU64::new(now))
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

#[tokio::test(start_paused = true)]
async fn memory_scheduler_advances_through_the_full_lifecycle() {
    let shard_count = NonZeroU16::new(1).expect("non-zero");
    let store = Arc::new(InMemoryStore::new(shard_count));
    let mono = Arc::new(ManualClock::new());
    let wall = Arc::new(ManualWall::new(1_000));
    let signer = Arc::new(Ed25519Signer::from_seed(&[3u8; 32]));
    let mut sealer = Sealer::new(
        store.clone(),
        signer,
        wall.clone(),
        mono.clone(),
        SealPolicy::new(100, 10, 10).expect("valid policy"),
        shard_count,
    );
    sealer.init().await.expect("init");

    // scheduled: 開始時刻はすでに現在時刻（1_000）、終了時刻は 1_010。
    let snapshot = store
        .ensure_initialized(Period {
            opens_at: Some(1_000),
            closes_at: Some(1_010),
        })
        .await
        .expect("election state init");
    assert_eq!(snapshot.phase, ElectionPhase::Scheduled);

    let handle = sealer::spawn(
        sealer,
        Duration::from_millis(10),
        Duration::from_secs(600),
        store.clone(),
        Duration::from_secs(1), // 締切の手続きの猶予。
    );

    // 開始時刻に達しているので、次の tick で open になる。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(store.get().await.expect("state").phase, ElectionPhase::Open);

    // 終了時刻に進める → closing。
    wall.set(1_010);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closing
    );

    // 猶予（1 秒）が経つまでは closed にならない。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closing,
        "猶予が経つ前に closed になってはいけません"
    );

    // 単調時計を猶予より進める → 全シャードのフラッシュを確認し、closed になる。
    mono.advance(Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closed
    );

    let audit = store.recent_audit(10).await.expect("audit");
    assert_eq!(audit.len(), 3, "{audit:?}");
    assert_eq!(
        (audit[0].from, audit[0].to),
        (ElectionPhase::Closing, ElectionPhase::Closed)
    );
    assert_eq!(
        (audit[1].from, audit[1].to),
        (ElectionPhase::Open, ElectionPhase::Closing)
    );
    assert_eq!(
        (audit[2].from, audit[2].to),
        (ElectionPhase::Scheduled, ElectionPhase::Open)
    );
    assert!(audit.iter().all(|e| e.actor == "sealer:memory"));

    handle.shutdown().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn ballots_cast_just_before_closing_are_sealed_before_closed() {
    let shard_count = NonZeroU16::new(1).expect("non-zero");
    let store = Arc::new(InMemoryStore::new(shard_count));
    let mono = Arc::new(ManualClock::new());
    let wall = Arc::new(ManualWall::new(1_000));
    let signer = Arc::new(Ed25519Signer::from_seed(&[4u8; 32]));
    let mut sealer = Sealer::new(
        store.clone(),
        signer,
        wall.clone(),
        mono.clone(),
        SealPolicy::new(100, 10, 10).expect("valid policy"),
        shard_count,
    );
    sealer.init().await.expect("init");
    store
        .ensure_initialized(Period {
            opens_at: Some(1_000),
            closes_at: Some(1_010),
        })
        .await
        .expect("election state init");

    use application::VoteStore;
    use domain::{Ballot, BallotId, CandidateId, ContestId, ShardId, VoterId};
    let cast = |i: u32| {
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&i.to_be_bytes());
        Ballot {
            ballot_id: BallotId::from_random_bytes(id),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
            candidate_id: CandidateId::parse("shugiin_smd.13.01.c1").expect("valid"),
        }
    };

    let handle = sealer::spawn(
        sealer,
        Duration::from_millis(10),
        Duration::from_secs(600),
        store.clone(),
        Duration::from_secs(1),
    );
    tokio::time::sleep(Duration::from_millis(50)).await; // scheduled -> open

    // 締切の直前に投じた 7 票。
    for i in 0..7u32 {
        let voter = VoterId::new(&format!("voter-{i}")).expect("valid");
        store.cast(&voter, ShardId(0), cast(i)).await.expect("cast");
    }

    wall.set(1_010); // open -> closing
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closing
    );
    mono.advance(Duration::from_secs(2)); // 猶予を経過させる
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        store.get().await.expect("state").phase,
        ElectionPhase::Closed
    );
    assert_eq!(
        store.pending_len(ShardId(0)).await.expect("pending"),
        0,
        "closed になった時点で、未封印の票は残っていないはず"
    );
    let head = store
        .head(ShardId(0))
        .await
        .expect("head")
        .expect("initialized");
    assert_eq!(
        head.ballots.len(),
        7,
        "締切直前の 7 票は、最終ブロックまでに封印されているはず"
    );

    handle.shutdown().await.expect("shutdown");
}
