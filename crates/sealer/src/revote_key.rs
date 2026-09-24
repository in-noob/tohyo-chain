//! 締切の手続きの中での、再投票の鍵（`revote_key`）の破棄（CLAUDE.md 原則1・ADR 0022）。
//!
//! 締切の手続き（closing）で、投票の受け付けが確実に止まった後（待ち時間 `election_grace` の後）に、鍵をファイルごと
//! 消し、`election_audit` に `revote_key_destroyed` を 1 行記録する。これで、締切後は slot から投票者 ID に戻せなくなる。
//! 破棄に失敗したら、closed には進めない（次の周期で再試行する）。

use application::{AuditEvent, ElectionStateStore, KeyDestroyed, RevoteKeyVault, StoreError};
use domain::{ElectionPhase, ElectionRules};

/// 記録済みかを調べる、監査ログの件数（新しい順）。締切の手続きの中の記録は、直近の数件に必ず入っている。
const AUDIT_LOOKBACK: usize = 50;

/// 鍵の破棄の失敗。
#[derive(Debug, thiserror::Error)]
pub enum KeyDestroyError {
    #[error("revote_key のファイルを消せません: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// 鍵を破棄し、必要なら `election_audit` に記録する（冪等）。
///
/// 記録するのは、「このプロセスが鍵のファイルを消した」か「再投票を認める選挙（固定したルール）」のときで、まだ記録が
/// 無いときだけ（再試行・複数の sealer で、同じ記録を重ねない）。再投票を認めない選挙で、鍵のファイルも無ければ、
/// 記録しない（破棄するものが無い）。
pub async fn destroy_revote_key(
    vault: &RevoteKeyVault,
    election: &dyn ElectionStateStore,
    rules: ElectionRules,
    phase: ElectionPhase,
    actor: &str,
    at_unix_secs: i64,
) -> Result<KeyDestroyed, KeyDestroyError> {
    let destroyed = vault.destroy()?;
    if destroyed == KeyDestroyed::AlreadyAbsent && !rules.allow_revote {
        return Ok(destroyed);
    }
    let recorded = election
        .recent_audit(AUDIT_LOOKBACK)
        .await?
        .iter()
        .any(|entry| entry.event == AuditEvent::RevoteKeyDestroyed);
    if !recorded {
        election
            .record_event(AuditEvent::RevoteKeyDestroyed, phase, actor, at_unix_secs)
            .await?;
        tracing::info!(
            removed_file = destroyed == KeyDestroyed::Removed,
            "再投票の鍵（revote_key）を破棄しました"
        );
    }
    Ok(destroyed)
}
