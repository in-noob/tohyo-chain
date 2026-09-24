//! 集計の計算（純粋関数）。入力は、検証済みのチェーンの票（`ShardReport::votes`）と、選挙マスタ、突合の行。
//!
//! 投票者は、チェーンにも participation にも紐付かない（秘密投票）ので、ここで扱うのは、
//! 投票用紙・候補者ごとの件数だけ。

use std::collections::{BTreeMap, HashMap};

use domain::election::Election;
use domain::ids::prefecture_name;
use domain::{CandidateId, ContestId};
use serde::Serialize;

use crate::verify::{ContestRow, ShardReport};

/// 集計できない理由（チェーンと選挙データの食い違い）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TallyError {
    #[error(
        "チェーンに、選挙データ（seed）に存在しない投票用紙 {0} の票があります（election.election_id / election.seed_dir が、この選挙のものか確認してください）"
    )]
    UnknownContest(String),
}

/// 候補者 1 人の得票。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateVotes {
    pub candidate_id: String,
    pub name: String,
    pub party: String,
    pub votes: u64,
}

/// 選挙区（= 投票用紙 1 枚）の集計。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DistrictTally {
    pub contest_id: String,
    pub district_id: String,
    pub name: String,
    pub election_type: String,
    pub type_name: String,
    /// 対象の都道府県の表示名。
    pub prefectures: Vec<String>,
    /// 得票の多い順（同数なら、選挙データの候補者の並び順）。0 票の候補者も含む。
    pub candidates: Vec<CandidateVotes>,
    /// 白票（無効票）: 選挙区の候補者ではない票。
    pub blank: u64,
    /// 有効票（候補者への票の合計）。
    pub valid: u64,
    /// 合計（有効票 + 白票 = チェーン内の票数）。
    pub total: u64,
    /// 投票済み者数（participation）。突合が済んでいれば `total` と等しい。
    pub participation: u64,
}

/// 都道府県別・選挙の種類別の合計。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupTotal {
    pub key: String,
    pub name: String,
    /// 複数の都道府県にまたがる選挙区（合区・比例ブロック・全国）。都道府県別のときだけ意味がある。
    pub wide: bool,
    /// 含まれる選挙区の数。
    pub districts: u64,
    pub valid: u64,
    pub blank: u64,
    pub total: u64,
    pub participation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub election_id: String,
    pub election_name: String,
    /// 表示順（選挙の種類の順、次に選挙区の順）。
    pub districts: Vec<DistrictTally>,
    /// 単独の都道府県（コード順）の後に、複数の都道府県にまたがる選挙区（表示順）。
    pub prefectures: Vec<GroupTotal>,
    pub types: Vec<GroupTotal>,
}

