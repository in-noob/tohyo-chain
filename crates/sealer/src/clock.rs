//! 単調時計。リースの有効期限・アンカーの周期・締切の待ち時間を、壁時計の巻き戻り（NTP 補正など）の
//! 影響を受けずに測る（封印の経過時間は、DB の投票開始時刻と比べるため、壁時計で測る。ADR 0020）。

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use application::Clock;

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
///
/// 単調時計（[`MonotonicClock`]）としても、壁時計（`application::Clock`。`wall_base` + 経過秒）としても使える。
/// 両方に同じ時計を渡すと、`advance` で両方が同じだけ進む。
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Mutex<Duration>,
    wall_base: u64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// 壁時計として使うときの起点（UNIX 秒）を指定する。
    pub fn with_wall_base(wall_base: u64) -> Self {
        Self {
            now: Mutex::default(),
            wall_base,
        }
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

impl Clock for ManualClock {
    fn now_unix_secs(&self) -> u64 {
        self.wall_base.saturating_add(self.elapsed().as_secs())
    }
}
