//! アプリケーション状態の組み立て。

use std::num::NonZeroU16;
use std::sync::Arc;

use anyhow::Context;
use app_config::{AuthMode, DisplayTimezone, Secret};
pub use application::SystemClock;
use application::{
    Authenticator, ChainRead, Clock, DbAuthenticator, ElectionStateStore, RandomBallotIds,
    SealStore, SessionSigner, StubAuthenticator, VoteStore, VoterRoll, VotingService,
};
use domain::election::Election;
use domain::{Ed25519Signer, ElectionRules};
use infra_memory::{InMemoryStore, StaticElectionRepository, StaticVoterRoll};
use infra_scylla::{ScyllaConfig, ScyllaStore};
use sealer::{MonotonicClock, Sealer};

use crate::ElectionGate;
use crate::auth::BlockingChecker;
use crate::chain_view::RevealPolicy;
use crate::config::{Config, Storage};

/// 各ハンドラが共有する状態。セッション状態は持たない（トークンは HMAC 署名で自己完結）。
pub struct AppState {
    pub auth: Arc<dyn Authenticator>,
    pub sessions: SessionSigner,
    pub voting: VotingService,
    pub clock: Arc<dyn Clock>,
    /// 封印済みチェーンの読み取り（公開データ）。
    pub chains: Arc<dyn ChainRead>,
    /// 選挙マスタ（読み取り専用のキャッシュ）。ブロックの詳細に、選挙区・候補者の表示名を付けるのに使う。
    pub election: Arc<Election>,
    /// シャード数（`shard.count`）。`/api/v1/chains` が、シャードの一覧を作るのに使う。
    pub shard_count: NonZeroU16,
    /// 票の中身を公開するタイミング（`chain.reveal_ballots`）。
    pub reveal: RevealPolicy,
    /// エラーメッセージに使う呼び名（設定の `labels`）。
    pub labels: ApiLabels,
    /// 選挙状態（scheduled → open → closing → closed。原則17）。
    pub election_state: Arc<dyn ElectionStateStore>,
    /// 選挙状態の短期キャッシュ（原則18: 投票の受け付けの判定に使う）。
    pub election_gate: ElectionGate,
    /// このプロセスの設定の選挙のルール（`vote.allow_blank`）。open に遷移させるときに固定する値。
    /// 固定した後は、選挙状態に保存した値が優先する（[`AppState::rules`]。原則19）。
    pub configured_rules: ElectionRules,
    /// 画面に表示するタイムゾーン（`election.display_timezone`）。
    pub display_timezone: DisplayTimezone,
    /// 管理用エンドポイントのトークン（秘密情報）。未設定なら管理用エンドポイントはすべて拒否する。
    pub admin_token: Option<Secret<String>>,
    /// リクエストのタイムアウト（`api.request_timeout_secs`）。
    pub request_timeout: std::time::Duration,
    /// 改ざんデモ用にメモリ上のストアを直接触るための参照（dev-tools 限定）。
    /// `app.mode=memory` のときだけ `Some`（scylla では改ざんデモは未対応）。
    #[cfg(feature = "dev-tools")]
    pub dev_store: Option<Arc<InMemoryStore>>,
}

/// API のエラーメッセージに使う、設定の `labels`。
#[derive(Debug, Clone)]
pub struct ApiLabels {
    /// 投票用紙 1 枚の呼び名（`labels.ballot_item`）。
    pub ballot_item: String,
    /// 開始前に投票しようとしたときのメッセージ。
    pub voting_not_started_message: String,
    /// 締切の手続き中に投票しようとしたときのメッセージ。
    pub voting_closing_message: String,
    /// 終了後に投票しようとしたときのメッセージ。
    pub voting_closed_message: String,
    /// 白票の呼び名（`labels.blank_name`）。白票を受け付けない選挙で、白票が指定されたときのメッセージに使う。
    pub blank_name: String,
}

impl AppState {
    /// 実際に使う選挙のルール: open の時点で固定した値があればそれ、無ければこのプロセスの設定の値。
    pub fn rules(&self, snapshot: &application::ElectionStateSnapshot) -> ElectionRules {
        ElectionRules::effective(snapshot.rules, self.configured_rules)
    }
}

