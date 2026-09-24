//! 選挙状態（scheduled → open → closing → closed）の判定ロジック（CLAUDE.md 原則17・18）。
//!
//! `seal_policy` と同じ方針: すべて純粋関数で、時刻は引数（UNIX 秒）で受け取る。
//! 状態そのもの（DB の行）は持たない。呼び出し側（sealer / api）が、この判定に従って
//! ストアを読み書きする。

use std::fmt;

/// 選挙の状態。この順にしか進まない（原則17）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ElectionPhase {
    Scheduled,
    Open,
    Closing,
    Closed,
}

impl ElectionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(Self::Scheduled),
            "open" => Some(Self::Open),
            "closing" => Some(Self::Closing),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }

    /// `self` から `to` へ、原則17の順（1 段ずつ）で進めるか。
    pub fn can_advance_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Scheduled, Self::Open)
                | (Self::Open, Self::Closing)
                | (Self::Closing, Self::Closed)
        )
    }
}

impl fmt::Display for ElectionPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 選挙のルール（原則19）。open に遷移する時点で、遷移させたプロセスの設定の値を選挙状態に保存し（固定し）、
/// それ以降は変更しない（設定ファイルを書き換えて再起動しても、保存した値を使う）。
///
/// 今あるのは白票の可否（設定 `vote.allow_blank`）だけ。再投票の可否・上限、封印ルールの固定は未実装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElectionRules {
    /// 白票（どの候補者にも投票しない）を受け付けるか。
    pub allow_blank: bool,
}

impl ElectionRules {
    /// 実際に使うルール: open の時点で固定した値（`frozen`）があればそれ、無ければ（open の前、または
    /// ルールを記録する前の版で open にした選挙なら）このプロセスの設定の値（`configured`）。
    pub fn effective(frozen: Option<Self>, configured: Self) -> Self {
        frozen.unwrap_or(configured)
    }
}

/// 投票の受付期間（両方 `None` なら無期限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Period {
    pub opens_at: Option<i64>,
    pub closes_at: Option<i64>,
}

/// アンカーのリースを持つ sealer が、自動で行う状態遷移の判定（原則17）。
/// 1 回の呼び出しで 1 段だけ進める（進めた後は、呼び出し側がストアの状態を読み直してから次を判定する）。
pub fn automatic_transition(
    phase: ElectionPhase,
    period: Period,
    now: i64,
) -> Option<ElectionPhase> {
    match phase {
        ElectionPhase::Scheduled => match period.opens_at {
            Some(opens_at) if now >= opens_at => Some(ElectionPhase::Open),
            _ => None,
        },
        ElectionPhase::Open => match period.closes_at {
            Some(closes_at) if now >= closes_at => Some(ElectionPhase::Closing),
            _ => None,
        },
        ElectionPhase::Closing | ElectionPhase::Closed => None,
    }
}

/// 投票が実際に始まった時刻（UNIX 秒）。封印の経過時間の起点に使う（原則9。ADR 0020）。
///
/// 票を受け付けるのは「状態が open」かつ「開始時刻 <= 現在時刻」のときだけ（[`vote_gate`]）なので、
/// 実際の開始は、open に遷移した時刻（`opened_at`）と設定の開始時刻（`period.opens_at`）の遅い方。
/// まだ open になっていなければ `None`。
pub fn voting_started_at(period: Period, opened_at: Option<i64>) -> Option<i64> {
    let opened_at = opened_at?;
    Some(
        period
            .opens_at
            .map_or(opened_at, |opens_at| opens_at.max(opened_at)),
    )
}

/// 投票を受け付けてよいかの判定結果（原則18）。理由ごとに画面・API のメッセージを分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteGate {
    /// 状態が open、かつ 開始時刻 <= 現在時刻 < 終了時刻。
    Accept,
    /// まだ開始していない（scheduled、または open だが開始時刻前）。
    NotStarted,
    /// 締切の手続き中（closing）。
    Closing,
    /// 終了している（closed、または open だが終了時刻以後）。
    Ended,
}

