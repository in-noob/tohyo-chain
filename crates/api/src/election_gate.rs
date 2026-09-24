//! 選挙状態の短期キャッシュ（`election.state_cache_secs`。原則17・18）。
//!
//! 投票の受け付けは、リクエストごとに DB を読むと負荷が大きいので、短時間だけキャッシュする。
//! 締切の手続きの待ち時間（`state_cache_secs + api.request_timeout_secs`）は、この最大の
//! 古さを考慮して決めている（ADR 0019）。

use std::sync::{Mutex, PoisonError};

use application::{ElectionStateSnapshot, ElectionStateStore, StoreError};

pub struct ElectionGate {
    store: std::sync::Arc<dyn ElectionStateStore>,
    cache_secs: u64,
    cached: Mutex<Option<(ElectionStateSnapshot, u64)>>,
}

impl ElectionGate {
    pub fn new(store: std::sync::Arc<dyn ElectionStateStore>, cache_secs: u64) -> Self {
        Self {
            store,
            cache_secs,
            cached: Mutex::new(None),
        }
    }

    /// `now`（UNIX 秒）時点のスナップショット。キャッシュが `cache_secs` 以内なら、それを返す。
    pub async fn snapshot(&self, now: u64) -> Result<ElectionStateSnapshot, StoreError> {
        if let Some(fresh) = self.fresh_cached(now) {
            return Ok(fresh);
        }
        let snapshot = self.store.get().await?;
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
    use domain::{ElectionPhase, Period};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingStore {
        calls: AtomicUsize,
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
                phase: ElectionPhase::Open,
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
}
