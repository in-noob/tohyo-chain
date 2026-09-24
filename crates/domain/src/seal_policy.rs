//! ブロック封印の判定ロジック（CLAUDE.md 原則9。ADR 0003・0020）。
//!
//! すべて純粋関数で、時刻は引数で受け取る。時計・IO・状態は持たない。
//! 時刻の単位は秒（呼び出し側の sealer は UNIX 秒を渡す）。
//!
//! ルール（シャードごとに独立して判定する）:
//! 1. 未封印が `max_ballots` 件に達したら、到着順に `max_ballots` 件ですぐに封印する（[`SealDecision::SealCount`]）。
//!    超過分は繰り越す。
//! 2. 経過時間の起点（[`window_start`] = max(前回の封印時刻, 投票開始時刻)）から `interval_secs` 秒以上経ち、
//!    かつ未封印が `min_ballots_after_interval` 件以上なら、全件を封印する（[`SealDecision::SealAll`]）。
//! 3. 上の 2 つに当てはまらなければ待つ（[`SealDecision::Wait`]）。0 件のときも待つ。ブロックを作らない判定で、
//!    起点（窓）を動かすことはない（起点が動くのは、封印したときだけ）。
//! 4. 投票終了（締切の手続き）の中でだけ、[`decide_close`] で残りを件数に関係なく封印する
//!    （[`SealDecision::CloseFlush`]）。1 ブロックが `max_ballots` 件を超えないよう、`max_ballots` 件以上
//!    残っている間は [`SealDecision::SealCount`] を返す。0 件なら何もしない。

/// 既定の 1 ブロックあたり最大件数（`seal.max_ballots`）。
pub const DEFAULT_MAX_BALLOTS: usize = 100;
/// 既定の、時間による封印の間隔（`seal.interval_secs`）。
pub const DEFAULT_INTERVAL_SECS: u64 = 600;
/// 既定の、時間による封印に必要な最小件数（`seal.min_ballots_after_interval`）。
pub const DEFAULT_MIN_BALLOTS_AFTER_INTERVAL: usize = 10;

/// 封印ポリシーの設定値が不正。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("max_ballots は 1 以上でなければなりません")]
    ZeroMaxBallots,
    #[error("interval_secs は 1 以上でなければなりません")]
    ZeroInterval,
    #[error("min_ballots_after_interval は 1 以上でなければなりません")]
    ZeroMinBallots,
}

/// 封印ポリシー。0 は、常時封印（空のブロック）や判定の無限ループを招くため、`new` で拒否する。
///
/// `min_ballots_after_interval > max_ballots` は拒否しない。その場合は、件数による封印が常に先に起きるので、
/// 時間による封印が起きないだけ（動作は明確）。確認用に `max_ballots` だけを小さくする設定を許すため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealPolicy {
    max_ballots: usize,
    interval_secs: u64,
    min_ballots_after_interval: usize,
}

impl SealPolicy {
    pub fn new(
        max_ballots: usize,
        interval_secs: u64,
        min_ballots_after_interval: usize,
    ) -> Result<Self, PolicyError> {
        if max_ballots == 0 {
            return Err(PolicyError::ZeroMaxBallots);
        }
        if interval_secs == 0 {
            return Err(PolicyError::ZeroInterval);
        }
        if min_ballots_after_interval == 0 {
            return Err(PolicyError::ZeroMinBallots);
        }
        Ok(Self {
            max_ballots,
            interval_secs,
            min_ballots_after_interval,
        })
    }

    pub fn max_ballots(&self) -> usize {
        self.max_ballots
    }

    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    pub fn min_ballots_after_interval(&self) -> usize {
        self.min_ballots_after_interval
    }
}

impl Default for SealPolicy {
    fn default() -> Self {
        Self {
            max_ballots: DEFAULT_MAX_BALLOTS,
            interval_secs: DEFAULT_INTERVAL_SECS,
            min_ballots_after_interval: DEFAULT_MIN_BALLOTS_AFTER_INTERVAL,
        }
    }
}

/// 封印の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealDecision {
    /// 到着順の先頭 N 件（= `max_ballots`）で封印する。残りは繰り越す。
    SealCount(usize),
    /// 時間による封印: 未封印の全件で封印する。
    SealAll,
    /// まだ封印しない。起点（窓）も動かさない。
    Wait,
    /// 投票終了の手続きの中での封印: 残り（1 件以上 `max_ballots` 件未満）の全件で封印する。
    CloseFlush,
}