impl Default for ApiLabels {
    fn default() -> Self {
        Self {
            ballot_item: "投票用紙".to_string(),
            voting_not_started_message: "投票の受付はまだ開始していません".to_string(),
            voting_closing_message: "投票の受付を締め切っています。しばらくお待ちください"
                .to_string(),
            voting_closed_message: "投票の受付は終了しました".to_string(),
            blank_name: "白票".to_string(),
        }
    }
}

/// 組み立て結果。
pub struct Built {
    pub state: Arc<AppState>,
    /// `app.mode=memory` のときだけ `Some`: api プロセス内のタスクとして動かす sealer（`init` 済みで、
    /// 各シャードにジェネシスがある）。`sealer::spawn` で起動する。
    /// `app.mode=db` では `None`（独立した sealer プロセスが封印し、ジェネシスも作る）。
    pub sealer: Option<Sealer>,
}

/// 選んだ保存先から作る、api の部品。`scylla` は DB モードのときだけ `Some`（認証情報と名簿を引くのに使う）。
struct Backend {
    votes: Arc<dyn VoteStore>,
    chains: Arc<dyn ChainRead>,
    election_state: Arc<dyn ElectionStateStore>,
    sealer: Option<Sealer>,
    scylla: Option<Arc<ScyllaStore>>,
}

