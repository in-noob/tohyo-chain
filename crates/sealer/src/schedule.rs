//! アンカー作成の周期管理（時計は引数で受け取る純粋なロジック）。

use std::time::Duration;

/// 一定間隔ごとに「今作る時期か」を返す。
#[derive(Debug, Clone, Copy)]
pub struct AnchorSchedule {
    interval: Duration,
    next_due: Option<Duration>,
}

impl AnchorSchedule {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_due: None,
        }
    }

    /// 次の期限を `now` から数え直す（担当になった直後など）。
    pub fn restart(&mut self, now: Duration) {
        self.next_due = Some(now + self.interval);
    }

    /// 今が作る時期なら `true` にして、次の期限を進める。最初の呼び出しは期限を決めるだけで `false`。
    pub fn due(&mut self, now: Duration) -> bool {
        match self.next_due {
            None => {
                self.restart(now);
                false
            }
            Some(due) if now >= due => {
                self.restart(now);
                true
            }
            Some(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn first_call_only_starts_the_clock() {
        let mut schedule = AnchorSchedule::new(S(10));
        assert!(!schedule.due(S(100)));
        assert!(!schedule.due(S(109)));
        assert!(schedule.due(S(110)));
    }

    #[test]
    fn fires_once_per_interval_counted_from_the_last_firing() {
        let mut schedule = AnchorSchedule::new(S(10));
        schedule.restart(S(0));
        assert!(!schedule.due(S(9)));
        assert!(schedule.due(S(12))); // 遅れて呼ばれても 1 回だけ
        assert!(!schedule.due(S(12)));
        assert!(!schedule.due(S(21)));
        assert!(schedule.due(S(22)));
    }

    #[test]
    fn restart_moves_the_deadline() {
        let mut schedule = AnchorSchedule::new(S(10));
        schedule.restart(S(0));
        schedule.restart(S(50));
        assert!(!schedule.due(S(59)));
        assert!(schedule.due(S(60)));
    }
}