impl SealDecision {
    /// この判定を実行した後の未封印件数。
    pub fn pending_after(self, pending: usize) -> usize {
        match self {
            Self::Wait => pending,
            Self::SealCount(n) => pending.saturating_sub(n),
            Self::SealAll | Self::CloseFlush => 0,
        }
    }

    /// ブロックを作る判定か（起点を封印時刻に進めるか）。
    pub fn seals(self) -> bool {
        !matches!(self, Self::Wait)
    }
}

/// 経過時間を測り始める時刻 = max(前回の封印時刻, 投票開始時刻)。
///
/// - `last_sealed_at`: このシャードで前回、票を封印した時刻。まだ封印していなければ `None`
/// - `voting_opened_at`: 投票開始時刻。開始前に投入された票はない前提（原則18 の受付判定が保証する）なので、
///   最初のブロックの経過時間は、投票開始時刻から測る
pub fn window_start(last_sealed_at: Option<u64>, voting_opened_at: u64) -> u64 {
    last_sealed_at.map_or(voting_opened_at, |sealed| sealed.max(voting_opened_at))
}

/// 投票期間中の封印判定。
///
/// - `pending`: 未封印の票の件数
/// - `window_start`: 経過時間の起点（[`window_start`] で求める）
/// - `now`: 現在時刻。`window_start` より前（時計の巻き戻り）なら経過 0 として扱う
pub fn decide(pending: usize, window_start: u64, now: u64, policy: &SealPolicy) -> SealDecision {
    // 件数の到達が最優先。時間の条件と同時でも、到着順に max_ballots 件だけ封印する。
    if pending >= policy.max_ballots {
        return SealDecision::SealCount(policy.max_ballots);
    }
    let elapsed = now.saturating_sub(window_start);
    if elapsed >= policy.interval_secs && pending >= policy.min_ballots_after_interval {
        return SealDecision::SealAll;
    }
    SealDecision::Wait
}

