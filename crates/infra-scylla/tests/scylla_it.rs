//! ScyllaDB（または互換の Cassandra）に対する統合テスト。
//!
//! `#[ignore]` なので通常の `cargo test` では実行されない。DB を起動してから実行する:
//!
//!   docker compose up -d --wait scylla && docker compose run --rm schema
//!   cargo test -p infra-scylla -- --ignored            （接続先は db.nodes の先頭。既定 127.0.0.1:9042）
//!   APP__DB__NODES=127.0.0.1:19042 cargo test -p infra-scylla -- --ignored
//!
//! テストごとに専用のキースペースを作り、`docs/schema.cql`（テンプレート）を `infra_scylla::schema` で描画して適用する。
//! キースペース名は `{TEST_KEYSPACE_PREFIX}_it_<乱数>`（環境変数 `TEST_KEYSPACE_PREFIX`、既定 `it`）。確認スクリプトは
//! 実行ごとの専用の接頭辞（vote_s6_<UNIXTIME>_<PID> など）を渡し、終了時に接頭辞で始まるキースペースをすべて DROP する。
//! 共用のキースペース（vote）には触れない。

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use application::{CastError, ChainRead, Clock, SealStore, StoreError, VoteStore};
use domain::{
    Ballot, BallotId, CandidateId, ContestId, Ed25519Signer, ShardId, VoterId, genesis, seal_block,
};
use infra_scylla::{ConnectError, ScyllaConfig, ScyllaStore};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;

/// 2027 年ごろの UNIX 秒（分の境界: 30_000_000 分 = 1_800_000_000 秒）。
const BASE_SECS: u64 = 1_800_000_000;

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_unix_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TestDb {
    store: Arc<ScyllaStore>,
    admin: Session,
    keyspace: String,
    clock: Arc<TestClock>,
}

/// 接続先は設定（`db.nodes` の先頭。環境変数 `APP__DB__NODES` で上書きできる）から読む。
fn uri() -> String {
    let loaded = app_config::load().expect("設定を読み込めません");
    loaded
        .config
        .db
        .nodes
        .first()
        .cloned()
        .expect("db.nodes は検証済みで、1 件以上ある")
}

fn config(keyspace: &str, shards: u16) -> ScyllaConfig {
    ScyllaConfig {
        nodes: vec![uri()],
        keyspace: keyspace.to_string(),
        shard_count: NonZeroU16::new(shards).expect("non-zero"),
    }
}

async fn setup(shards: u16) -> TestDb {
    let admin = SessionBuilder::new()
        .known_node(uri())
        .build()
        .await
        .expect("DB に接続できません（db.nodes / APP__DB__NODES を確認してください）");
    let prefix = std::env::var("TEST_KEYSPACE_PREFIX").unwrap_or_else(|_| "it".to_string());
    // 名前の長さの上限（48 文字）に収まるよう、乱数は 12 桁の 16 進にする。
    let keyspace = format!(
        "{prefix}_it_{:012x}",
        rand::random::<u64>() & 0xffff_ffff_ffff
    );
    for statement in infra_scylla::schema::statements(&keyspace) {
        admin
            .query_unpaged(statement.as_str(), &[])
            .await
            .unwrap_or_else(|e| panic!("スキーマ適用に失敗: {e}\n{statement}"));
    }
    let clock = Arc::new(TestClock(AtomicU64::new(BASE_SECS)));
    let store = ScyllaStore::connect(&config(&keyspace, shards), clock.clone())
        .await
        .expect("connect");
    TestDb {
        store: Arc::new(store),
        admin,
        keyspace,
        clock,
    }
}

impl TestDb {
    async fn teardown(self) {
        let _ = self
            .admin
            .query_unpaged(format!("DROP KEYSPACE IF EXISTS {}", self.keyspace), &[])
            .await;
    }

    fn set_time(&self, secs: u64) {
        self.clock.0.store(secs, Ordering::SeqCst);
    }
}

fn voter(name: &str) -> VoterId {
    VoterId::new(name).expect("valid")
}

/// 番号 `n` の投票用紙（東京の小選挙区 `n` 区）。ID は文字列（`domain::ids`）。
fn contest(n: u32) -> ContestId {
    ContestId::parse(&format!("2026-general/shugiin_smd.13.{n:02}")).expect("valid")
}

