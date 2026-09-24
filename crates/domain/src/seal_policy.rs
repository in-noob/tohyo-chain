//! ブロック封印の判定ロジック（CLAUDE.md 原則9）。
//!
//! すべて純粋関数で、時刻は引数で受け取る。時計・IO・状態は持たない。
//! 時刻は単調増加する秒カウンタ（`u64`）なら何でもよい。
//!
//! 窓（前回の封印時刻からの区間）の管理は呼び出し側（sealer）の責務だが、
//! 判定後の状態更新は [`SealDecision::next_window_start`] と
//! [`SealDecision::pending_after`] で純粋関数として提供する。
//!
//! ルール（シャードごとに独立）:
//! 1. 未封印が `max_ballots` 件に達したら、到着順に `max_ballots` 件で即封印し窓をリセット。
//!    超過分は繰り越す。
//! 2. 窓の開始から `max_interval_secs` 秒経過したとき、未封印が 1 件以上なら全件封印して
//!    窓をリセット。0 件ならブロックを作らず窓だけリセット（到着時刻の漏洩を防ぐ）。
//! 3. 選挙締切・sealer 正常停止時は [`decide_flush`] で残りをすべて封印する。

/// 既定の 1 ブロックあたり最大件数（`seal.max_ballots`）。
pub const DEFAULT_MAX_BALLOTS: usize = 100;
/// 既定の窓の最大秒数（`seal.max_interval_secs`）。
pub const DEFAULT_MAX_INTERVAL_SECS: u64 = 600;

/// 封印ポリシーの設定値が不正。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("max_ballots は 1 以上でなければなりません")]
    ZeroMaxBallots,
    #[error("max_interval_secs は 1 以上でなければなりません")]
    ZeroMaxInterval,
}

/// 封印ポリシー。0 は無限ループや常時封印を招くため、`new` で拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealPolicy {
    max_ballots: usize,
    max_interval_secs: u64,
}

impl SealPolicy {
    pub fn new(max_ballots: usize, max_interval_secs: u64) -> Result<Self, PolicyError> {
        if max_ballots == 0 {
            return Err(PolicyError::ZeroMaxBallots);
        }
        if max_interval_secs == 0 {
            return Err(PolicyError::ZeroMaxInterval);
        }
        Ok(Self {
            max_ballots,
            max_interval_secs,
        })
    }

    pub fn max_ballots(&self) -> usize {
        self.max_ballots
    }

    pub fn max_interval_secs(&self) -> u64 {
        self.max_interval_secs
    }
}

impl Default for SealPolicy {
    fn default() -> Self {
        Self {
            max_ballots: DEFAULT_MAX_BALLOTS,
            max_interval_secs: DEFAULT_MAX_INTERVAL_SECS,
        }
    }
}

/// 封印の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealDecision {
    /// まだ封印しない。窓もそのまま。
    Wait,
    /// 到着順の先頭 N 件で封印し、窓をリセットする。残りは繰り越す。
    SealCount(usize),
    /// 未封印の全件で封印し、窓をリセットする。
    SealAll,
    /// ブロックは作らず、窓だけリセットする（0 件で窓が満了した場合）。
    ResetWindowOnly,
}

impl SealDecision {
    /// この判定を実行した後の窓の開始時刻。`Wait` 以外は窓をリセットして `now` になる。
    pub fn next_window_start(self, window_start: u64, now: u64) -> u64 {
        match self {
            Self::Wait => window_start,
            Self::SealCount(_) | Self::SealAll | Self::ResetWindowOnly => now,
        }
    }

    /// この判定を実行した後の未封印件数。
    pub fn pending_after(self, pending: usize) -> usize {
        match self {
            Self::Wait => pending,
            Self::SealCount(n) => pending.saturating_sub(n),
            Self::SealAll | Self::ResetWindowOnly => 0,
        }
    }
}

