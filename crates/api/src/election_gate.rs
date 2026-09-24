//! 選挙状態の短期キャッシュ（`election.state_cache_secs`。原則17・18）。
//!
//! 投票の受け付けは、リクエストごとに DB を読むと負荷が大きいので、短時間だけキャッシュする。
//! 締切の手続きの待ち時間（`state_cache_secs + api.request_timeout_secs`）は、この最大の
//! 古さを考慮して決めている（ADR 0019）。
//!
//! 選挙状態が closing 以降になったのを読んだら、このプロセスのメモリ上の再投票の鍵を捨てる（ADR 0022。ファイルの
//! 破棄は、締切の手続きを行うプロセスが行う）。

use std::sync::{Arc, Mutex, PoisonError};

use application::{ElectionStateSnapshot, ElectionStateStore, RevoteKeyVault, StoreError};
use domain::ElectionPhase;

pub struct ElectionGate {
    store: Arc<dyn ElectionStateStore>,
    cache_secs: u64,
    cached: Mutex<Option<(ElectionStateSnapshot, u64)>>,
    revote_keys: Option<Arc<RevoteKeyVault>>,
}

impl ElectionGate {
    pub fn new(store: Arc<dyn ElectionStateStore>, cache_secs: u64) -> Self {
        Self {
            store,
            cache_secs,
            cached: Mutex::new(None),
            revote_keys: None,
        }
    }

    /// closing 以降を読んだら捨てる、再投票の鍵。
    pub fn with_revote_keys(mut self, revote_keys: Arc<RevoteKeyVault>) -> Self {
        self.revote_keys = Some(revote_keys);
        self
    }

    /// `now`（UNIX 秒）時点のスナップショット。キャッシュが `cache_secs` 以内なら、それを返す。
    pub async fn snapshot(&self, now: u64) -> Result<ElectionStateSnapshot, StoreError> {
        if let Some(fresh) = self.fresh_cached(now) {
            return Ok(fresh);
        }
        let snapshot = self.store.get().await?;
        if snapshot.phase >= ElectionPhase::Closing
            && let Some(keys) = &self.revote_keys
            && keys.has_key()
        {
            keys.forget();
            tracing::info!("締切の手続きに入ったので、メモリ上の再投票の鍵を捨てました");
        }
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some((snapshot.clone(), now));
        Ok(snapshot)
    }

    fn fresh_cached(&self, now: u64) -> Option<ElectionStateSnapshot> {
        let cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let (snapshot, at) = cached.as_ref()?;
        (now.saturating_sub(*at) < self.cache_secs).then(|| snapshot.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use domain::Period;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingStore {
        calls: AtomicUsize,
        phase: ElectionPhase,
    }

    #[async_trait]
    impl ElectionStateStore for CountingStore {
        async fn ensure_initialized(
            &self,
            period: Period,
        ) -> Result<ElectionStateSnapshot, StoreError> {
            Ok(ElectionStateSnapshot {
                phase: ElectionPhase::Scheduled,
                period,
                opened_at: None,
                closing_started_at: None,
                rules: None,
            })
        }

        async fn get(&self) -> Result<ElectionStateSnapshot, StoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ElectionStateSnapshot {
                phase: self.phase,
                period: Period::default(),
                opened_at: None,
                closing_started_at: None,
                rules: None,
            })
        }

        async fn schedule(&self, _period: Period, _at_unix_secs: i64) -> Result<bool, StoreError> {
            Ok(true)
        }

        async fn transition(
            &self,
            _from: ElectionPhase,
            _to: ElectionPhase,
            _rules: domain::ElectionRules,
            _actor: &str,
            _at_unix_secs: i64,
        ) -> Result<bool, StoreError> {
            Ok(true)
        }

        async fn record_event(
            &self,
            _event: application::AuditEvent,
            _phase: ElectionPhase,
            _actor: &str,
            _at_unix_secs: i64,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn recent_audit(
            &self,
            _limit: usize,
        ) -> Result<Vec<application::ElectionAuditEntry>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn refetches_only_after_the_cache_expires() {
        let store = Arc::new(CountingStore {
            calls: AtomicUsize::new(0),
            phase: ElectionPhase::Open,
        });
        let gate = ElectionGate::new(store.clone(), 5);

        gate.snapshot(100).await.expect("first read");
        assert_eq!(store.calls.load(Ordering::SeqCst), 1);
        gate.snapshot(104).await.expect("cached read");
        assert_eq!(
            store.calls.load(Ordering::SeqCst),
            1,
            "5 秒未満はキャッシュを使う"
        );
        gate.snapshot(105).await.expect("refetch");
        assert_eq!(store.calls.load(Ordering::SeqCst), 2, "5 秒経てば読み直す");
    }

    #[tokio::test]
    async fn the_in_memory_revote_key_is_dropped_once_closing_is_seen() {
        let keys = Arc::new(RevoteKeyVault::in_memory(Some(
            application::RevoteKey::new([1; 32]),
        )));
        let open = ElectionGate::new(
            Arc::new(CountingStore {
                calls: AtomicUsize::new(0),
                phase: ElectionPhase::Open,
            }),
            0,
        )
        .with_revote_keys(keys.clone());
        open.snapshot(1).await.expect("read");
        assert!(keys.has_key(), "open の間は鍵を使う");
        let closing = ElectionGate::new(
            Arc::new(CountingStore {
                calls: AtomicUsize::new(0),
                phase: ElectionPhase::Closing,
            }),
            0,
        )
        .with_revote_keys(keys.clone());
        closing.snapshot(1).await.expect("read");
        assert!(!keys.has_key());
    }
}