/// 設定から依存を組み立てる。ポートの実装（現在はインメモリ）はここで選ぶ。
pub async fn build(
    config: &Config,
    clock: Arc<dyn Clock>,
    mono: Arc<dyn MonotonicClock>,
) -> anyhow::Result<Built> {
    // 選挙マスタ（と、stub 認証のときは有権者名簿）を読む（読み取り専用のキャッシュ）。不正なデータは、ここで、
    // ファイル・行つきのエラーになる。`auth.mode=db` の名簿は DB（`voter_roll`）にあるので、`voters.csv` は読まない。
    let (election, stub_roll) = match config.auth_mode {
        AuthMode::Stub => {
            let seeded = seed::load(&config.seed_dir, &config.election_id)
                .context("選挙データの読み込みに失敗しました")?;
            let voters = seeded.voters.len();
            tracing::info!(
                election_id = %config.election_id,
                contests = seeded.election.contests().len(),
                candidates = seeded.election.candidate_count(),
                voters,
                "選挙データを読み込みました"
            );
            (seeded.election, Some(seeded.voters.into_map()))
        }
        AuthMode::Db => {
            let dir = seed::election_dir(&config.seed_dir, &config.election_id);
            let election =
                seed::load_election(&dir).context("選挙データの読み込みに失敗しました")?;
            tracing::info!(
                election_id = %config.election_id,
                contests = election.contests().len(),
                candidates = election.candidate_count(),
                "選挙データを読み込みました（名簿と認証情報は DB から読みます）"
            );
            (election, None)
        }
    };
    let sessions = SessionSigner::new(config.session_secret.as_bytes(), config.session_ttl_secs)
        .context("session.secret が不正です")?;

    // 保存先を選ぶ。票のプールとチェーンは、どちらも同じストアが持つ。
    #[cfg(feature = "dev-tools")]
    let mut dev_store = None;
    let Backend {
        votes,
        chains,
        election_state,
        sealer,
        scylla,
    } = match &config.storage {
        Storage::Memory => {
            let store = Arc::new(InMemoryStore::new(config.shard_count));
            #[cfg(feature = "dev-tools")]
            {
                dev_store = Some(store.clone());
            }
            // メモリ保存では、sealer を同じプロセス内のタスクとして動かす（リースは使わない）。
            let seed = config.sealer_signing_seed.unwrap_or_else(|| {
                    tracing::warn!(
                        "sealer.signing_seed が未設定のため、署名鍵をランダムに生成しました（再起動で変わります）"
                    );
                    rand::random()
                });
            let mut sealer = Sealer::new(
                store.clone() as Arc<dyn SealStore>,
                Arc::new(Ed25519Signer::from_seed(&seed)),
                clock.clone(),
                mono,
                config.seal_policy,
                config.shard_count,
            );
            // 署名鍵の登録、各シャードの復旧とジェネシスの作成。
            sealer
                .init()
                .await
                .context("チェーンの初期化（ジェネシス作成）に失敗しました")?;
            Backend {
                votes: store.clone(),
                chains: store.clone(),
                election_state: store,
                sealer: Some(sealer),
                scylla: None,
            }
        }
        Storage::Scylla { nodes, keyspace } => {
            let store = Arc::new(
                ScyllaStore::connect(
                    &ScyllaConfig {
                        nodes: nodes.clone(),
                        keyspace: keyspace.clone(),
                        shard_count: config.shard_count,
                    },
                    clock.clone(),
                )
                .await
                .context("ScyllaDB への接続に失敗しました")?,
            );
            tracing::info!(%keyspace, nodes = nodes.len(), "ScyllaDB に接続しました");
            // 封印・ジェネシスの作成・アンカーは、独立した sealer プロセスが行う。api は持たない。
            Backend {
                votes: store.clone(),
                chains: store.clone(),
                election_state: store.clone(),
                sealer: None,
                scylla: Some(store),
            }
        }
    };

    // 選挙状態（原則17）: init（スキーマを投入した後の最初の接続）で、設定の期間を取り込む。
    // その後に設定ファイルの期間と DB（memory モードではプロセス内）の期間が違っていたら、
    // 起動時に警告して、その値（DB 側）を使う。
    let election_snapshot = election_state
        .ensure_initialized(config.period)
        .await
        .context("選挙状態の初期化に失敗しました")?;
    if election_snapshot.period != config.period {
        tracing::warn!(
            configured = ?config.period,
            stored = ?election_snapshot.period,
            "設定ファイルの投票期間と、保存されている期間が異なります。保存されている値を使います"
        );
    }
    // 選挙のルールは open の時点で固定する（原則19）。固定した後に設定を変えても、保存されている値を使う。
    if let Some(frozen) = election_snapshot.rules
        && frozen != config.rules
    {
        tracing::warn!(
            configured = ?config.rules,
            stored = ?frozen,
            "設定ファイルの選挙のルール（vote.*）と、open の時点で固定したルールが異なります。固定した値を使います"
        );
    }

    // 認証と名簿。stub は入力 ID を採用し、名簿は voters.csv（メモリのキャッシュ）。db は、事前登録した
    // ログイン ID とパスワードで認証し、名簿は DB（voter_roll）から、リクエストごとに引く（状態を持たない）。
    let (auth, roll): (Arc<dyn Authenticator>, Arc<dyn VoterRoll>) =
        match (config.auth_mode, scylla) {
            (AuthMode::Db, Some(store)) => (
                Arc::new(
                    DbAuthenticator::new(
                        store.clone(),
                        Arc::new(BlockingChecker),
                        &config.password_params,
                    )
                    .context("auth.argon2.* が不正です")?,
                ),
                store,
            ),
            (AuthMode::Db, None) => anyhow::bail!("auth.mode=db には app.mode=db が必要です"),
            (AuthMode::Stub, _) => (
                Arc::new(StubAuthenticator),
                Arc::new(StaticVoterRoll::new(stub_roll.unwrap_or_default())),
            ),
        };

    let election = Arc::new(election);
    let voting = VotingService::new(
        Arc::new(StaticElectionRepository::new(Election::clone(&election))),
        roll,
        votes,
        Arc::new(RandomBallotIds),
        config.shard_count,
    );
    let election_gate = ElectionGate::new(election_state.clone(), config.state_cache_secs);
    let state = Arc::new(AppState {
        auth,
        sessions,
        voting,
        clock,
        chains,
        election,
        shard_count: config.shard_count,
        reveal: config.reveal,
        labels: config.labels.clone(),
        election_state,
        election_gate,
        configured_rules: config.rules,
        display_timezone: config.display_timezone,
        admin_token: config.admin_token.clone(),
        request_timeout: config.request_timeout,
        #[cfg(feature = "dev-tools")]
        dev_store,
    });
    Ok(Built { state, sealer })
}
