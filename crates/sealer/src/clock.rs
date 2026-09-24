//! 単調時計。封印窓の経過時間を、壁時計の巻き戻り（NTP 補正など）の影響を受けずに測る。

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub trait MonotonicClock: Send + Sync {
    /// 固定の起点からの経過時間（単調非減少）。
    fn elapsed(&self) -> Duration;
}

/// `Instant` ベースの実時計。生成時点が起点。
#[derive(Debug, Clone, Copy)]
pub struct SystemMonotonic {
    start: Instant,
}

impl SystemMonotonic {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemMonotonic {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for SystemMonotonic {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// 手動で進める時計。sleep なしで決定的にテストするために使う。
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Mutex<Duration>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }
}

impl MonotonicClock for ManualClock {
    fn elapsed(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
