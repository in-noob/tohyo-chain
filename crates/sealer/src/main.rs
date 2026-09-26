//! 独立した sealer プロセス。リース（TTL 付きの LWT）を取ったシャードだけを封印し、アンカーを作る。
//!
//! 複数のプロセスを起動でき、1 つが落ちても、残りがリースの期限切れ後にそのシャードを引き継ぐ。
//! SIGTERM / SIGINT では、票をフラッシュせずに（原則9。残りの封印は締切の手続きの中でだけ行う）、
//! アンカー担当なら最終アンカーを済ませ、リースを解放してから終了する。

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use application::{ElectionStateStore, LeaseStore, RevoteKeyVault, SealStore, SystemClock};
use domain::Ed25519Signer;
use infra_scylla::{ScyllaConfig, ScyllaStore};
use sealer::config::SealerConfig;
use sealer::{Coordinator, DEFAULT_TICK, LeaseConfig, Sealer, SystemMonotonic, spawn_coordinator};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // 端末以外（ファイル・パイプ）では ANSI 装飾を付けない（ログを grep で検査できるように）。
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let config = SealerConfig::load()?;
    // 選挙定義のハッシュ（ADR 0025）。既にあるチェーンのジェネシスの値や、DB に登録済みの値（先に起動した api / sealer が
    // 登録）と違えば、起動を拒否する。
    let election_hash = config.election_hash()?;
    let clock = Arc::new(SystemClock);
    let store = Arc::new(
        ScyllaStore::connect(
            &ScyllaConfig {
                nodes: config.nodes.clone(),
                keyspace: config.keyspace.clone(),
                shard_count: config.shard_count,
            },
            clock.clone(),
        )
        .await
        .context("ScyllaDB への接続に失敗しました")?,
    );

    let signer = Arc::new(Ed25519Signer::from_seed(&config.signing_seed));
    let sealer = Sealer::new(
        store.clone() as Arc<dyn SealStore>,
        signer,
        clock,
        Arc::new(SystemMonotonic::new()),
        config.policy,
        config.shard_count,
        election_hash,
    );
    // 別の署名鍵が既に登録されていれば、ここで起動を拒否する（1 本のチェーンに別の鍵の署名を混ぜない）。
    sealer.register_signer().await.context(
        "署名鍵の登録に失敗しました（別の sealer.signing_seed で作られたチェーンがある可能性があります）",
    )?;
    // 既にあるチェーンのジェネシスと照合してから、cluster_config に登録・照合する（先に登録すると、チェーンと食い違う値が
    // 残ってしまう）。
    sealer
        .check_existing_chains()
        .await
        .context("選挙定義の照合に失敗しました")?;
    store
        .ensure_election_hash(&election_hash)
        .await
        .context("選挙定義の照合に失敗しました")?;

    // 選挙状態（原則17）。init（スキーマを作った後の最初の接続）で、設定の期間を取り込む。
    // その後に設定ファイルの期間と DB の期間が違っていたら、警告して DB の値を使う。
    let election_state = store
        .ensure_initialized(config.period)
        .await
        .context("選挙状態の初期化に失敗しました")?;
    if election_state.period != config.period {
        tracing::warn!(
            configured = ?config.period,
            db = ?election_state.period,
            "設定ファイルの投票期間と DB の期間が異なります。DB の値を使います"
        );
    }
    // 選挙のルールは open の時点で固定する（原則19）。固定した後に設定を変えても、DB の値を使う。
    if let Some(frozen) = election_state.rules
        && frozen != config.rules
    {
        tracing::warn!(
            configured = ?config.rules,
            db = ?frozen,
            "設定ファイルの選挙のルール（vote.*）と、open の時点で固定したルールが異なります。固定した値を使います"
        );
    }

    // 再投票の鍵（secrets/revote_key）。締切の手続きの中で、ファイルごと破棄する（ADR 0022）。sealer は slot を計算しない。
    let revote_keys = Arc::new(match &config.revote_key_path {
        Some(path) => RevoteKeyVault::load(path).context("secrets/revote_key を読めません")?,
        None => RevoteKeyVault::none(),
    });
    let coordinator = Coordinator::new(
        sealer,
        store.clone() as Arc<dyn LeaseStore>,
        LeaseConfig {
            owner: config.sealer_id.clone(),
            ttl: config.lease_ttl,
        },
        // アンカーは seal.interval_secs ごとに、作るかどうかを判定する（既定 600 秒 = 10 分）。
        // 直前のアンカー以降にどのシャードの先頭ブロックも変わっていなければ、作らない。
        Duration::from_secs(config.policy.interval_secs()),
        store as Arc<dyn ElectionStateStore>,
        config.election_grace,
        config.rules,
    )
    .with_revote_keys(revote_keys);
    tracing::info!(
        sealer_id = %config.sealer_id,
        shards = config.shard_count.get(),
        lease_ttl_secs = config.lease_ttl.as_secs(),
        max_ballots = config.policy.max_ballots(),
        interval_secs = config.policy.interval_secs(),
        min_ballots_after_interval = config.policy.min_ballots_after_interval(),
        election_hash = %shared_types::hex::encode(&election_hash),
        "sealer を起動しました"
    );
    let handle = spawn_coordinator(coordinator, DEFAULT_TICK);

    wait_for_shutdown_signal().await;
    tracing::info!("停止シグナルを受信しました");
    handle
        .shutdown()
        .await
        .context("sealer の停止に失敗しました")?;
    tracing::info!("停止しました");
    Ok(())
}

/// SIGTERM または Ctrl-C（SIGINT）を待つ。
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => log_signal_error(result),
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::error!(error = %e, "SIGTERM ハンドラを登録できませんでした"),
        }
    }
    log_signal_error(tokio::signal::ctrl_c().await);
}

// シグナル待ち自体の失敗はログに残し、停止扱いにする。
fn log_signal_error(result: std::io::Result<()>) {
    if let Err(e) = result {
        tracing::error!(error = %e, "シグナル待機に失敗しました");
    }
}
