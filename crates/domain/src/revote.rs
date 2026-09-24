//! 再投票のつながりの検証と、slot ごとの最後の票（CLAUDE.md 原則1・ADR 0022）。
//!
//! 再投票を認める選挙（`vote.allow_revote = true`）の票は、再投票の仮名 `slot` と、その slot の何番目の票か（`seq`）、
//! 1 つ前の版の票のハッシュ（`supersedes`）を持つ。同じ slot の全版は同じシャード（[`crate::shard_for_slot`]）の
//! チェーンに入るので、ここでは、検証済みのチェーンの票を受け取って、次を確かめる:
//!
//! - slot ごとに、`seq` が 1 から連続している（重複・欠番がない）
//! - `supersedes` が、1 つ前の版の票のハッシュと一致する（初回の投票は `supersedes` を持たない）
//! - `seq` が `max_revotes + 1` 以下
//! - 再投票を認めない選挙に、`seq > 1` の票（と、そもそも slot を持つ票）がない。認める選挙に、slot の無い票がない
//! - 同じ slot の票が、同じ投票用紙・`hash(slot)` のシャードにある
//!
//! 締切前は、最後の版がまだ封印されていないことがある。その場合も、封印済みの版は 1 から連続している（未封印の票は
//! 到着順に封印され、次の版は前の版より先に封印されない）ので、同じ規則で検証できる。
//!
//! 時刻・IO を持たない純粋関数で、エラーには票の位置（シャード・高さ）だけを含め、票の中身（候補者）は含めない。

use std::collections::BTreeMap;
use std::num::NonZeroU16;

use crate::election_state::ElectionRules;
use crate::encoding::ballot_hash;
use crate::shard::shard_for_slot;
use crate::types::{Ballot, Slot};

/// 検証済みのチェーンにある 1 票と、その位置。
#[derive(Debug, Clone, Copy)]
pub struct PlacedBallot<'a> {
    pub shard: u16,
    pub height: u64,
    pub ballot: &'a Ballot,
}

/// 再投票のつながりの不整合。位置（シャード・高さ）は、問題の票のもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RevoteError {
    #[error(
        "shard={shard} height={height}: 再投票を認めない選挙なのに、seq={seq} の票があります（2 回目以降の票）"
    )]
    RevoteNotAllowed { shard: u16, height: u64, seq: u32 },
    #[error("shard={shard} height={height}: 再投票を認めない選挙なのに、slot を持つ票があります")]
    SlotNotAllowed { shard: u16, height: u64 },
    #[error("shard={shard} height={height}: 再投票を認める選挙なのに、slot の無い票があります")]
    MissingSlot { shard: u16, height: u64 },
    #[error("shard={shard} height={height}: seq={seq} が範囲外です（1 以上 {max} 以下）")]
    SeqOutOfRange {
        shard: u16,
        height: u64,
        seq: u32,
        max: u32,
    },
    #[error("shard={shard} height={height}: 同じ slot に、同じ seq={seq} の票が複数あります")]
    DuplicateSeq { shard: u16, height: u64, seq: u32 },
    #[error("shard={shard} height={height}: seq={seq} の票の前の版（seq={missing}）がありません")]
    SeqGap {
        shard: u16,
        height: u64,
        seq: u32,
        missing: u32,
    },
    #[error(
        "shard={shard} height={height}: seq={seq} の票の supersedes が、1 つ前の版の票のハッシュと一致しません"
    )]
    SupersedesMismatch { shard: u16, height: u64, seq: u32 },
    #[error("shard={shard} height={height}: 同じ slot の票が、別の投票用紙にあります")]
    ContestMismatch { shard: u16, height: u64 },
    #[error(
        "shard={shard} height={height}: slot の票が、hash(slot) のシャード（{expected}）にありません"
    )]
    WrongShard {
        shard: u16,
        height: u64,
        expected: u16,
    },
}

impl RevoteError {
    /// エラー種別名（表示・テスト用）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RevoteNotAllowed { .. } => "RevoteNotAllowed",
            Self::SlotNotAllowed { .. } => "SlotNotAllowed",
            Self::MissingSlot { .. } => "MissingSlot",
            Self::SeqOutOfRange { .. } => "SeqOutOfRange",
            Self::DuplicateSeq { .. } => "DuplicateSeq",
            Self::SeqGap { .. } => "SeqGap",
            Self::SupersedesMismatch { .. } => "SupersedesMismatch",
            Self::ContestMismatch { .. } => "ContestMismatch",
            Self::WrongShard { .. } => "WrongShard",
        }
    }
}

/// 検証の結果。票は借用のまま返す（呼び出し側のチェーンの票を複製しない）。
#[derive(Debug, Clone, Default)]
pub struct RevoteAnalysis<'a> {
    /// 集計に数える票: slot ごとの最後の版（`seq` が最大）と、slot を持たない票。
    pub counted: Vec<&'a Ballot>,
    /// 置き換え（1 つ前の版 → 次の版）。再投票の件数と、変更の内訳（A→B）の元。
    pub replacements: Vec<(&'a Ballot, &'a Ballot)>,
    /// slot の数（重複を除く）。
    pub slots: usize,
}