fn ballot(n: u8, contest_no: u32, candidate_seq: u32) -> Ballot {
    Ballot {
        ballot_id: BallotId::from_random_bytes([n; 16]),
        contest_id: contest(contest_no),
        candidate_id: CandidateId::parse(&format!(
            "shugiin_smd.13.{contest_no:02}.c{candidate_seq}"
        ))
        .expect("valid"),
    }
}

fn signer() -> Ed25519Signer {
    Ed25519Signer::from_seed(&[5u8; 32])
}

async fn cast_n(store: &ScyllaStore, shard: u16, first: u8, n: u8) {
    for i in first..first + n {
        store
            .cast(
                &voter(&format!("v{i}")),
                ShardId(shard),
                ballot(i, 1, 100 + u32::from(i)),
            )
            .await
            .expect("cast");
    }
}

// --- VoteStore ---

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn participation_lwt_and_pool_basics() {
    let db = setup(2).await;
    let s = &db.store;
    let alice = voter("alice");

    assert_eq!(s.cast(&alice, ShardId(1), ballot(1, 1, 101)).await, Ok(()));
    assert_eq!(
        s.cast(&alice, ShardId(0), ballot(2, 1, 102)).await,
        Err(CastError::AlreadyVoted)
    );
    // 拒否された票はプールに入らない。他の投票用紙・他の投票者は投票できる。
    assert_eq!(s.pending_by_shard().await, Ok(vec![0, 1]));
    assert_eq!(s.cast(&alice, ShardId(0), ballot(3, 2, 203)).await, Ok(()));
    assert_eq!(
        s.cast(&voter("bob"), ShardId(0), ballot(4, 1, 101)).await,
        Ok(())
    );

    let mut voted = s.voted_contests(&alice).await.expect("voted");
    voted.sort();
    assert_eq!(voted, vec![contest(1), contest(2)]);
    assert_eq!(s.voted_contests(&voter("carol")).await, Ok(vec![]));
    assert_eq!(s.pending_by_shard().await, Ok(vec![2, 1]));

    // 不正なシャードでは、投票済みにならない。
    assert_eq!(
        s.cast(&voter("dave"), ShardId(9), ballot(5, 1, 101)).await,
        Err(CastError::Store(StoreError::InvalidShard))
    );
    assert_eq!(s.voted_contests(&voter("dave")).await, Ok(vec![]));
    db.teardown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ScyllaDB"]
async fn lwt_lets_exactly_one_of_100_concurrent_votes_through() {
    let db = setup(4).await;
    let alice = voter("alice");
    let mut tasks = Vec::new();
    for i in 0..100u8 {
        let (store, alice) = (db.store.clone(), alice.clone());
        tasks.push(tokio::spawn(async move {
            store
                .cast(&alice, ShardId(u16::from(i % 4)), ballot(i, 1, 101))
                .await
        }));
    }
    let (mut ok, mut rejected) = (0, 0);
    for task in tasks {
        match task.await.expect("task") {
            Ok(()) => ok += 1,
            Err(CastError::AlreadyVoted) => rejected += 1,
            Err(e) => panic!("unexpected: {e}"),
        }
    }
    assert_eq!((ok, rejected), (1, 99));
    let pending: usize = db
        .store
        .pending_by_shard()
        .await
        .expect("counts")
        .iter()
        .sum();
    assert_eq!(pending, 1, "拒否された票はプールに入らない");
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn an_existing_participation_row_from_another_attempt_is_a_duplicate() {
    let db = setup(1).await;
    // 別のリクエスト（別の attempt）が先に記録した行がある状態。
    db.admin
        .query_unpaged(
            format!(
                "INSERT INTO {}.participation (voter_id, contest_id, attempt) \
                 VALUES ('alice', '{}', 0x00112233445566778899aabbccddeeff)",
                db.keyspace,
                contest(1)
            ),
            &[],
        )
        .await
        .expect("insert");
    assert_eq!(
        db.store
            .cast(&voter("alice"), ShardId(0), ballot(1, 1, 101))
            .await,
        Err(CastError::AlreadyVoted)
    );
    assert_eq!(db.store.pending_by_shard().await, Ok(vec![0]));
    db.teardown().await;
}

// --- ballot_pool ---

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn pool_is_ordered_by_minute_then_ballot_id() {
    let db = setup(1).await;
    // 先に届いた分（100 分）の票 3 つ、次の分（101 分）の票 2 つ。ballot_id はわざと逆順に投じる。
    db.set_time(BASE_SECS + 5);
    for n in [30u8, 10, 20] {
        db.store
            .cast(
                &voter(&format!("a{n}")),
                ShardId(0),
                ballot(n, 1, 100 + u32::from(n)),
            )
            .await
            .expect("cast");
    }
    db.set_time(BASE_SECS + 65);
    for n in [5u8, 4] {
        db.store
            .cast(
                &voter(&format!("b{n}")),
                ShardId(0),
                ballot(n, 1, 100 + u32::from(n)),
            )
            .await
            .expect("cast");
    }

    let all = db.store.peek_pending(ShardId(0), 10).await.expect("peek");
    let ids: Vec<u8> = all.iter().map(|b| b.ballot_id.0[0]).collect();
    // 分の古い順、同じ分の中は ballot_id の順。
    assert_eq!(ids, vec![10, 20, 30, 4, 5]);
    // peek は削除せず、件数で切れる。
    let first_three = db.store.peek_pending(ShardId(0), 3).await.expect("peek");
    assert_eq!(first_three.len(), 3);
    assert_eq!(db.store.pending_len(ShardId(0)).await, Ok(5));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn pool_write_times_are_rounded_to_the_minute() {
    let db = setup(1).await;
    // 同じ分の中の、秒の違う 2 つの時刻で投票する。
    db.set_time(BASE_SECS + 3);
    cast_n(&db.store, 0, 1, 1).await;
    db.set_time(BASE_SECS + 57);
    cast_n(&db.store, 0, 2, 1).await;
    db.set_time(BASE_SECS + 61);
    cast_n(&db.store, 0, 3, 1).await;

    let result = db
        .admin
        .query_unpaged(
            format!(
                "SELECT WRITETIME(contest_id), WRITETIME(candidate_id) FROM {}.ballot_pool WHERE shard = 0",
                db.keyspace
            ),
            &[],
        )
        .await
        .expect("select")
        .into_rows_result()
        .expect("rows");
    let times: Vec<(i64, i64)> = result
        .rows::<(i64, i64)>()
        .expect("rows")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(times.len(), 3);
    for (a, b) in &times {
        assert_eq!(a % 60_000_000, 0, "書き込み時刻が分に丸められていない: {a}");
        assert_eq!(a, b);
    }
    // 同じ分（BASE+3 と BASE+57）の票は書き込み時刻が同一で、次の分の票だけが 1 分後。
    let mut distinct: Vec<i64> = times.iter().map(|t| t.0).collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 2);
    assert_eq!(distinct[1] - distinct[0], 60_000_000);
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn participation_and_pool_share_no_identifying_columns() {
    let db = setup(1).await;
    let columns = |table: &'static str| {
        let (admin, keyspace) = (&db.admin, db.keyspace.clone());
        async move {
            let result = admin
                .query_unpaged(
                    "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
                    (keyspace, table),
                )
                .await
                .expect("select")
                .into_rows_result()
                .expect("rows");
            let mut names: Vec<String> = result
                .rows::<(String,)>()
                .expect("rows")
                .map(|r| r.expect("row").0)
                .collect();
            names.sort();
            names
        }
    };
    let pool = columns("ballot_pool").await;
    let participation = columns("participation").await;
    // 票の側に投票者を特定する列がない / 投票済みの側に票の内容を示す列がない。
    assert!(
        !pool.iter().any(|c| c.contains("voter") || c == "attempt"),
        "{pool:?}"
    );
    assert!(
        !participation
            .iter()
            .any(|c| c.contains("ballot") || c.contains("candidate") || c.contains("minute")),
        "{participation:?}"
    );
    db.teardown().await;
}

// --- チェーン（blocks）とプールの連携 ---

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn commit_appends_block_and_removes_sealed_ballots() {
    let db = setup(2).await;
    let (s, signer) = (&db.store, signer());
    assert_eq!(s.head(ShardId(0)).await, Ok(None));
    assert_eq!(s.head(ShardId(9)).await, Ok(None));

    let g = genesis(&signer, 100);
    s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
    assert_eq!(s.head(ShardId(0)).await, Ok(Some(g.clone())));
    assert_eq!(s.block(ShardId(0), 0).await, Ok(Some(g.clone())));
    assert_eq!(
        s.head(ShardId(1)).await,
        Ok(None),
        "チェーンはシャードごとに独立"
    );

    cast_n(s, 0, 1, 5).await;
    cast_n(s, 1, 100, 2).await;
    let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");
    assert_eq!(batch.len(), 3);
    let block = seal_block(&g, batch, 101, &signer).expect("seal");
    s.commit(ShardId(0), block.clone(), 3)
        .await
        .expect("commit");

    // ブロックの票の順序（ハッシュ順）と内容が保たれ、封印済みの 3 件だけがプールから消える。
    assert_eq!(s.head(ShardId(0)).await, Ok(Some(block.clone())));
    assert_eq!(s.block(ShardId(0), 1).await, Ok(Some(block)));
    assert_eq!(s.block(ShardId(0), 2).await, Ok(None));
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(2));
    assert_eq!(s.pending_len(ShardId(1)).await, Ok(2));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn commit_rejects_conflicts_without_changing_state() {
    let db = setup(1).await;
    let (s, signer) = (&db.store, signer());
    let g = genesis(&signer, 100);
    cast_n(s, 0, 1, 3).await;
    let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");

    // チェーンが空のとき、ジェネシス以外は置けない。
    let orphan = seal_block(&g, batch.clone(), 101, &signer).expect("seal");
    assert_eq!(
        s.commit(ShardId(0), orphan, 3).await,
        Err(StoreError::Conflict)
    );

    s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
    // 別内容の高さ 0 は矛盾（LWT に負ける）。
    assert_eq!(
        s.commit(ShardId(0), genesis(&signer, 999), 0).await,
        Err(StoreError::Conflict)
    );
    // prev_hash 不一致 / 消費件数の不一致 / プールにない票。
    let mut bad_prev = seal_block(&g, batch.clone(), 101, &signer).expect("seal");
    bad_prev.header.prev_hash[0] ^= 1;
    assert_eq!(
        s.commit(ShardId(0), bad_prev, 3).await,
        Err(StoreError::Conflict)
    );
    let good = seal_block(&g, batch, 101, &signer).expect("seal");
    assert_eq!(
        s.commit(ShardId(0), good.clone(), 2).await,
        Err(StoreError::Conflict)
    );
    let foreign = seal_block(&g, vec![ballot(200, 1, 1)], 101, &signer).expect("seal");
    assert_eq!(
        s.commit(ShardId(0), foreign, 1).await,
        Err(StoreError::Conflict)
    );
    assert_eq!(
        s.commit(ShardId(7), g.clone(), 0).await,
        Err(StoreError::InvalidShard)
    );

    assert_eq!(s.pending_len(ShardId(0)).await, Ok(3));
    assert_eq!(s.head(ShardId(0)).await, Ok(Some(g)));
    s.commit(ShardId(0), good, 3).await.expect("commit");
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(0));
    db.teardown().await;
}

fn hex(bytes: &[u8]) -> String {
    format!(
        "0x{}",
        bytes.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// `commit` を経由せずにブロックだけを書く。「ブロックを追加した後、プールの削除の前に落ちた」
/// 状態（ブロックはあるが、票はプールに残っている）を作るため。
async fn insert_block_only(db: &TestDb, block: &domain::Block) {
    let h = &block.header;
    let ballots: Vec<String> = block
        .ballots
        .iter()
        .map(|b| {
            format!(
                "({}, '{}', '{}')",
                hex(&b.ballot_id.0),
                b.contest_id,
                b.candidate_id
            )
        })
        .collect();
    db.admin
        .query_unpaged(
            format!(
                "INSERT INTO {}.blocks (shard, height, format_version, prev_hash, merkle_root, \
                 ballot_count, sealed_at_minute, block_hash, signature, ballots) \
                 VALUES (0, {}, {}, {}, {}, {}, {}, {}, {}, [{}])",
                db.keyspace,
                h.height,
                h.version,
                hex(&h.prev_hash),
                hex(&h.merkle_root),
                h.ballot_count,
                h.sealed_at_minute,
                hex(&block.block_hash),
                hex(&block.signature),
                ballots.join(", ")
            ),
            &[],
        )
        .await
        .expect("insert block");
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn recover_and_retried_commit_clean_up_a_partially_applied_commit() {
    let db = setup(1).await;
    let (s, signer) = (&db.store, signer());
    let g = genesis(&signer, 100);
    s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");

    // 落ちた状態 1: ブロックは書かれたが、票がプールに残っている。recover が取り除く（冪等）。
    cast_n(s, 0, 1, 4).await;
    let batch = s.peek_pending(ShardId(0), 4).await.expect("peek");
    let block1 = seal_block(&g, batch, 101, &signer).expect("seal");
    insert_block_only(&db, &block1).await;
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(4));
    s.recover(ShardId(0)).await.expect("recover");
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(0));
    s.recover(ShardId(0)).await.expect("recover again");
    assert_eq!(s.head(ShardId(0)).await, Ok(Some(block1.clone())));

    // 落ちた状態 2: 同じブロックでの commit の再試行は、成功してプールの後始末だけを行う。
    cast_n(s, 0, 10, 3).await;
    let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");
    let block2 = seal_block(&block1, batch, 102, &signer).expect("seal");
    insert_block_only(&db, &block2).await;
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(3));
    s.commit(ShardId(0), block2.clone(), 3)
        .await
        .expect("retried commit");
    assert_eq!(s.pending_len(ShardId(0)).await, Ok(0));
    assert_eq!(s.head(ShardId(0)).await, Ok(Some(block2)));
    // 別内容の同じ高さは、引き続き矛盾として拒否される。
    let other = seal_block(&block1, vec![ballot(99, 1, 1)], 102, &signer).expect("seal");
    assert_eq!(
        s.commit(ShardId(0), other, 1).await,
        Err(StoreError::Conflict)
    );
    db.teardown().await;
}

// --- 永続化・接続 ---

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn state_survives_a_new_connection() {
    let db = setup(1).await;
    let (s, signer) = (&db.store, signer());
    let g = genesis(&signer, 100);
    s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
    cast_n(s, 0, 1, 3).await;
    let batch = s.peek_pending(ShardId(0), 2).await.expect("peek");
    let block = seal_block(&g, batch, 101, &signer).expect("seal");
    s.commit(ShardId(0), block.clone(), 2)
        .await
        .expect("commit");

    // 別の接続（= プロセスの再起動を想定）から、投票状況・チェーン・未封印の票が見える。
    let reopened = ScyllaStore::connect(&config(&db.keyspace, 1), db.clock.clone())
        .await
        .expect("reconnect");
    assert_eq!(
        reopened.voted_contests(&voter("v1")).await,
        Ok(vec![contest(1)])
    );
    assert_eq!(
        reopened
            .cast(&voter("v1"), ShardId(0), ballot(50, 1, 101))
            .await,
        Err(CastError::AlreadyVoted)
    );
    assert_eq!(reopened.head(ShardId(0)).await, Ok(Some(block)));
    assert_eq!(reopened.pending_len(ShardId(0)).await, Ok(1));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn connect_fails_clearly_when_the_keyspace_is_missing() {
    let clock = Arc::new(TestClock(AtomicU64::new(BASE_SECS)));
    let err = ScyllaStore::connect(&config("no_such_keyspace_for_test", 1), clock)
        .await
        .expect_err("should fail");
    assert!(matches!(err, ConnectError::Connect(_)), "{err}");
}

// --- リース・アンカー・署名鍵・クラスタ設定・監査用の集計 ---

use application::LeaseStore;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn lease_is_exclusive_renewable_expiring_and_releasable() {
    let db = setup(1).await;
    let s = &db.store;
    let ttl = Duration::from_secs(3);

    assert_eq!(s.try_acquire("shard-0", "sealer-a", ttl).await, Ok(true));
    // 自分が既に持っているリースの再取得は成功（結果が不明だった先の試行と区別できない場合の冪等性）。
    assert_eq!(s.try_acquire("shard-0", "sealer-a", ttl).await, Ok(true));
    assert_eq!(s.try_acquire("shard-0", "sealer-b", ttl).await, Ok(false));
    // 更新は持っている owner だけ。
    assert_eq!(s.renew("shard-0", "sealer-b", ttl).await, Ok(false));
    assert_eq!(s.renew("shard-0", "sealer-a", ttl).await, Ok(true));
    // 他の owner の解放は効かない。
    s.release("shard-0", "sealer-b").await.expect("no-op");
    assert_eq!(s.try_acquire("shard-0", "sealer-b", ttl).await, Ok(false));
    // 別の名前のリースは独立している。
    assert_eq!(s.try_acquire("shard-1", "sealer-b", ttl).await, Ok(true));

    // 更新しなければ TTL で失効し、失った側は更新できず、別の owner が取得できる。
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(s.renew("shard-0", "sealer-a", ttl).await, Ok(false));
    assert_eq!(s.try_acquire("shard-0", "sealer-b", ttl).await, Ok(true));
    // 更新し続ければ、TTL を超えても保持できる。
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(s.renew("shard-0", "sealer-b", ttl).await, Ok(true));
    }
    // 解放すると、すぐ別の owner が取得できる。
    s.release("shard-0", "sealer-b").await.expect("release");
    assert_eq!(s.try_acquire("shard-0", "sealer-a", ttl).await, Ok(true));
    db.teardown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ScyllaDB"]
async fn only_one_of_many_concurrent_acquirers_gets_the_lease() {
    let db = setup(1).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let store = db.store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .try_acquire("shard-0", &format!("sealer-{i}"), Duration::from_secs(30))
                .await
        }));
    }
    let mut winners = 0;
    for task in tasks {
        if task.await.expect("task").expect("acquire") {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
    db.teardown().await;
}

fn anchor(seq: u64, prev: [u8; 32]) -> domain::Anchor {
    domain::build_anchor(
        seq,
        30_000_000 + seq,
        prev,
        vec![
            domain::ShardHead {
                shard: 1,
                height: seq + 1,
                block_hash: [2; 32],
            },
            domain::ShardHead {
                shard: 0,
                height: seq,
                block_hash: [1; 32],
            },
        ],
        &signer(),
    )
    .expect("anchor")
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn anchors_are_stored_once_per_seq_and_latest_wins() {
    let db = setup(2).await;
    let s = &db.store;
    assert_eq!(s.latest_anchor().await, Ok(None));

    let a1 = anchor(1, [0; 32]);
    assert_eq!(s.append_anchor(&a1).await, Ok(true));
    assert_eq!(s.latest_anchor().await, Ok(Some(a1.clone())));
    // 同じ seq は競り負け。ただし、自分と同一内容の再試行（結果が不明だった先の試行）は成功。
    let rival = anchor(1, [9; 32]);
    assert_eq!(s.append_anchor(&rival).await, Ok(false));
    assert_eq!(s.append_anchor(&a1).await, Ok(true));
    assert_eq!(s.latest_anchor().await, Ok(Some(a1.clone())));

    let a2 = anchor(2, a1.anchor_hash);
    assert_eq!(s.append_anchor(&a2).await, Ok(true));
    assert_eq!(s.latest_anchor().await, Ok(Some(a2.clone())));
    assert_eq!(domain::verify_anchor_link(&a1, &a2), Ok(()));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn anchors_are_listed_newest_first_with_a_limit() {
    let db = setup(2).await;
    let s = &db.store;
    assert_eq!(s.latest_anchors(5).await, Ok(vec![]));
    let a1 = anchor(1, [0; 32]);
    let a2 = anchor(2, a1.anchor_hash);
    let a3 = anchor(3, a2.anchor_hash);
    for a in [&a1, &a2, &a3] {
        assert_eq!(s.append_anchor(a).await, Ok(true));
    }
    assert_eq!(
        s.latest_anchors(10).await,
        Ok(vec![a3.clone(), a2.clone(), a1.clone()])
    );
    assert_eq!(s.latest_anchors(2).await, Ok(vec![a3, a2]));
    assert_eq!(s.latest_anchors(0).await, Ok(vec![]));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn blocks_are_read_in_pages_newest_first_per_shard() {
    let db = setup(2).await;
    let (s, signer) = (&db.store, signer());
    let mut chain = vec![genesis(&signer, 100)];
    s.commit(ShardId(0), chain[0].clone(), 0)
        .await
        .expect("genesis");
    for i in 0..5u8 {
        cast_n(s, 0, i * 3 + 1, 1).await;
        let batch = s.peek_pending(ShardId(0), 1).await.expect("peek");
        let block = seal_block(
            chain.last().expect("head"),
            batch,
            101 + u64::from(i),
            &signer,
        )
        .expect("seal");
        s.commit(ShardId(0), block.clone(), 1)
            .await
            .expect("commit");
        chain.push(block);
    }
    let heights = |blocks: Vec<domain::Block>| -> Vec<u64> {
        blocks.iter().map(|b| b.header.height).collect()
    };
    // 先頭から。limit で切る。
    let top = s.blocks_before(ShardId(0), None, 4).await.expect("top");
    assert_eq!(heights(top.clone()), vec![5, 4, 3, 2]);
    assert_eq!(top[0], chain[5], "内容（票を含む）がそのまま読める");
    // before_height は「それより低い」高さ。
    let page = s
        .blocks_before(ShardId(0), Some(2), 10)
        .await
        .expect("page");
    assert_eq!(heights(page), vec![1, 0]);
    assert_eq!(
        heights(
            s.blocks_before(ShardId(0), Some(0), 10)
                .await
                .expect("zero")
        ),
        Vec::<u64>::new()
    );
    // 先頭より先を指しても、先頭から返す。
    assert_eq!(
        heights(
            s.blocks_before(ShardId(0), Some(99), 2)
                .await
                .expect("beyond")
        ),
        vec![5, 4]
    );
    // チェーンが無いシャード・存在しないシャードは空。
    assert_eq!(s.blocks_before(ShardId(1), None, 5).await, Ok(vec![]));
    assert_eq!(s.blocks_before(ShardId(9), None, 5).await, Ok(vec![]));
    assert_eq!(s.blocks_before(ShardId(0), None, 0).await, Ok(vec![]));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn signer_key_is_registered_once_and_readable() {
    let db = setup(1).await;
    let s = &db.store;
    assert_eq!(s.signer_public_key().await, Ok(None));
    s.register_signer([1; 32]).await.expect("register");
    s.register_signer([1; 32]).await.expect("same key again");
    assert_eq!(s.register_signer([2; 32]).await, Err(StoreError::Conflict));
    assert_eq!(s.signer_public_key().await, Ok(Some([1; 32])));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn connecting_with_a_different_shard_count_is_refused() {
    let db = setup(4).await;
    // 同じシャード数なら再接続できる。
    ScyllaStore::connect(&config(&db.keyspace, 4), db.clock.clone())
        .await
        .expect("same shard count");
    let err = ScyllaStore::connect(&config(&db.keyspace, 2), db.clock.clone())
        .await
        .expect_err("mismatch");
    assert!(matches!(err, ConnectError::ClusterConfig(_)), "{err}");
    assert!(err.to_string().contains("shard.count=2"), "{err}");
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn audit_counts_count_participation_and_pending_per_contest() {
    let db = setup(2).await;
    let s = &db.store;
    assert_eq!(s.audit_counts().await, Ok(vec![]));
    for (name, shard, contest, n) in [
        ("a", 0u16, 1u32, 1u8),
        ("b", 1, 1, 2),
        ("c", 0, 2, 3),
        ("d", 1, 3, 4),
    ] {
        s.cast(
            &voter(name),
            ShardId(shard),
            ballot(n, contest, 100 + u32::from(n)),
        )
        .await
        .expect("cast");
    }
    s.cast(&voter("a"), ShardId(1), ballot(9, 2, 200))
        .await
        .expect("cast");

    let rows = |counts: Vec<application::ContestCounts>| -> Vec<(ContestId, u64, u64)> {
        counts
            .into_iter()
            .map(|c| (c.contest, c.participation, c.pending))
            .collect()
    };
    assert_eq!(
        rows(s.audit_counts().await.expect("counts")),
        vec![(contest(1), 2, 2), (contest(2), 2, 2), (contest(3), 1, 1)]
    );

    // 封印してプールから消えると、pending だけが減る。
    let signer = signer();
    let g = genesis(&signer, 100);
    s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
    let batch = s.peek_pending(ShardId(0), 10).await.expect("peek");
    let n = batch.len();
    let block = seal_block(&g, batch, 101, &signer).expect("seal");
    s.commit(ShardId(0), block, n).await.expect("commit");
    let after = rows(s.audit_counts().await.expect("counts"));
    assert_eq!(after.iter().map(|r| r.1).collect::<Vec<_>>(), vec![2, 2, 1]);
    assert_eq!(after.iter().map(|r| r.2).sum::<u64>(), 3);
    db.teardown().await;
}

// --- 認証情報・名簿・台帳（auth.mode=db。credgen が登録し、api が引く）---

use application::{CredentialAdmin, CredentialRecord, CredentialStore, RegistryEntry, VoterRoll};
use domain::DistrictId;

fn credential(login_id: &str, voter_id: &str) -> CredentialRecord {
    CredentialRecord {
        login_id: login_id.to_string(),
        password_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2g"
            .to_string(),
        voter_id: voter(voter_id),
    }
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn credentials_are_unique_per_login_id_and_can_be_found_and_deleted() {
    let db = setup(1).await;
    let record = credential("ABCDEFGHJK", "v0123456789abcdef0123456789abcdef");
    assert_eq!(db.store.find("ABCDEFGHJK").await, Ok(None));
    // LWT: 最初の登録だけが通り、同じログイン ID の 2 つ目は（別の有権者でも）拒否される。
    assert_eq!(db.store.insert_credential(&record).await, Ok(true));
    let other = credential("ABCDEFGHJK", "vffffffffffffffffffffffffffffffff");
    assert_eq!(db.store.insert_credential(&other).await, Ok(false));
    assert_eq!(db.store.find("ABCDEFGHJK").await, Ok(Some(record.clone())));
    // 削除すると、見つからず、同じ ID を再登録できる。
    assert_eq!(db.store.delete_credential("ABCDEFGHJK").await, Ok(()));
    assert_eq!(db.store.find("ABCDEFGHJK").await, Ok(None));
    assert_eq!(db.store.insert_credential(&other).await, Ok(true));
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn registry_and_roll_round_trip() {
    let db = setup(1).await;
    assert_eq!(db.store.registry_get("voter-1").await, Ok(None));
    let entry = RegistryEntry {
        external_id: "voter-1".to_string(),
        voter_id: voter("v0123456789abcdef0123456789abcdef"),
        login_id: "ABCDEFGHJK".to_string(),
    };
    assert_eq!(db.store.upsert_registry(&entry).await, Ok(()));
    assert_eq!(
        db.store.registry_get("voter-1").await,
        Ok(Some(entry.clone()))
    );
    // 上書き（再発行で、現在のログイン ID が変わる）。
    let reissued = RegistryEntry {
        login_id: "ZZZZZZZZZZ".to_string(),
        ..entry.clone()
    };
    assert_eq!(db.store.upsert_registry(&reissued).await, Ok(()));
    assert_eq!(db.store.registry_get("voter-1").await, Ok(Some(reissued)));

    // 名簿: 選挙区のリスト（順序を保つ）。無い有権者は None。
    let districts: Vec<DistrictId> = ["shugiin_smd.13.01", "shugiin_pr.tokyo", "governor.13"]
        .iter()
        .map(|d| DistrictId::new(d).expect("valid"))
        .collect();
    assert_eq!(db.store.districts_of(&entry.voter_id).await, Ok(None));
    assert_eq!(db.store.put_roll(&entry.voter_id, &districts).await, Ok(()));
    assert_eq!(
        db.store.districts_of(&entry.voter_id).await,
        Ok(Some(districts))
    );
    db.teardown().await;
}

#[tokio::test]
#[ignore = "requires ScyllaDB"]
async fn election_rules_are_fixed_by_the_open_transition() {
    use application::ElectionStateStore;
    use domain::{ElectionPhase, ElectionRules, Period};

    let db = setup(1).await;
    let s = &db.store;
    let on = ElectionRules { allow_blank: true };
    let off = ElectionRules { allow_blank: false };
    let initial = s.ensure_initialized(Period::default()).await.expect("init");
    assert_eq!(initial.rules, None, "scheduled の間は、まだ固定しない");
    // open への遷移（LWT）と同じ書き込みで固定する。遅れて来た 2 つ目の遷移は失敗し、値を変えない。
    assert_eq!(
        s.transition(ElectionPhase::Scheduled, ElectionPhase::Open, off, "a", 10)
            .await,
        Ok(true)
    );
    assert_eq!(
        s.transition(ElectionPhase::Scheduled, ElectionPhase::Open, on, "b", 11)
            .await,
        Ok(false)
    );
    assert_eq!(s.get().await.expect("get").rules, Some(off));
    // 後の遷移に別のルールを渡しても、固定した値のまま。別の接続（再起動）からも同じ値が見える。
    assert_eq!(
        s.transition(ElectionPhase::Open, ElectionPhase::Closing, on, "a", 20)
            .await,
        Ok(true)
    );
    let reopened = ScyllaStore::connect(&config(&db.keyspace, 1), db.clock.clone())
        .await
        .expect("reconnect");
    let snapshot = reopened.get().await.expect("get");
    assert_eq!(snapshot.phase, ElectionPhase::Closing);
    assert_eq!(snapshot.rules, Some(off));
    db.teardown().await;
}