/// 検証済みのシャードの票を、選挙マスタに従って集計する。
///
/// `rows` は突合の行（participation の出所）。チェーンに、選挙マスタに無い投票用紙の票があれば `Err`。
pub fn compute(
    election: &Election,
    reports: &[&ShardReport],
    rows: &[ContestRow],
) -> Result<Tally, TallyError> {
    // シャードをまたいで、投票用紙・候補者ごとの票数を合算する（借用だけ。票のデータはコピーしない）。
    let mut votes: HashMap<&ContestId, BTreeMap<&CandidateId, u64>> = HashMap::new();
    for report in reports {
        for (contest, per_candidate) in &report.votes {
            if election.contest(contest).is_none() {
                return Err(TallyError::UnknownContest(contest.to_string()));
            }
            let merged = votes.entry(contest).or_default();
            for (candidate, n) in per_candidate {
                *merged.entry(candidate).or_default() += n;
            }
        }
    }
    let participation: HashMap<&str, u64> = rows
        .iter()
        .map(|r| (r.contest_id.as_str(), r.participation))
        .collect();

    let mut districts = Vec::with_capacity(election.contests().len());
    for contest in election.contests() {
        let cast = votes.get(&contest.id);
        let mut candidates: Vec<CandidateVotes> = contest
            .candidates
            .iter()
            .map(|c| CandidateVotes {
                candidate_id: c.id.to_string(),
                name: c.name.clone(),
                party: c.party.clone(),
                votes: cast.and_then(|m| m.get(&c.id)).copied().unwrap_or(0),
            })
            .collect();
        let valid: u64 = candidates.iter().map(|c| c.votes).sum();
        let total: u64 = cast.map_or(0, |m| m.values().sum());
        // 安定ソート: 同数なら、選挙データの並び順のまま。
        candidates.sort_by_key(|c| std::cmp::Reverse(c.votes));
        let type_name = election
            .election_type(&contest.district.election_type)
            .map_or_else(
                || contest.district.election_type.to_string(),
                |t| t.name.clone(),
            );
        districts.push(DistrictTally {
            contest_id: contest.id.to_string(),
            district_id: contest.district.id.to_string(),
            name: contest.district.name.clone(),
            election_type: contest.district.election_type.to_string(),
            type_name,
            prefectures: contest
                .district
                .prefectures
                .iter()
                .map(|code| prefecture_name(code).unwrap_or(code.as_str()).to_string())
                .collect(),
            candidates,
            blank: total.saturating_sub(valid),
            valid,
            total,
            participation: participation
                .get(contest.id.to_string().as_str())
                .copied()
                .unwrap_or(0),
        });
    }

    let prefectures = prefecture_totals(election, &districts);
    let types = type_totals(election, &districts);
    Ok(Tally {
        election_id: election.id().to_string(),
        election_name: election.name().to_string(),
        districts,
        prefectures,
        types,
    })
}

fn add(group: &mut GroupTotal, d: &DistrictTally) {
    group.districts += 1;
    group.valid += d.valid;
    group.blank += d.blank;
    group.total += d.total;
    group.participation += d.participation;
}

fn new_group(key: &str, name: &str, wide: bool) -> GroupTotal {
    GroupTotal {
        key: key.to_string(),
        name: name.to_string(),
        wide,
        districts: 0,
        valid: 0,
        blank: 0,
        total: 0,
        participation: 0,
    }
}

/// 単独の都道府県の選挙区は、その都道府県へ。複数の都道府県にまたがる選挙区（合区・比例ブロック・全国）は、
/// 按分できないので、選挙区ごとに 1 行にする。
fn prefecture_totals(election: &Election, districts: &[DistrictTally]) -> Vec<GroupTotal> {
    let mut single: BTreeMap<String, GroupTotal> = BTreeMap::new();
    let mut wide = Vec::new();
    for (contest, d) in election.contests().iter().zip(districts) {
        match contest.district.prefectures.as_slice() {
            [code] => {
                let group = single
                    .entry(code.clone())
                    .or_insert_with(|| new_group(code, &d.prefectures[0], false));
                add(group, d);
            }
            _ => {
                let mut group = new_group(&d.district_id, &d.name, true);
                add(&mut group, d);
                wide.push(group);
            }
        }
    }
    single.into_values().chain(wide).collect()
}

fn type_totals(election: &Election, districts: &[DistrictTally]) -> Vec<GroupTotal> {
    let mut groups: Vec<GroupTotal> = election
        .types()
        .iter()
        .map(|t| new_group(t.code.as_ref(), &t.name, false))
        .collect();
    for d in districts {
        if let Some(group) = groups.iter_mut().find(|g| g.key == d.election_type) {
            add(group, d);
        }
    }
    groups
}