/// 投票終了（締切の手続き）の中での封印判定。件数や経過時間に関係なく、残りをすべて封印する。
/// ブロックの票数が `max_ballots` を超えないよう、`max_ballots` 件以上ある間は `SealCount` を返す
/// （250 件なら SealCount(100) → SealCount(100) → CloseFlush（50 件））。0 件なら `Wait`（空のブロックは作らない）。
pub fn decide_close(pending: usize, policy: &SealPolicy) -> SealDecision {
    if pending >= policy.max_ballots {
        SealDecision::SealCount(policy.max_ballots)
    } else if pending > 0 {
        SealDecision::CloseFlush
    } else {
        SealDecision::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SealDecision::{CloseFlush, SealAll, SealCount, Wait};

    const P: SealPolicy = SealPolicy {
        max_ballots: DEFAULT_MAX_BALLOTS,
        interval_secs: DEFAULT_INTERVAL_SECS,
        min_ballots_after_interval: DEFAULT_MIN_BALLOTS_AFTER_INTERVAL,
    };
    const MIN: u64 = 60;
    /// 投票開始時刻（テストの時刻の原点）。
    const OPEN: u64 = 1_800_000_000;

    /// 判定を連続で適用するための、呼び出し側（sealer）相当の状態。
    struct Sim {
        pending: usize,
        last_sealed_at: Option<u64>,
        opened_at: u64,
    }

    impl Sim {
        fn new(pending: usize) -> Self {
            Self {
                pending,
                last_sealed_at: None,
                opened_at: OPEN,
            }
        }

        fn step(&mut self, now: u64) -> SealDecision {
            let start = window_start(self.last_sealed_at, self.opened_at);
            let d = decide(self.pending, start, now, &P);
            self.apply(d, now);
            d
        }

        fn close(&mut self, now: u64) -> SealDecision {
            let d = decide_close(self.pending, &P);
            self.apply(d, now);
            d
        }

        fn apply(&mut self, d: SealDecision, now: u64) {
            if d.seals() {
                self.last_sealed_at = Some(now);
            }
            self.pending = d.pending_after(self.pending);
        }

        /// 投票終了の手続き: `Wait` になるまで `decide_close` を繰り返し、封印した件数の列を返す。
        fn close_all(&mut self, now: u64) -> Vec<usize> {
            let mut sealed = Vec::new();
            loop {
                let before = self.pending;
                match self.close(now) {
                    Wait => return sealed,
                    SealCount(n) => sealed.push(n),
                    CloseFlush | SealAll => sealed.push(before),
                }
            }
        }
    }

    // --- 依頼された必須ケース ---

    #[test]
    fn case_99_ballots_at_9m59s_waits_and_100_ballots_seal_at_any_time() {
        let start = window_start(None, OPEN);
        assert_eq!(decide(99, start, OPEN + 9 * MIN + 59, &P), Wait);
        for now in [
            OPEN,
            OPEN + 1,
            OPEN + 9 * MIN + 59,
            OPEN + 10 * MIN,
            OPEN + 100 * MIN,
        ] {
            assert_eq!(decide(100, start, now, &P), SealCount(100), "now={now}");
        }
    }

    #[test]
    fn case_250_ballots_seal_100_100_then_the_remaining_50_at_10_minutes() {
        let mut sim = Sim::new(250);
        assert_eq!(sim.step(OPEN + 5), SealCount(100));
        assert_eq!(sim.step(OPEN + 5), SealCount(100));
        assert_eq!(sim.pending, 50);
        // 残り 50 件は、前回の封印（OPEN + 5）から 10 分経つまで待つ。
        assert_eq!(sim.step(OPEN + 5), Wait);
        assert_eq!(sim.step(OPEN + 5 + 10 * MIN - 1), Wait);
        assert_eq!(sim.step(OPEN + 5 + 10 * MIN), SealAll);
        assert_eq!(sim.pending, 0);
    }

    #[test]
    fn case_9_ballots_at_10_minutes_wait_then_the_10th_at_12_minutes_seals_all_at_once() {
        let mut sim = Sim::new(9);
        assert_eq!(sim.step(OPEN + 10 * MIN), Wait);
        assert_eq!(sim.step(OPEN + 11 * MIN), Wait);
        // 12 分の時点で 10 件目が届いた: すぐに（その時点で）全 10 件を封印する。
        sim.pending += 1;
        let before = sim.pending;
        assert_eq!(sim.step(OPEN + 12 * MIN), SealAll);
        assert_eq!(before, 10);
        assert_eq!(sim.pending, 0);
    }

    #[test]
    fn case_exactly_10_ballots_at_exactly_10_minutes_seals_all() {
        let start = window_start(None, OPEN);
        assert_eq!(decide(10, start, OPEN + 10 * MIN, &P), SealAll);
        // 境界の 1 つ手前（件数・時間のどちらか）は待つ。
        assert_eq!(decide(9, start, OPEN + 10 * MIN, &P), Wait);
        assert_eq!(decide(10, start, OPEN + 10 * MIN - 1, &P), Wait);
    }

    #[test]
    fn case_0_ballots_at_10_minutes_wait_without_a_block_or_a_window_reset() {
        let mut sim = Sim::new(0);
        assert_eq!(sim.step(OPEN + 10 * MIN), Wait);
        // 窓はリセットしない: 起点は投票開始のまま。
        assert_eq!(sim.last_sealed_at, None);
        // その後 10 件届けば、（窓をリセットしていないので）すぐに封印する。
        sim.pending = 10;
        assert_eq!(sim.step(OPEN + 10 * MIN + 1), SealAll);
    }

    #[test]
    fn case_close_seals_3_as_one_block() {
        let mut sim = Sim::new(3);
        assert_eq!(sim.close_all(OPEN + MIN), vec![3]);
        assert_eq!(decide_close(3, &P), CloseFlush);
    }

    #[test]
    fn case_close_with_0_ballots_does_nothing() {
        let mut sim = Sim::new(0);
        assert_eq!(sim.close_all(OPEN + MIN), Vec::<usize>::new());
        assert_eq!(decide_close(0, &P), Wait);
    }

    #[test]
    fn case_close_splits_250_into_100_100_50() {
        let mut sim = Sim::new(250);
        assert_eq!(sim.close_all(OPEN + MIN), vec![100, 100, 50]);
        assert_eq!(decide_close(250, &P), SealCount(100));
        assert_eq!(decide_close(100, &P), SealCount(100));
        assert_eq!(decide_close(50, &P), CloseFlush);
    }

    #[test]
    fn case_the_voting_start_is_the_origin_of_the_elapsed_time() {
        // 開始前に投入された票はない前提。まだ一度も封印していなければ、起点は投票開始時刻。
        assert_eq!(window_start(None, OPEN), OPEN);
        // 投票開始から 10 分経たないうちは、10 件以上あっても時間では封印しない
        // （時計の原点（0）から測っていたら、ここで封印してしまう）。
        assert_eq!(
            decide(10, window_start(None, OPEN), OPEN + 10 * MIN - 1, &P),
            Wait
        );
        assert_eq!(
            decide(10, window_start(None, OPEN), OPEN + 10 * MIN, &P),
            SealAll
        );
        // 投票開始より前の封印（ジェネシスなど）が記録されていても、起点は投票開始時刻。
        assert_eq!(window_start(Some(OPEN - 3_600), OPEN), OPEN);
        // 投票開始より後に封印していれば、起点はその封印時刻。
        assert_eq!(window_start(Some(OPEN + 7), OPEN), OPEN + 7);
    }

    // --- 境界・補足 ---

    #[test]
    fn the_window_is_measured_from_the_last_seal() {
        let mut sim = Sim::new(100);
        assert_eq!(sim.step(OPEN + 3 * MIN), SealCount(100));
        sim.pending = 10;
        // 前回の封印（3 分）から 10 分経つまでは待つ（投票開始から測ると 10 分を過ぎているが）。
        assert_eq!(sim.step(OPEN + 12 * MIN), Wait);
        assert_eq!(sim.step(OPEN + 13 * MIN), SealAll);
    }

    #[test]
    fn a_minimum_above_the_maximum_only_disables_sealing_by_time() {
        let p = SealPolicy::new(3, 10, 10).expect("valid policy");
        assert_eq!(decide(2, OPEN, OPEN + 1_000, &p), Wait);
        assert_eq!(decide(3, OPEN, OPEN, &p), SealCount(3));
    }

    #[test]
    fn count_limit_takes_priority_over_time() {
        assert_eq!(decide(250, OPEN, OPEN + 10 * MIN, &P), SealCount(100));
        assert_eq!(decide(101, OPEN, OPEN, &P), SealCount(100));
    }

    #[test]
    fn clock_going_backwards_counts_as_zero_elapsed() {
        assert_eq!(decide(50, OPEN, OPEN - 1, &P), Wait);
        assert_eq!(decide(50, u64::MAX, 0, &P), Wait);
    }

    #[test]
    fn pending_after_and_seals_per_decision() {
        assert_eq!(Wait.pending_after(42), 42);
        assert_eq!(SealCount(100).pending_after(250), 150);
        assert_eq!(SealAll.pending_after(42), 0);
        assert_eq!(CloseFlush.pending_after(42), 0);
        assert!(!Wait.seals());
        assert!(SealCount(100).seals() && SealAll.seals() && CloseFlush.seals());
    }

    #[test]
    fn policy_rejects_invalid_values() {
        assert_eq!(
            SealPolicy::new(0, 600, 10),
            Err(PolicyError::ZeroMaxBallots)
        );
        assert_eq!(SealPolicy::new(100, 0, 10), Err(PolicyError::ZeroInterval));
        assert_eq!(
            SealPolicy::new(100, 600, 0),
            Err(PolicyError::ZeroMinBallots)
        );
        let p = SealPolicy::new(3, 7, 2).expect("valid policy");
        assert_eq!(
            (
                p.max_ballots(),
                p.interval_secs(),
                p.min_ballots_after_interval()
            ),
            (3, 7, 2)
        );
    }

    #[test]
    fn default_policy_is_100_ballots_600_secs_10_ballots() {
        assert_eq!(SealPolicy::default(), P);
        let p = SealPolicy::new(100, 600, 10).expect("valid policy");
        assert_eq!(p, SealPolicy::default());
    }

    #[test]
    fn one_arrival_per_minute_seals_every_ballot_exactly_once() {
        let mut sim = Sim::new(0);
        let mut sealed = Vec::new();
        // 25 件が 1 分に 1 件ずつ到着する。
        for i in 1..=25u64 {
            sim.pending += 1;
            let before = sim.pending;
            match sim.step(OPEN + i * MIN) {
                SealCount(n) => sealed.push(n),
                SealAll | CloseFlush => sealed.push(before),
                Wait => {}
            }
        }
        // 10 分（10 件）で 1 ブロック、次の 10 分（20 分時点で 10 件）で 1 ブロック、残り 5 件は締切で。
        assert_eq!(sealed, vec![10, 10]);
        assert_eq!(sim.close_all(OPEN + 26 * MIN), vec![5]);
        assert_eq!(sim.pending, 0);
    }
}
