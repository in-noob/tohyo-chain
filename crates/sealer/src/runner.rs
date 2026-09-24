//! `Sealer` を tokio タスクとして周期実行し、停止時にフラッシュする。

use std::sync::Arc;
use std::time::Duration;

use application::{ElectionStateStore, StoreError};
use domain::{ElectionPhase, automatic_transition};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

use crate::coordinator::Coordinator;
use crate::schedule::AnchorSchedule;
use crate::sealer::Sealer;

/// 判定の周期。`seal.max_ballots` 到達の検知遅延の上限になる。
pub const DEFAULT_TICK: Duration = Duration::from_millis(200);

/// 実行中の sealer タスクへのハンドル。
#[derive(Debug)]
pub struct SealerHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl SealerHandle {
    /// 停止を指示し、締切フラッシュ（残りをすべて封印）が終わるまで待つ。
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        // 受信側（タスク）が既に終わっていても構わない。
        let _ = self.stop.send(true);
        self.task.await
    }
}

/// `sealer` をバックグラウンドタスクとして起動する（`init` 済みであること）。リースは使わず、全シャードを
/// このプロセスが担当する（`app.mode=memory` のプロセス内 sealer 用）。
///
/// `anchor_interval` ごとに、アンカーを作るかどうかを判定する（変化がなければ作らない）。ハンドルが `shutdown` されるか、破棄されて送信側が閉じると、
/// フラッシュしてから終了する。
///
/// `election` / `election_grace`: 選挙状態（scheduled → open → closing → closed）の自動遷移と
/// 締切の手続きを、このプロセス内のスケジューラが行う（原則17。memory モードにはリース・複数プロセスが
/// 無いので、Coordinator の「アンカー担当」に相当する役割を、この唯一の sealer タスクがそのまま担う）。
pub fn spawn(
    mut sealer: Sealer,
    tick: Duration,
    anchor_interval: Duration,
    election: Arc<dyn ElectionStateStore>,
    election_grace: Duration,
) -> SealerHandle {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut timer = interval(tick);
        // 処理が遅れたときに、取りこぼした tick をまとめて追いかけない。
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut anchors = AnchorSchedule::new(anchor_interval);
        let mut closing_deadline: Option<Duration> = None;
        loop {
            tokio::select! {
                _ = timer.tick() => {
                    // 失敗は tick 内でログ済み。次の周期で再試行する。
                    sealer.tick().await;
                    if anchors.due(sealer.now())
                        && let Err(e) = sealer.anchor().await
                    {
                        tracing::error!(error = %e, "アンカーの作成に失敗しました");
                    }
                    election_tick(&mut sealer, &election, election_grace, &mut closing_deadline).await;
                }
                // 停止指示、または送信側の破棄。
                _ = stopped.changed() => break,
            }
        }
        tracing::info!("停止を受け付けました。残りの票をフラッシュします");
        sealer.flush().await;
        // 最終アンカー: 変化がなければ、最後のアンカーが最新の状態を指していることを確認するだけ（作らない）。
        if let Err(e) = sealer.finalize_anchor().await {
            tracing::error!(error = %e, "最終アンカーの確認に失敗しました");
        }
    });
    SealerHandle { stop, task }
}

/// memory モードの選挙状態の遷移（原則17）。`spawn` のループから、tick ごとに呼ぶ。
async fn election_tick(
    sealer: &mut Sealer,
    election: &Arc<dyn ElectionStateStore>,
    grace: Duration,
    closing_deadline: &mut Option<Duration>,
) {
    let snapshot = match election.get().await {
        Ok(snapshot) => snapshot,
        // 未初期化（`ensure_initialized` が呼ばれていない）: この選挙は状態機械を使わない。
        Err(StoreError::Unavailable) => return,
        Err(e) => {
            tracing::warn!(error = %e, "選挙状態の取得に失敗しました");
            return;
        }
    };
    let wall_now = i64::try_from(sealer.wall_now_unix_secs()).unwrap_or(i64::MAX);
    const ACTOR: &str = "sealer:memory";

    match snapshot.phase {
        ElectionPhase::Scheduled | ElectionPhase::Open => {
            *closing_deadline = None;
            let Some(next) = automatic_transition(snapshot.phase, snapshot.period, wall_now) else {
                return;
            };
            match election
                .transition(snapshot.phase, next, ACTOR, wall_now)
                .await
            {
                Ok(true) => {
                    tracing::info!(from = %snapshot.phase, to = %next, "選挙状態を自動で遷移しました");
                    if next == ElectionPhase::Closing {
                        *closing_deadline = Some(sealer.now() + grace);
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "選挙状態の遷移に失敗しました"),
            }
        }
        ElectionPhase::Closing => {
            // 単一プロセスなので、シャードの担当という概念はなく、常に全シャードをフラッシュする。
            sealer.flush().await;
            let deadline = *closing_deadline.get_or_insert_with(|| sealer.now() + grace);
            if sealer.now() < deadline {
                return;
            }
            match sealer.total_pending().await {
                Ok(0) => {
                    if let Err(e) = sealer.finalize_anchor().await {
                        tracing::error!(error = %e, "最終アンカーの作成に失敗しました");
                        return;
                    }
                    match election
                        .transition(
                            ElectionPhase::Closing,
                            ElectionPhase::Closed,
                            ACTOR,
                            wall_now,
                        )
                        .await
                    {
                        Ok(true) => {
                            tracing::info!(
                                "選挙状態を closed にしました（締切の手続きが完了しました）"
                            );
                            *closing_deadline = None;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "選挙状態の closed への遷移に失敗しました")
                        }
                    }
                }
                Ok(pending) => {
                    tracing::debug!(pending, "締切の手続き: 未封印が残っているので待ちます");
                }
                Err(e) => tracing::warn!(error = %e, "未封印の件数の確認に失敗しました"),
            }
        }
        ElectionPhase::Closed => {}
    }
}

/// リースを使う sealer（`Coordinator`）をバックグラウンドタスクとして起動する。
///
/// 停止時は、保持しているシャードを締切フラッシュし、リースを解放してから終了する。
pub fn spawn_coordinator(mut coordinator: Coordinator, tick: Duration) -> SealerHandle {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut timer = interval(tick);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = timer.tick() => {
                    // 失敗・リース喪失は step 内でログ済み。
                    coordinator.step().await;
                }
                _ = stopped.changed() => break,
            }
        }
        tracing::info!(
            "停止を受け付けました。保持しているシャードをフラッシュしてリースを解放します"
        );
        coordinator.shutdown().await;
    });
    SealerHandle { stop, task }
}
