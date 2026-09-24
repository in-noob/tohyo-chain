//! 集計してよいかの判定（純粋関数）。
//!
//! チェーンの検証と突合が済んだ後に使う。未封印の票が残っていれば、`--allow-interim` があっても中止する
//! （封印されていない票は、チェーンで守られていない）。選挙状態が `closed` より前の集計（中間集計）は、
//! `--allow-interim` があるときだけ（原則17。`--allow-interim` 自体は `app.env=dev` のときだけ受け付ける。
//! `main.rs` が保証する）。
//!
//! 締切（`election.voting_closes_at`）に基づく時刻の判定は、選挙状態（`ElectionPhase`）で置き換えた
//! （時刻による判定は、api・sealer と重複するため削除。ADR 0019）。

use domain::ElectionPhase;

/// 集計に進めるときの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 選挙状態が `closed` になった後の集計。
    Final,
    /// `closed` より前の中間集計。`--allow-interim` が指定されたときだけ。
    Interim,
}

/// 集計しない理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// 未封印の票が残っている。
    Unsealed { ballots: u64, contests: usize },
    /// 選挙状態が `closed` ではなく、`--allow-interim` がない。
    NotClosed { phase: ElectionPhase },
}

/// 突合の行から、未封印の票の合計と、未封印のある投票用紙の数を出す。
pub fn pending_of(rows: &[crate::verify::ContestRow]) -> (u64, usize) {
    let total = rows.iter().map(|r| r.pending).sum();
    let contests = rows.iter().filter(|r| r.pending > 0).count();
    (total, contests)
}

/// 集計してよいか。未封印の確認が先（`--allow-interim` でも通らない）。
pub fn check(
    pending_ballots: u64,
    pending_contests: usize,
    phase: ElectionPhase,
    allow_interim: bool,
) -> Result<Phase, Refusal> {
    if pending_ballots > 0 {
        return Err(Refusal::Unsealed {
            ballots: pending_ballots,
            contests: pending_contests,
        });
    }
    match phase {
        ElectionPhase::Closed => Ok(Phase::Final),
        _ if allow_interim => Ok(Phase::Interim),
        other => Err(Refusal::NotClosed { phase: other }),
    }
}

impl Refusal {
    /// 利用者向けの文言。`ballot_item` は設定の `labels.ballot_item`。
    pub fn message(&self, ballot_item: &str) -> String {
        match self {
            Self::Unsealed { ballots, contests } => format!(
                "未封印の票が {ballots} 件（{contests} 枚の{ballot_item}）残っています。集計を中止しました。\n\
                 sealer を SIGTERM で正常停止して（締切フラッシュ: 残りの票をすべて封印します）から、もう一度集計してください。"
            ),
            Self::NotClosed { phase } => format!(
                "選挙状態が {phase}（closed ではありません）なので、集計を中止しました。\n\
                 中間集計は、--allow-interim を指定したときだけです（app.env=dev のときだけ使えます）。"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ElectionPhase::{Closed, Closing, Open, Scheduled};

    #[test]
    fn closed_with_nothing_pending_is_final() {
        assert_eq!(check(0, 0, Closed, false), Ok(Phase::Final));
        // --allow-interim があっても、closed は確定した集計。
        assert_eq!(check(0, 0, Closed, true), Ok(Phase::Final));
    }

    #[test]
    fn not_closed_needs_allow_interim() {
        for phase in [Scheduled, Open, Closing] {
            assert_eq!(
                check(0, 0, phase, false),
                Err(Refusal::NotClosed { phase }),
                "{phase}"
            );
            assert_eq!(check(0, 0, phase, true), Ok(Phase::Interim), "{phase}");
        }
    }

    #[test]
    fn pending_ballots_always_stop_the_tally_even_with_allow_interim() {
        let expected = Err(Refusal::Unsealed {
            ballots: 3,
            contests: 2,
        });
        assert_eq!(check(3, 2, Closed, false), expected);
        assert_eq!(check(3, 2, Closed, true), expected);
        assert_eq!(check(3, 2, Open, true), expected);
    }

    #[test]
    fn messages_name_the_next_step() {
        let m = Refusal::Unsealed {
            ballots: 3,
            contests: 2,
        }
        .message("投票用紙");
        assert!(m.contains("3 件") && m.contains("2 枚の投票用紙") && m.contains("SIGTERM"));
        let m = Refusal::NotClosed { phase: Open }.message("投票用紙");
        assert!(m.contains("--allow-interim") && m.contains("open"));
    }

    #[test]
    fn pending_is_summed_from_the_reconciliation_rows() {
        use crate::verify::ContestRow;
        let row = |pending| ContestRow {
            contest_id: "e/d".to_string(),
            participation: pending,
            sealed: 0,
            pending,
        };
        assert_eq!(pending_of(&[row(0), row(2), row(5)]), (7, 2));
        assert_eq!(pending_of(&[]), (0, 0));
    }
}