/// 通常運転時の封印判定。
///
/// - `pending`: 未封印の票の件数
/// - `window_start`: 現在の窓の開始時刻（秒）
/// - `now`: 現在時刻（秒）。`window_start` より前（時計の巻き戻り）なら経過 0 として扱う
pub fn decide(pending: usize, window_start: u64, now: u64, policy: &SealPolicy) -> SealDecision {
    // 件数到達が最優先。時間切れと同時でも、到着順に max_ballots 件だけ封印する。
    if pending >= policy.max_ballots {
        return SealDecision::SealCount(policy.max_ballots);
    }
    let elapsed = now.saturating_sub(window_start);
    if elapsed >= policy.max_interval_secs {
        return if pending == 0 {
            SealDecision::ResetWindowOnly
        } else {
            SealDecision::SealAll
        };
    }
    SealDecision::Wait
}

/// 選挙締切・sealer 正常停止時の判定。件数や経過時間に関係なく残りをすべて封印する。
/// 0 件なら空ブロックは作らない（`ResetWindowOnly`）。
pub fn decide_flush(pending: usize) -> SealDecision {
    if pending == 0 {
        SealDecision::ResetWindowOnly
    } else {
        SealDecision::SealAll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SealDecision::{ResetWindowOnly, SealAll, SealCount, Wait};

    const P: SealPolicy = SealPolicy {
        max_ballots: DEFAULT_MAX_BALLOTS,
        max_interval_secs: DEFAULT_MAX_INTERVAL_SECS,
    };

    /// 判定を連続適用するための、呼び出し側（sealer）相当の状態。
    struct Sim {
        pending: usize,
        window_start: u64,
    }

    impl Sim {
        fn step(&mut self, now: u64) -> SealDecision {
            let d = decide(self.pending, self.window_start, now, &P);
            self.window_start = d.next_window_start(self.window_start, now);
            self.pending = d.pending_after(self.pending);
            d
        }
    }

    // --- 依頼された必須ケース ---

    #[test]
    fn case_99_ballots_at_9m59s_waits() {
        assert_eq!(decide(99, 0, 9 * 60 + 59, &P), Wait);
    }

    #[test]
    fn case_100_ballots_at_1s_seals_count_100() {
        assert_eq!(decide(100, 0, 1, &P), SealCount(100));
    }

    #[test]
    fn case_250_ballots_seal_100_100_then_wait_for_window_expiry() {
        let mut sim = Sim {
            pending: 250,
            window_start: 1_000,
        };
        assert_eq!(sim.step(1_000), SealCount(100));
        assert_eq!(sim.pending, 150);
        assert_eq!(sim.step(1_000), SealCount(100));
        assert_eq!(sim.pending, 50);
        // 残り 50 件は、窓（直前の封印時刻 1_000）が満了するまで待つ。
        assert_eq!(sim.step(1_000), Wait);
        assert_eq!(sim.step(1_000 + 599), Wait);
        assert_eq!(sim.step(1_000 + 600), SealAll);
        assert_eq!(sim.pending, 0);
    }

    #[test]
    fn case_1_ballot_at_exactly_600s_seals_all() {
        assert_eq!(decide(1, 0, 600, &P), SealAll);
    }

    #[test]
    fn case_0_ballots_at_600s_resets_window_only() {
        assert_eq!(decide(0, 0, 600, &P), ResetWindowOnly);
    }

    #[test]
    fn case_flush_37_ballots_seals_all() {
        assert_eq!(decide_flush(37), SealAll);
    }

    #[test]
    fn case_elapsed_is_recomputed_from_the_reset_window() {
        // 窓 0 で 100 件到達 → 100 秒時点で封印、新しい窓は 100 から始まる。
        let d = decide(100, 0, 100, &P);
        assert_eq!(d, SealCount(100));
        let new_start = d.next_window_start(0, 100);
        assert_eq!(new_start, 100);

        // 新しい窓の起点（100）から測るので、699 秒（経過 599）はまだ待つ。
        assert_eq!(decide(1, new_start, 699, &P), Wait);
        assert_eq!(decide(1, new_start, 700, &P), SealAll);
        // 古い起点（0）のままなら 699 秒で満了してしまう（これが誤り）。
        assert_eq!(decide(1, 0, 699, &P), SealAll);
    }

    #[test]
    fn empty_window_reset_also_restarts_the_clock() {
        let d = decide(0, 0, 600, &P);
        let new_start = d.next_window_start(0, 600);
        assert_eq!(new_start, 600);
        assert_eq!(decide(1, new_start, 1_199, &P), Wait);
        assert_eq!(decide(1, new_start, 1_200, &P), SealAll);
    }

    // --- 境界・補足 ---

    #[test]
    fn interval_boundary_is_inclusive() {
        assert_eq!(decide(1, 0, 599, &P), Wait);
        assert_eq!(decide(1, 0, 600, &P), SealAll);
        assert_eq!(decide(1, 0, 601, &P), SealAll);
    }

    #[test]
    fn zero_pending_before_expiry_waits() {
        assert_eq!(decide(0, 0, 0, &P), Wait);
        assert_eq!(decide(0, 0, 599, &P), Wait);
    }

    #[test]
    fn count_limit_takes_priority_over_expiry() {
        assert_eq!(decide(250, 0, 600, &P), SealCount(100));
        assert_eq!(decide(100, 0, 10_000, &P), SealCount(100));
    }

    #[test]
    fn count_boundary() {
        assert_eq!(decide(99, 0, 0, &P), Wait);
        assert_eq!(decide(100, 0, 0, &P), SealCount(100));
        assert_eq!(decide(101, 0, 0, &P), SealCount(100));
    }

    #[test]
    fn clock_going_backwards_counts_as_zero_elapsed() {
        assert_eq!(decide(1, 1_000, 500, &P), Wait);
        assert_eq!(decide(0, u64::MAX, 0, &P), Wait);
    }

    #[test]
    fn flush_with_zero_pending_makes_no_block() {
        assert_eq!(decide_flush(0), ResetWindowOnly);
        assert_eq!(decide_flush(1), SealAll);
        assert_eq!(decide_flush(100), SealAll);
        assert_eq!(decide_flush(10_000), SealAll);
    }

    #[test]
    fn next_window_start_per_decision() {
        assert_eq!(Wait.next_window_start(5, 9), 5);
        assert_eq!(SealCount(100).next_window_start(5, 9), 9);
        assert_eq!(SealAll.next_window_start(5, 9), 9);
        assert_eq!(ResetWindowOnly.next_window_start(5, 9), 9);
    }

    #[test]
    fn pending_after_per_decision() {
        assert_eq!(Wait.pending_after(42), 42);
        assert_eq!(SealCount(100).pending_after(250), 150);
        assert_eq!(SealCount(100).pending_after(100), 0);
        assert_eq!(SealAll.pending_after(42), 0);
        assert_eq!(ResetWindowOnly.pending_after(0), 0);
    }

    #[test]
    fn policy_rejects_zero_values() {
        assert_eq!(SealPolicy::new(0, 600), Err(PolicyError::ZeroMaxBallots));
        assert_eq!(SealPolicy::new(100, 0), Err(PolicyError::ZeroMaxInterval));
        let p = SealPolicy::new(3, 7).expect("valid policy");
        assert_eq!((p.max_ballots(), p.max_interval_secs()), (3, 7));
    }

    #[test]
    fn default_policy_is_100_ballots_600_secs() {
        let p = SealPolicy::default();
        assert_eq!((p.max_ballots(), p.max_interval_secs()), (100, 600));
    }

    #[test]
    fn custom_policy_is_respected() {
        let p = SealPolicy::new(3, 10).expect("valid policy");
        assert_eq!(decide(3, 0, 0, &p), SealCount(3));
        assert_eq!(decide(2, 0, 9, &p), Wait);
        assert_eq!(decide(2, 0, 10, &p), SealAll);
    }

    #[test]
    fn one_arrival_per_second_seals_every_ballot_exactly_once() {
        let mut sim = Sim {
            pending: 0,
            window_start: 0,
        };
        let mut sealed = Vec::new();
        let mut record = |d: SealDecision, before: usize| match d {
            SealCount(n) => sealed.push(n),
            SealAll => sealed.push(before),
            Wait | ResetWindowOnly => {}
        };

        // 250 件が 1 秒に 1 件ずつ到着する。
        for t in 1..=250u64 {
            sim.pending += 1;
            let before = sim.pending;
            let d = sim.step(t);
            record(d, before);
        }
        // 到着が止んだ後、時間だけ進めて残りを封印させる。
        for t in 251..=2_000u64 {
            let before = sim.pending;
            let d = sim.step(t);
            record(d, before);
        }

        assert_eq!(sealed, vec![100, 100, 50]);
        assert_eq!(sim.pending, 0);
    }
}