/// 検証済みのチェーンの票（全シャード）から、再投票のつながりを検証する。最初に見つかった不整合を返す。
pub fn analyze_revotes<'a>(
    ballots: &[PlacedBallot<'a>],
    rules: ElectionRules,
    shard_count: NonZeroU16,
) -> Result<RevoteAnalysis<'a>, RevoteError> {
    let mut out = RevoteAnalysis::default();
    let mut by_slot: BTreeMap<Slot, Vec<PlacedBallot<'a>>> = BTreeMap::new();
    for placed in ballots {
        let (shard, height) = (placed.shard, placed.height);
        match (&placed.ballot.revote, rules.allow_revote) {
            (None, false) => out.counted.push(placed.ballot),
            (None, true) => return Err(RevoteError::MissingSlot { shard, height }),
            (Some(link), false) if link.seq > 1 => {
                return Err(RevoteError::RevoteNotAllowed {
                    shard,
                    height,
                    seq: link.seq,
                });
            }
            (Some(_), false) => return Err(RevoteError::SlotNotAllowed { shard, height }),
            (Some(link), true) => by_slot.entry(link.slot).or_default().push(*placed),
        }
    }

    let max = rules.max_seq();
    out.slots = by_slot.len();
    for (slot, mut versions) in by_slot {
        versions.sort_by_key(|p| p.ballot.revote.map_or(0, |l| l.seq));
        let expected_shard = shard_for_slot(&slot, shard_count).0;
        let first_contest = &versions[0].ballot.contest_id;
        let mut prev: Option<&PlacedBallot<'a>> = None;
        for (index, placed) in versions.iter().enumerate() {
            let (shard, height) = (placed.shard, placed.height);
            let Some(link) = placed.ballot.revote else {
                continue;
            };
            if link.seq == 0 || link.seq > max {
                return Err(RevoteError::SeqOutOfRange {
                    shard,
                    height,
                    seq: link.seq,
                    max,
                });
            }
            // 並べ替えた後の位置 + 1 が seq でなければ、重複か欠番。
            let want = u32::try_from(index).map_or(u32::MAX, |i| i.saturating_add(1));
            if link.seq < want {
                return Err(RevoteError::DuplicateSeq {
                    shard,
                    height,
                    seq: link.seq,
                });
            }
            if link.seq > want {
                return Err(RevoteError::SeqGap {
                    shard,
                    height,
                    seq: link.seq,
                    missing: want,
                });
            }
            if shard != expected_shard {
                return Err(RevoteError::WrongShard {
                    shard,
                    height,
                    expected: expected_shard,
                });
            }
            if &placed.ballot.contest_id != first_contest {
                return Err(RevoteError::ContestMismatch { shard, height });
            }
            let expected_supersedes = prev.map(|p| ballot_hash(p.ballot));
            if link.supersedes != expected_supersedes {
                return Err(RevoteError::SupersedesMismatch {
                    shard,
                    height,
                    seq: link.seq,
                });
            }
            if let Some(p) = prev {
                out.replacements.push((p.ballot, placed.ballot));
            }
            prev = Some(placed);
        }
        if let Some(last) = prev {
            out.counted.push(last.ballot);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CandidateId, ContestId, DistrictId};
    use crate::types::{BallotId, RevoteLink};

    const SHARDS: u16 = 4;

    fn count() -> NonZeroU16 {
        NonZeroU16::new(SHARDS).expect("non-zero")
    }

    fn rules(allow_revote: bool) -> ElectionRules {
        ElectionRules {
            allow_revote,
            max_revotes: 2,
            ..ElectionRules::default()
        }
    }

    fn cand(seq: u64) -> CandidateId {
        CandidateId::new(&DistrictId::new("shugiin_smd.13.01").expect("valid"), seq).expect("valid")
    }

    fn contest(n: u8) -> ContestId {
        ContestId::parse(&format!("2026-general/shugiin_smd.13.0{n}")).expect("valid")
    }

    /// slot `s` の版を、A → B → 白票 … の順に `n` 件作る（つながりは正しい）。
    fn versions(s: u8, n: u32) -> Vec<Ballot> {
        let slot = Slot([s; 32]);
        let mut out: Vec<Ballot> = Vec::new();
        for seq in 1..=n {
            let candidate = match seq % 3 {
                1 => cand(1),
                2 => cand(2),
                _ => CandidateId::Blank,
            };
            let supersedes = out.last().map(ballot_hash);
            let mut id = [s; 16];
            id[0] = u8::try_from(seq).expect("small");
            out.push(Ballot {
                ballot_id: BallotId::from_random_bytes(id),
                contest_id: contest(1),
                candidate_id: candidate,
                revote: Some(RevoteLink {
                    slot,
                    seq,
                    supersedes,
                }),
            });
        }
        out
    }

    /// 票を、slot のシャードの、高さ 1, 2, … に置く。
    fn place(ballots: &[Ballot]) -> Vec<PlacedBallot<'_>> {
        ballots
            .iter()
            .enumerate()
            .map(|(i, b)| PlacedBallot {
                shard: b.revote.map_or(0, |l| shard_for_slot(&l.slot, count()).0),
                height: i as u64 + 1,
                ballot: b,
            })
            .collect()
    }

    fn kind(ballots: &[Ballot], rules: ElectionRules) -> &'static str {
        analyze_revotes(&place(ballots), rules, count())
            .expect_err("should fail")
            .kind()
    }

    #[test]
    fn only_the_last_version_of_each_slot_is_counted() {
        let mut ballots = versions(1, 3);
        ballots.extend(versions(2, 1));
        let placed = place(&ballots);
        let analysis = analyze_revotes(&placed, rules(true), count()).expect("valid");
        assert_eq!(analysis.slots, 2);
        let counted: Vec<&CandidateId> = analysis.counted.iter().map(|b| &b.candidate_id).collect();
        // slot 1: A → B → 白票 の最後（白票）。slot 2: A だけ。
        assert!(counted.contains(&&CandidateId::Blank));
        assert!(counted.contains(&&cand(1)));
        assert_eq!(counted.len(), 2);
        let changes: Vec<(&CandidateId, &CandidateId)> = analysis
            .replacements
            .iter()
            .map(|(a, b)| (&a.candidate_id, &b.candidate_id))
            .collect();
        assert_eq!(
            changes,
            vec![(&cand(1), &cand(2)), (&cand(2), &CandidateId::Blank)]
        );
    }

    #[test]
    fn the_order_in_the_chain_does_not_matter_only_the_links_do() {
        let mut ballots = versions(1, 3);
        ballots.reverse();
        let analysis = analyze_revotes(&place(&ballots), rules(true), count()).expect("valid");
        assert_eq!(analysis.counted[0].candidate_id, CandidateId::Blank);
    }

    #[test]
    fn ballots_without_slots_are_counted_as_they_are_when_revotes_are_off() {
        let plain = Ballot {
            revote: None,
            ..versions(1, 1).remove(0)
        };
        let ballots = vec![
            plain.clone(),
            Ballot {
                ballot_id: BallotId([9; 16]),
                ..plain
            },
        ];
        let analysis = analyze_revotes(&place(&ballots), rules(false), count()).expect("valid");
        assert_eq!((analysis.counted.len(), analysis.slots), (2, 0));
        assert!(analysis.replacements.is_empty());
    }

    #[test]
    fn a_second_vote_or_a_slot_is_rejected_when_revotes_are_off() {
        assert_eq!(kind(&versions(1, 2)[1..], rules(false)), "RevoteNotAllowed");
        assert_eq!(kind(&versions(1, 1), rules(false)), "SlotNotAllowed");
        let plain = Ballot {
            revote: None,
            ..versions(1, 1).remove(0)
        };
        assert_eq!(kind(&[plain], rules(true)), "MissingSlot");
    }

    #[test]
    fn gaps_duplicates_and_the_limit_are_detected() {
        // 欠番（seq=2 が無い）。
        let mut gap = versions(1, 3);
        gap.remove(1);
        assert_eq!(kind(&gap, rules(true)), "SeqGap");
        // 最初の版が無い。
        let mut headless = versions(1, 2);
        headless.remove(0);
        assert_eq!(kind(&headless, rules(true)), "SeqGap");
        // 重複（同じ seq が 2 件）。
        let mut dup = versions(1, 2);
        let mut again = dup[1].clone();
        again.ballot_id = BallotId([0xee; 16]);
        dup.push(again);
        assert_eq!(kind(&dup, rules(true)), "DuplicateSeq");
        // 上限: max_revotes=2 なら seq=3 まで。
        assert!(analyze_revotes(&place(&versions(1, 3)), rules(true), count()).is_ok());
        assert_eq!(kind(&versions(1, 4), rules(true)), "SeqOutOfRange");
    }

    #[test]
    fn broken_links_wrong_contests_and_wrong_shards_are_detected() {
        let mut broken = versions(1, 2);
        if let Some(link) = broken[1].revote.as_mut() {
            link.supersedes = Some([0; 32]);
        }
        assert_eq!(kind(&broken, rules(true)), "SupersedesMismatch");
        // 初回の投票が supersedes を持つ。
        let mut initial = versions(1, 1);
        if let Some(link) = initial[0].revote.as_mut() {
            link.supersedes = Some([0; 32]);
        }
        assert_eq!(kind(&initial, rules(true)), "SupersedesMismatch");

        let mut other_contest = versions(1, 2);
        other_contest[1].contest_id = contest(2);
        // 投票用紙を書き換えると、つながり（ハッシュ）より先に、投票用紙の不一致として見つかる。
        assert_eq!(kind(&other_contest, rules(true)), "ContestMismatch");

        let ballots = versions(1, 1);
        let mut placed = place(&ballots);
        placed[0].shard = (placed[0].shard + 1) % SHARDS;
        assert_eq!(
            analyze_revotes(&placed, rules(true), count())
                .expect_err("wrong shard")
                .kind(),
            "WrongShard"
        );
    }
}
