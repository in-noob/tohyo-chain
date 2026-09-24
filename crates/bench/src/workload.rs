//! 計測用の投票の割り当て。選挙データ（seed）の有権者と、その有権者に関係する投票用紙から作る「投票計画」。
//!
//! api は、名簿にある有権者の、属する選挙区の投票用紙にしか投票させない（対象外は 403）。だから、負荷も
//! 実在の有権者の実在の投票用紙に投じる。計画は、ラウンド `r` で「`r` 枚目の投票用紙を持つ有権者全員」を
//! 有権者 ID の順に並べる（種類が均等に回り、同じ (有権者, 投票用紙) は 2 度出ない）。i 番目の投票は `get(i)`。
//! 計画が尽きたら `None`（有権者数を増やして seedgen で作り直す）。

use domain::{CandidateId, ContestId, VoterId};
use seed::SeedData;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub voter: VoterId,
    pub contest: ContestId,
    pub candidate: CandidateId,
}

#[derive(Debug, Clone)]
pub struct Workload {
    items: Vec<WorkItem>,
}

impl Workload {
    pub fn from_seed(data: &SeedData) -> Self {
        // 有権者ごとの (投票用紙, 投じる候補者)。有権者 ID の順（決定的）、投票用紙は表示順。
        let mut voters: Vec<(&VoterId, Vec<(ContestId, CandidateId)>)> = data
            .voters
            .iter()
            .map(|(voter, districts)| {
                let mut contests: Vec<(usize, &domain::Contest)> = districts
                    .iter()
                    .filter_map(|d| data.election.contest_for_district(d))
                    .filter_map(|c| Some((data.election.contest_position(&c.id)?, c)))
                    .collect();
                contests.sort_by_key(|(position, _)| *position);
                (voter, contests)
            })
            .map(|(voter, contests)| {
                let picks = contests
                    .into_iter()
                    .enumerate()
                    .filter_map(|(k, (_, contest))| {
                        // 候補者は、有権者ごとに巡回させる（特定の候補者に偏らせない）。
                        let candidate = contest
                            .candidates
                            .get((voter_hash(voter) + k) % contest.candidates.len())?;
                        Some((contest.id.clone(), CandidateId::from(candidate.id.clone())))
                    })
                    .collect();
                (voter, picks)
            })
            .collect();
        voters.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let rounds = voters
            .iter()
            .map(|(_, picks)| picks.len())
            .max()
            .unwrap_or(0);
        let mut items = Vec::new();
        for round in 0..rounds {
            for (voter, picks) in &voters {
                if let Some((contest, candidate)) = picks.get(round) {
                    items.push(WorkItem {
                        voter: (*voter).clone(),
                        contest: contest.clone(),
                        candidate: candidate.clone(),
                    });
                }
            }
        }
        Self { items }
    }

    pub fn get(&self, index: u64) -> Option<&WorkItem> {
        usize::try_from(index).ok().and_then(|i| self.items.get(i))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 有権者 ID から決まる、小さな整数（候補者を巡回させるため）。
fn voter_hash(voter: &VoterId) -> usize {
    voter.as_str().bytes().fold(0usize, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(usize::from(b))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use domain::{
        Candidate, District, DistrictId, Election, ElectionId, ElectionType, ElectionTypeCode,
        VotingMethod,
    };
    use seed::VoterAssignments;

    use super::*;

    fn data() -> SeedData {
        let etype = |code: &str, order: u32| ElectionType {
            code: ElectionTypeCode::new(code).expect("valid"),
            name: code.to_string(),
            order,
            method: VotingMethod::SingleChoice,
        };
        let district = |id: &str, order: u32| {
            let id = DistrictId::new(id).expect("valid");
            District {
                election_type: ElectionTypeCode::new(id.type_segment()).expect("valid"),
                name: id.to_string(),
                prefectures: vec!["13".to_string()],
                order,
                id,
            }
        };
        let candidates = |d: &str, n: u64| -> Vec<Candidate> {
            let d = DistrictId::new(d).expect("valid");
            (1..=n)
                .map(|i| Candidate {
                    id: domain::CandidateCode::new(&d, i).expect("valid"),
                    name: format!("c{i}"),
                    party: String::new(),
                    profile: String::new(),
                })
                .collect()
        };
        let mut all = candidates("shugiin_smd.13.01", 4);
        all.extend(candidates("governor.13", 2));
        let election = Election::new(
            ElectionId::new("2026-general").expect("valid"),
            "e".to_string(),
            vec![etype("shugiin_smd", 10), etype("governor", 20)],
            vec![district("shugiin_smd.13.01", 1), district("governor.13", 1)],
            all,
        )
        .expect("valid");
        let d = |s: &str| DistrictId::new(s).expect("valid");
        let voters = VoterAssignments::from_map(HashMap::from([
            (
                VoterId::new("v-1").expect("valid"),
                vec![d("governor.13"), d("shugiin_smd.13.01")],
            ),
            (
                VoterId::new("v-2").expect("valid"),
                vec![d("shugiin_smd.13.01")],
            ),
        ]));
        SeedData { election, voters }
    }

    #[test]
    fn plan_has_one_item_per_voter_and_ballot_in_rounds() {
        let plan = Workload::from_seed(&data());
        assert_eq!(plan.len(), 3);
        let flat: Vec<(&str, &str)> = (0..3)
            .map(|i| {
                let w = plan.get(i).expect("item");
                (w.voter.as_str(), w.contest.district_part())
            })
            .collect();
        // ラウンド 0: 全有権者の最初の投票用紙（表示順: 小選挙区 → 知事）。ラウンド 1: v-1 の 2 枚目。
        assert_eq!(
            flat,
            vec![
                ("v-1", "shugiin_smd.13.01"),
                ("v-2", "shugiin_smd.13.01"),
                ("v-1", "governor.13")
            ]
        );
        assert!(plan.get(3).is_none(), "計画が尽きたら None");
    }

    #[test]
    fn every_voter_ballot_pair_is_unique_and_candidates_belong_to_their_ballot() {
        let data = data();
        let plan = Workload::from_seed(&data);
        let pairs: HashSet<(String, String)> = (0..plan.len() as u64)
            .filter_map(|i| plan.get(i))
            .map(|w| (w.voter.as_str().to_string(), w.contest.to_string()))
            .collect();
        assert_eq!(pairs.len(), plan.len());
        for i in 0..plan.len() as u64 {
            let w = plan.get(i).expect("item");
            let contest = data.election.contest(&w.contest).expect("exists");
            assert!(contest.accepts(&w.candidate, false), "{w:?}");
        }
    }
}