/// 投票を受け付けてよいか（原則18）。状態と期間の両方を見る
/// （`open --now` で期間前に手動で開けた場合や、締切の自動遷移が遅れている場合を含むため）。
pub fn vote_gate(phase: ElectionPhase, period: Period, now: i64) -> VoteGate {
    match phase {
        ElectionPhase::Scheduled => VoteGate::NotStarted,
        ElectionPhase::Closing => VoteGate::Closing,
        ElectionPhase::Closed => VoteGate::Ended,
        ElectionPhase::Open => {
            if period.opens_at.is_some_and(|opens_at| now < opens_at) {
                VoteGate::NotStarted
            } else if period.closes_at.is_some_and(|closes_at| now >= closes_at) {
                VoteGate::Ended
            } else {
                VoteGate::Accept
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ElectionPhase::{Closed, Closing, Open, Scheduled};

    #[test]
    fn frozen_rules_win_over_the_configured_ones() {
        let on = ElectionRules { allow_blank: true };
        let off = ElectionRules { allow_blank: false };
        // open の前（固定前）は設定の値。
        assert_eq!(ElectionRules::effective(None, off), off);
        assert_eq!(ElectionRules::effective(None, on), on);
        // 固定した後は、設定を変えても固定した値。
        assert_eq!(ElectionRules::effective(Some(on), off), on);
        assert_eq!(ElectionRules::effective(Some(off), on), off);
    }

    #[test]
    fn phase_round_trips_through_text() {
        for phase in [Scheduled, Open, Closing, Closed] {
            assert_eq!(ElectionPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(ElectionPhase::parse("bogus"), None);
    }

    #[test]
    fn phases_only_advance_one_step_forward() {
        assert!(Scheduled.can_advance_to(Open));
        assert!(Open.can_advance_to(Closing));
        assert!(Closing.can_advance_to(Closed));
        assert!(!Scheduled.can_advance_to(Closing));
        assert!(!Open.can_advance_to(Scheduled));
        assert!(!Closed.can_advance_to(Scheduled));
        assert!(!Scheduled.can_advance_to(Scheduled));
    }

    #[test]
    fn automatic_transition_opens_at_the_start_time_inclusive() {
        let period = Period {
            opens_at: Some(1_000),
            closes_at: Some(2_000),
        };
        assert_eq!(automatic_transition(Scheduled, period, 999), None);
        assert_eq!(automatic_transition(Scheduled, period, 1_000), Some(Open));
        assert_eq!(automatic_transition(Scheduled, period, 1_001), Some(Open));
    }

    #[test]
    fn automatic_transition_closes_at_the_end_time_inclusive() {
        let period = Period {
            opens_at: Some(1_000),
            closes_at: Some(2_000),
        };
        assert_eq!(automatic_transition(Open, period, 1_999), None);
        assert_eq!(automatic_transition(Open, period, 2_000), Some(Closing));
    }

    #[test]
    fn automatic_transition_needs_a_configured_time() {
        assert_eq!(
            automatic_transition(Scheduled, Period::default(), 1_000),
            None
        );
        assert_eq!(automatic_transition(Open, Period::default(), 1_000), None);
    }

    #[test]
    fn closing_and_closed_never_auto_advance() {
        let period = Period {
            opens_at: Some(0),
            closes_at: Some(0),
        };
        assert_eq!(automatic_transition(Closing, period, 10_000), None);
        assert_eq!(automatic_transition(Closed, period, 10_000), None);
    }

    #[test]
    fn vote_gate_follows_phase_first() {
        let period = Period {
            opens_at: Some(1_000),
            closes_at: Some(2_000),
        };
        assert_eq!(vote_gate(Scheduled, period, 1_500), VoteGate::NotStarted);
        assert_eq!(vote_gate(Closing, period, 1_500), VoteGate::Closing);
        assert_eq!(vote_gate(Closed, period, 1_500), VoteGate::Ended);
    }

    #[test]
    fn vote_gate_checks_the_period_even_while_open() {
        let period = Period {
            opens_at: Some(1_000),
            closes_at: Some(2_000),
        };
        // open --now で期間前に手動で開けた場合。
        assert_eq!(vote_gate(Open, period, 999), VoteGate::NotStarted);
        assert_eq!(vote_gate(Open, period, 1_000), VoteGate::Accept);
        assert_eq!(vote_gate(Open, period, 1_999), VoteGate::Accept);
        // 終了時刻は含まない。
        assert_eq!(vote_gate(Open, period, 2_000), VoteGate::Ended);
    }

    #[test]
    fn voting_starts_at_the_later_of_the_transition_and_the_configured_time() {
        let period = Period {
            opens_at: Some(1_000),
            closes_at: Some(2_000),
        };
        // まだ open になっていない。
        assert_eq!(voting_started_at(period, None), None);
        // 開始時刻ちょうどに自動で open になった。
        assert_eq!(voting_started_at(period, Some(1_000)), Some(1_000));
        // open --now で期間前に開けても、受付は開始時刻から（vote_gate）なので、起点も開始時刻。
        assert_eq!(voting_started_at(period, Some(900)), Some(1_000));
        // 自動遷移が遅れた（sealer の周期の分）: 起点は実際に open になった時刻。
        assert_eq!(voting_started_at(period, Some(1_001)), Some(1_001));
        // 期間の指定がなく、open --now で開けた。
        assert_eq!(
            voting_started_at(Period::default(), Some(1_234)),
            Some(1_234)
        );
    }

    #[test]
    fn vote_gate_with_no_period_accepts_whenever_open() {
        assert_eq!(vote_gate(Open, Period::default(), 0), VoteGate::Accept);
        assert_eq!(
            vote_gate(Open, Period::default(), i64::MAX),
            VoteGate::Accept
        );
    }
}
