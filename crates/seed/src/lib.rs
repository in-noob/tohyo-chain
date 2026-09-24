//! 選挙マスタと有権者名簿の読み込み・検証。
//!
//! ```text
//! seed/<election_id>/
//!   election.toml                  選挙の定義（id・名前）と、選挙の種類（code・表示名・表示順・投票方式）
//!   districts.csv                  district_id, election_type, name, prefectures（`;` 区切りの 2 桁コード）, order
//!   candidates/<election_type>.csv candidate_id, district_id, name, party, profile
//!   voters.csv                     voter_id, districts（`;` 区切りの district_id）
//! ```
//!
//! CSV は、表計算ソフトで編集できる（UTF-8、ヘッダ行あり。列の順序は問わない）。読み込み時に、ID の形式と
//! 最大長（`domain::ids`）、重複、存在しない選挙区・選挙の種類の参照、選挙区と候補者・有権者の整合を検証し、
//! 不正なものは「どのファイルの何行目がなぜ不正か」で、まとめて報告する。

mod csv_table;
mod election;
mod voters;

use std::fmt;
use std::path::{Path, PathBuf};

pub use election::load_election;
pub use voters::{VoterAssignments, load_voters};

use domain::Election;

/// 読み込んだ選挙マスタと有権者名簿。
#[derive(Debug, Clone)]
pub struct SeedData {
    pub election: Election,
    pub voters: VoterAssignments,
}

/// 1 件の不正（どのファイルの何行目か、何が不正か）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedIssue {
    pub file: PathBuf,
    /// CSV のデータの行番号（ヘッダが 1 行目）。ファイル全体の問題では `None`。
    pub line: Option<usize>,
    pub reason: String,
}

impl fmt::Display for SeedIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}: {}", self.file.display(), self.reason),
            None => write!(f, "{}: {}", self.file.display(), self.reason),
        }
    }
}

/// 表示する不正の件数の上限（残りは件数だけ示す）。
const MAX_SHOWN: usize = 30;

/// 1 件以上の不正。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct SeedError {
    pub issues: Vec<SeedIssue>,
}

impl fmt::Display for SeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "選挙データが不正です（{} 件）:", self.issues.len())?;
        for issue in self.issues.iter().take(MAX_SHOWN) {
            write!(f, "\n  - {issue}")?;
        }
        if self.issues.len() > MAX_SHOWN {
            write!(f, "\n  - ほか {} 件", self.issues.len() - MAX_SHOWN)?;
        }
        Ok(())
    }
}

/// 不正を集める。
#[derive(Debug, Default)]
pub(crate) struct Issues(Vec<SeedIssue>);

impl Issues {
    pub(crate) fn push(&mut self, file: &Path, line: Option<usize>, reason: impl Into<String>) {
        self.0.push(SeedIssue {
            file: file.to_path_buf(),
            line,
            reason: reason.into(),
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn into_error(self) -> SeedError {
        SeedError { issues: self.0 }
    }
}

/// 選挙のディレクトリ: `<seed_dir>/<election_id>`。
pub fn election_dir(seed_dir: &Path, election_id: &str) -> PathBuf {
    seed_dir.join(election_id)
}

/// `<seed_dir>/<election_id>/` の選挙マスタと、有権者名簿（`voters.csv`）を読む。
pub fn load(seed_dir: &Path, election_id: &str) -> Result<SeedData, SeedError> {
    let dir = election_dir(seed_dir, election_id);
    let election = load_election(&dir)?;
    let voters = load_voters(&dir, &election)?;
    Ok(SeedData { election, voters })
}

#[cfg(test)]
mod tests;
