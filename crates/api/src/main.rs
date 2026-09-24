//! API サーバの起動。sealer は保存先（memory / scylla）にかかわらず、同じプロセス内のタスクとして動かす。
//!
//! 公開用のポート（`api.port`）と、管理用のリスナー（`admin.bind`。既定 `127.0.0.1:18081`）を、
//! 別々の `TcpListener` で立てる（原則17: 公開用のポートに管理用のエンドポイントを置かない）。
//! `admin.token` が未設定なら、管理用リスナーは起動しない。

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use api::{Config, SystemClock, admin::admin_app, app, build};
use sealer::{DEFAULT_TICK, SystemMonotonic};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// 停止シグナル受信後、処理中のリクエストの完了を待つ上限。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=debug")),
        )
        // 端末以外（ファイル・パイプ）では ANSI 装飾を付けない。フィールド名に制御文字が
        // 混ざると、ログを grep で検査できなくなる。
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let config = Config::load()?;
    let built = build(
        &config,
        Arc::new(SystemClock),
        Arc::new(SystemMonotonic::new()),
    )
    .await?;
    // app.mode=memory のときだけ、sealer を同じプロセス内のタスクとして動かす（アンカーも、変化があったときだけ作る）。
    // app.mode=db では、独立した sealer プロセスが封印する。選挙状態（原則17）の自動遷移・締切の手続きも、
    // memory モードではこの同じタスクが行う。
    let sealer = built.sealer.map(|sealer| {
        sealer::spawn(
            sealer,
            DEFAULT_TICK,
            Duration::from_secs(config.seal_policy.max_interval_secs()),
            built.state.election_state.clone(),
            config.election_grace,
        )
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("{addr} に bind できません"))?;
    tracing::info!(
        %addr,
        shards = config.shard_count.get(),
        max_ballots = config.seal_policy.max_ballots(),
        max_interval_secs = config.seal_policy.max_interval_secs(),
        "api を起動しました"
    );
    #[cfg(feature = "dev-tools")]
    tracing::warn!("dev-tools が有効です（/debug/pool と /debug/tamper を公開しています）");

    // 管理用リスナー。トークン未設定なら起動しない（管理操作を使えない設定は明示的に許す。開発の既定は無効）。
    let (admin_stop_tx, admin_stop_rx) = watch::channel(false);
    let admin_task = if built.state.admin_token.is_some() {
        let admin_listener = tokio::net::TcpListener::bind(&config.admin_bind)
            .await
            .with_context(|| format!("管理用リスナー {} に bind できません", config.admin_bind))?;
        tracing::info!(addr = %config.admin_bind, "管理用リスナーを起動しました");
        let router = admin_app(built.state.clone());
        let mut stop = admin_stop_rx;
        Some(tokio::spawn(async move {
            let result = axum::serve(admin_listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stop.changed().await;
                })
                .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "管理用リスナーが異常終了しました");
            }
        }))
    } else {
        tracing::warn!(
            "admin.token が未設定のため、管理用リスナーは起動しません（scripts/election.sh は使えません）"
        );
        None
    };

    // 停止シグナルの受信を、猶予タイマーと管理用リスナーにも伝える。
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app(built.state)).with_graceful_shutdown(async move {
        wait_for_shutdown_signal().await;
        tracing::info!(
            "停止シグナルを受信しました。新規接続を止めて、処理中のリクエストを完了します"
        );
        let _ = admin_stop_tx.send(true);
        // 受信側が既に無くても構わない。
        let _ = signalled_tx.send(());
    });
    let grace_expired = async move {
        if signalled_rx.await.is_ok() {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    let served = tokio::select! {
        result = server => result.context("サーバが異常終了しました"),
        () = grace_expired => {
            tracing::warn!("猶予時間内にリクエストが完了しなかったため、強制的に続行します");
            Ok(())
        }
    };

    if let Some(admin_task) = admin_task {
        let _ = admin_task.await;
    }

    // HTTP の受付が止まった後で、プロセス内の sealer があれば、残りの票を締切フラッシュしてから終了する。
    // （サーバが異常終了した場合も、受理済みの票は必ず封印する。）
    if let Some(sealer) = sealer {
        sealer
            .shutdown()
            .await
            .context("sealer の停止に失敗しました")?;
    }
    tracing::info!("停止しました");
    served
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
