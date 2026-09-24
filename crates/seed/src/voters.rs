//! 有権者と選挙区の対応（voters.csv）。有権者ごとに、属する選挙区のリストを持つ。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use domain::{DistrictId, Election, ElectionTypeCode, VoterId};

use crate::csv_table::{read_table, split_list};
use crate::{Issues, SeedError};

/// 有権者ごとの、属する選挙区のリスト。
#[derive(Debug, Clone, Default)]
pub struct VoterAssignments {
    map: HashMap<VoterId, Vec<DistrictId>>,
}

impl VoterAssignments {
    /// 名簿を組み立てる（読み込み済みのデータや、テストから）。
    pub fn from_map(map: HashMap<VoterId, Vec<DistrictId>>) -> Self {
        Self { map }
    }

    pub fn get(&self, voter: &VoterId) -> Option<&[DistrictId]> {
        self.map.get(voter).map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&VoterId, &[DistrictId])> {
        self.map
            .iter()
            .map(|(voter, districts)| (voter, districts.as_slice()))
    }

    /// 内部のマップ（メモリ保存の名簿に所有権ごと渡す）。
    pub fn into_map(self) -> HashMap<VoterId, Vec<DistrictId>> {
        self.map
    }
}

/// `<dir>/voters.csv` を読んで検証する。選挙区は `election` にあるものだけ。同じ選挙の種類の選挙区は、
/// 1 人の有権者につき 1 つまで（種類ごとに投票用紙は 1 枚）。
pub fn load_voters(dir: &Path, election: &Election) -> Result<VoterAssignments, SeedError> {
    let path = dir.join("voters.csv");
    let mut issues = Issues::default();
    let Some(table) = read_table(&path, &["voter_id", "districts"], &[], &mut issues) else {
        return Err(issues.into_error());
    };
    let mut map: HashMap<VoterId, Vec<DistrictId>> = HashMap::with_capacity(table.rows.len());
    let mut first_line: HashMap<String, usize> = HashMap::with_capacity(table.rows.len());
    for row in &table.rows {
        let line = row.line;
        let raw_voter = table.get(row, "voter_id");
        let voter = match VoterId::new(raw_voter) {
            Ok(voter) => voter,
            Err(e) => {
                issues.push(&path, Some(line), format!("voter_id {raw_voter:?}: {e}"));
                continue;
            }
        };
        if let Some(first) = first_line.insert(raw_voter.to_string(), line) {
            issues.push(
                &path,
                Some(line),
                format!(
                    "有権者 {voter:?} が重複しています（{first} 行目と同じ ID）",
                    voter = voter.as_str()
                ),
            );
            continue;
        }
        let mut districts = Vec::new();
        let mut types_seen: HashSet<ElectionTypeCode> = HashSet::new();
        let mut row_ok = true;
        for raw in split_list(table.get(row, "districts")) {
            let district = match DistrictId::new(raw) {
                Ok(district) => district,
                Err(e) => {
                    issues.push(&path, Some(line), format!("districts: {e}"));
                    row_ok = false;
                    continue;
                }
            };
            let Some(contest) = election.contest_for_district(&district) else {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "有権者 {} が、存在しない選挙区 {district} に属しています",
                        voter.as_str()
                    ),
                );
                row_ok = false;
                continue;
            };
            if !types_seen.insert(contest.district.election_type.clone()) {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "有権者 {} に、選挙の種類 {} の選挙区が複数あります（{district} など。1 つまで）",
                        voter.as_str(),
                        contest.district.election_type
                    ),
                );
                row_ok = false;
                continue;
            }
            districts.push(district);
        }
        if row_ok {
            map.insert(voter, districts);
        }
    }
    if issues.is_empty() {
        Ok(VoterAssignments { map })
    } else {
        Err(issues.into_error())
    }
}