impl Tally {
    /// 全体の合計（選挙区の合計を足したもの）。
    pub fn grand_total(&self) -> GroupTotal {
        let mut all = new_group("all", "全体", false);
        for d in &self.districts {
            add(&mut all, d);
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use domain::election::{Candidate, District, ElectionType, VotingMethod};
    use domain::{DistrictId, ElectionId, ElectionTypeCode};

    use super::*;

    fn code(s: &str) -> ElectionTypeCode {
        ElectionTypeCode::new(s).expect("code")
    }

    fn district(id: &str, ty: &str, name: &str, prefs: &[&str], order: u32) -> District {
        District {
            id: DistrictId::new(id).expect("district id"),
            election_type: code(ty),
            name: name.to_string(),
            prefectures: prefs.iter().map(|p| p.to_string()).collect(),
            order,
        }
    }

    fn cand(district: &str, seq: u32, name: &str, party: &str) -> Candidate {
        Candidate {
            id: CandidateId::parse(&format!("{district}.c{seq}")).expect("candidate id"),
            name: name.to_string(),
            party: party.to_string(),
            profile: String::new(),
        }
    }

    /// 東京 1 区・東京 2 区（小選挙区）、鳥取・島根の合区（参議院選挙区）、全国（比例）。
    fn election() -> Election {
        let types = vec![
            ElectionType {
                code: code("smd"),
                name: "小選挙区".to_string(),
                order: 1,
                method: VotingMethod::SingleChoice,
            },
            ElectionType {
                code: code("sangiin"),
                name: "参議院選挙区".to_string(),
                order: 2,
                method: VotingMethod::SingleChoice,
            },
        ];
        let districts = vec![
            district("smd.13.01", "smd", "東京1区", &["13"], 1),
            district("smd.13.02", "smd", "東京2区", &["13"], 2),
            district("sangiin.31_32", "sangiin", "鳥取・島根", &["31", "32"], 1),
        ];
        let mut candidates = Vec::new();
        for (d, names) in [
            ("smd.13.01", ["甲", "乙", "丙"]),
            ("smd.13.02", ["丁", "戊", "己"]),
            ("sangiin.31_32", ["庚", "辛", "壬"]),
        ] {
            for (i, name) in names.iter().enumerate() {
                candidates.push(cand(d, u32::try_from(i + 1).expect("seq"), name, "党"));
            }
        }
        Election::new(
            ElectionId::new("e1").expect("election id"),
            "テスト選挙".to_string(),
            types,
            districts,
            candidates,
        )
        .expect("election")
    }

    fn report(votes: &[(&str, &str, u64)]) -> ShardReport {
        let mut v: BTreeMap<ContestId, BTreeMap<CandidateId, u64>> = BTreeMap::new();
        let mut contests: BTreeMap<ContestId, u64> = BTreeMap::new();
        for (district, candidate, n) in votes {
            let contest = ContestId::parse(&format!("e1/{district}")).expect("contest id");
            *contests.entry(contest.clone()).or_default() += n;
            v.entry(contest)
                .or_default()
                .insert(CandidateId::parse(candidate).expect("candidate"), *n);
        }
        ShardReport {
            shard: 0,
            blocks: 1,
            ballots: 0,
            block_hashes: Vec::new(),
            contests,
            votes: v,
            ballot_ids: Vec::new(),
            public_key: [0; 32],
        }
    }

    fn row(district: &str, participation: u64, sealed: u64) -> ContestRow {
        ContestRow {
            contest_id: format!("e1/{district}"),
            participation,
            sealed,
            pending: 0,
        }
    }

    #[test]
    fn candidates_are_sorted_by_votes_and_zero_vote_candidates_are_listed() {
        let e = election();
        let r = report(&[
            ("smd.13.01", "smd.13.01.c1", 2),
            ("smd.13.01", "smd.13.01.c3", 5),
        ]);
        let t = compute(&e, &[&r], &[row("smd.13.01", 7, 7)]).expect("tally");
        let d = &t.districts[0];
        let order: Vec<(&str, u64)> = d
            .candidates
            .iter()
            .map(|c| (c.name.as_str(), c.votes))
            .collect();
        // 丙 5 → 甲 2 → 乙 0（0 票も載る）。
        assert_eq!(order, [("丙", 5), ("甲", 2), ("乙", 0)]);
        assert_eq!((d.valid, d.blank, d.total, d.participation), (7, 0, 7, 7));
    }

    #[test]
    fn ties_keep_the_order_of_the_election_data() {
        let e = election();
        let r = report(&[
            ("smd.13.02", "smd.13.02.c2", 3),
            ("smd.13.02", "smd.13.02.c3", 3),
            ("smd.13.02", "smd.13.02.c1", 3),
        ]);
        let t = compute(&e, &[&r], &[row("smd.13.02", 9, 9)]).expect("tally");
        let names: Vec<&str> = t.districts[1]
            .candidates
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["丁", "戊", "己"]);
    }

    #[test]
    fn votes_are_merged_across_shards() {
        let e = election();
        let a = report(&[("smd.13.01", "smd.13.01.c1", 2)]);
        let b = report(&[("smd.13.01", "smd.13.01.c1", 3)]);
        let t = compute(&e, &[&a, &b], &[row("smd.13.01", 5, 5)]).expect("tally");
        assert_eq!(t.districts[0].candidates[0].votes, 5);
    }

    #[test]
    fn a_vote_for_someone_outside_the_contest_is_a_blank_ballot() {
        let e = election();
        // 東京 1 区の投票用紙に、東京 2 区の候補者への票（選挙区の候補者ではない）。
        let r = report(&[
            ("smd.13.01", "smd.13.01.c1", 1),
            ("smd.13.01", "smd.13.02.c1", 2),
        ]);
        let t = compute(&e, &[&r], &[row("smd.13.01", 3, 3)]).expect("tally");
        let d = &t.districts[0];
        assert_eq!((d.valid, d.blank, d.total), (1, 2, 3));
    }

    #[test]
    fn an_unknown_contest_in_the_chain_is_an_error() {
        let e = election();
        let mut r = report(&[]);
        let contest = ContestId::parse("e1/smd.99.99").expect("contest");
        r.votes.insert(
            contest,
            BTreeMap::from([(CandidateId::parse("smd.99.99.c1").expect("c"), 1)]),
        );
        assert_eq!(
            compute(&e, &[&r], &[]),
            Err(TallyError::UnknownContest("e1/smd.99.99".to_string()))
        );
    }

    #[test]
    fn contests_without_any_vote_are_listed_with_zero() {
        let e = election();
        let t = compute(&e, &[&report(&[])], &[]).expect("tally");
        assert_eq!(t.districts.len(), 3);
        assert!(t.districts.iter().all(|d| d.total == 0 && d.valid == 0));
        assert_eq!(t.grand_total().total, 0);
    }

    #[test]
    fn prefecture_totals_group_single_prefecture_districts_and_keep_wide_ones_apart() {
        let e = election();
        let r = report(&[
            ("smd.13.01", "smd.13.01.c1", 2),
            ("smd.13.02", "smd.13.02.c1", 3),
            ("sangiin.31_32", "sangiin.31_32.c1", 4),
        ]);
        let rows = [
            row("smd.13.01", 2, 2),
            row("smd.13.02", 3, 3),
            row("sangiin.31_32", 4, 4),
        ];
        let t = compute(&e, &[&r], &rows).expect("tally");
        let summary: Vec<(&str, bool, u64, u64)> = t
            .prefectures
            .iter()
            .map(|g| (g.name.as_str(), g.wide, g.districts, g.total))
            .collect();
        assert_eq!(
            summary,
            [("東京都", false, 2, 5), ("鳥取・島根", true, 1, 4)]
        );
        let types: Vec<(&str, u64)> = t.types.iter().map(|g| (g.name.as_str(), g.total)).collect();
        assert_eq!(types, [("小選挙区", 5), ("参議院選挙区", 4)]);
        let all = t.grand_total();
        assert_eq!((all.districts, all.total, all.participation), (3, 9, 9));
    }
}
