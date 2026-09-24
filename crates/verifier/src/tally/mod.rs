//! `verifier tally`: 検証済みのチェーンから、選挙区別・都道府県別・選挙の種類別の得票を集計する。
//!
//! 集計の前に、チェーン全体の検証と、投票済み記録との突合が必ず成功していること（`main.rs` が保証する）。
//! ここには、純粋な部分（計算・判定・表示）と、ファイル出力を置く。

pub mod compute;
pub mod export;
pub mod gate;
pub mod render;

use serde::Serialize;

use crate::verify::{AnchorCheck, AuditReport, ShardReport};
use gate::Phase;

/// 出力に添える情報（表示名の元になる設定と、集計した日時）。
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    /// 設定の `labels.ballot_item`。
    pub ballot_item: &'a str,
    pub generated_at_unix: i64,
    pub phase: Phase,
    /// 設定の `election.voting_closes_at`（未設定なら `None`）。
    pub voting_closes_at: Option<&'a str>,
}

/// 投票用紙 1 枚の突合の行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconRow {
    pub contest_id: String,
    pub participation: u64,
    pub sealed: u64,
    pub pending: u64,
    pub consistent: bool,
}

/// 突合の結果（チェーンの検証 + 投票済み記録との突合 + 重複 + アンカー）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    pub shards: usize,
    pub blocks: usize,
    pub ballots: usize,
    pub contests: Vec<ReconRow>,
    pub duplicate_ballots: usize,
    pub anchor: String,
}

impl Reconciliation {
    pub fn new(reports: &[&ShardReport], audit: &AuditReport) -> Self {
        Self {
            shards: reports.len(),
            blocks: reports.iter().map(|r| r.blocks).sum(),
            ballots: reports.iter().map(|r| r.ballots).sum(),
            contests: audit
                .contests
                .iter()
                .map(|r| ReconRow {
                    contest_id: r.contest_id.clone(),
                    participation: r.participation,
                    sealed: r.sealed,
                    pending: r.pending,
                    consistent: r.is_consistent(),
                })
                .collect(),
            duplicate_ballots: audit.duplicate_ballots,
            anchor: match &audit.anchor {
                AnchorCheck::Valid { seq, shards } => format!("OK（seq={seq}, {shards} シャード）"),
                AnchorCheck::Missing => "まだ作られていません".to_string(),
                AnchorCheck::Invalid(reason) => format!("NG（{reason}）"),
            },
        }
    }
}
