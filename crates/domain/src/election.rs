//! 選挙マスタ（選挙・選挙の種類・選挙区・候補者）。読み取り専用のデータで、投票者の情報は含まない。
//!
//! ID の体系は [`crate::ids`]。1 つの選挙区には、1 つの選挙の 1 種類につき、投票用紙が 1 枚ある
//! （`contest_id` = `{election_id}/{district_id}`）。投票用紙の表示順は、選挙の種類の表示順、
//! 次に選挙区の表示順（同順なら選挙区 ID の辞書順）。

use std::collections::{HashMap, HashSet};

pub use crate::ids::{
    CandidateCode, CandidateId, ContestId, DistrictId, ElectionId, ElectionTypeCode, IdError,
    is_prefecture_code,
};

/// 投票方式。今回実装しているのは、候補者を 1 人選ぶ `SingleChoice` だけ。将来の拡張に備えて `enum` にしてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VotingMethod {
    /// 候補者を 1 人選ぶ。
    SingleChoice,
}

impl VotingMethod {
    /// 設定・データファイルでの名前。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleChoice => "single_choice",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "single_choice" => Some(Self::SingleChoice),
            _ => None,
        }
    }
}

/// 選挙の種類（衆議院小選挙区、知事選挙など）。表示名・表示順・投票方式を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionType {
    pub code: ElectionTypeCode,
    pub name: String,
    /// 表示順（小さいほど先）。
    pub order: u32,
    pub method: VotingMethod,
}

/// 選挙区。合区のように、1 つの選挙区が複数の都道府県にまたがることがあるので、対象の都道府県はリストで持つ
/// （ID は変えない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct District {
    pub id: DistrictId,
    pub election_type: ElectionTypeCode,
    pub name: String,
    /// 対象の都道府県（JIS X 0401 の 2 桁コード）。1 件以上。
    pub prefectures: Vec<String>,
    /// 同じ種類の中での表示順（小さいほど先）。
    pub order: u32,
}

/// 候補者。氏名・政党・略歴は属性（将来の候補者詳細画面で使う）。
/// ID は候補者コード（[`CandidateCode`]）なので、白票（[`CandidateId::Blank`]）は型の上で候補者になれない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: CandidateCode,
    pub name: String,
    pub party: String,
    pub profile: String,
}

/// 投票用紙 1 枚（選挙 × 選挙区）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contest {
    pub id: ContestId,
    pub district: District,
    pub candidates: Vec<Candidate>,
}

impl Contest {
    pub fn has_candidate(&self, id: &CandidateCode) -> bool {
        self.candidates.iter().any(|c| &c.id == id)
    }

    /// この投票用紙の投票先として受け付けられるか。白票は `allow_blank`（open の時点で固定した
    /// 選挙のルール `vote.allow_blank`）が真のときだけ、候補者はこの選挙区の候補者のときだけ。
    pub fn accepts(&self, choice: &CandidateId, allow_blank: bool) -> bool {
        match choice {
            CandidateId::Blank => allow_blank,
            CandidateId::Candidate(code) => self.has_candidate(code),
        }
    }
}

/// 選挙マスタの検証エラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElectionError {
    #[error("選挙の種類が 1 件もありません")]
    NoTypes,
    #[error("選挙区が 1 件もありません")]
    NoDistricts,
    #[error("選挙の種類 {0} が重複しています")]
    DuplicateType(ElectionTypeCode),
    #[error("選挙区 {0} が重複しています")]
    DuplicateDistrict(DistrictId),
    #[error("候補者 {0} が重複しています")]
    DuplicateCandidate(CandidateCode),
    #[error("選挙区 {district} の選挙の種類 {election_type} が定義されていません")]
    UnknownType {
        district: DistrictId,
        election_type: ElectionTypeCode,
    },
    #[error("選挙区 {district} の ID が、選挙の種類 {election_type} で始まっていません")]
    DistrictTypeMismatch {
        district: DistrictId,
        election_type: ElectionTypeCode,
    },
    #[error("選挙区 {0} の対象の都道府県がありません")]
    NoPrefectures(DistrictId),
    #[error("選挙区 {district} の都道府県コード {code:?} が不正です（01〜47）")]
    InvalidPrefecture { district: DistrictId, code: String },
    #[error("候補者 {candidate} が、存在しない選挙区を参照しています")]
    UnknownDistrict { candidate: CandidateCode },
    #[error("選挙区 {0} に候補者がいません")]
    EmptyDistrict(DistrictId),
}

/// 選挙。ID の重複、存在しない選挙区・選挙の種類の参照などは、構築時に検出する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Election {
    id: ElectionId,
    name: String,
    /// 表示順。
    types: Vec<ElectionType>,
    /// 表示順（種類の順、次に選挙区の順）。
    contests: Vec<Contest>,
    contest_index: HashMap<ContestId, usize>,
    district_index: HashMap<DistrictId, usize>,
}

impl Election {
    /// 選挙を組み立てて検証する。`candidates` は、各候補者が属する選挙区を、自分の ID（`district_part`）で示す。
    pub fn new(
        id: ElectionId,
        name: String,
        mut types: Vec<ElectionType>,
        districts: Vec<District>,
        candidates: Vec<Candidate>,
    ) -> Result<Self, ElectionError> {
        if types.is_empty() {
            return Err(ElectionError::NoTypes);
        }
        if districts.is_empty() {
            return Err(ElectionError::NoDistricts);
        }
        let mut seen_types = HashSet::new();
        for t in &types {
            if !seen_types.insert(t.code.clone()) {
                return Err(ElectionError::DuplicateType(t.code.clone()));
            }
        }
        types.sort_by(|a, b| (a.order, &a.code).cmp(&(b.order, &b.code)));
        let type_order: HashMap<&ElectionTypeCode, u32> =
            types.iter().map(|t| (&t.code, t.order)).collect();

        let mut seen_districts = HashSet::new();
        for d in &districts {
            if !seen_districts.insert(d.id.clone()) {
                return Err(ElectionError::DuplicateDistrict(d.id.clone()));
            }
            if !type_order.contains_key(&d.election_type) {
                return Err(ElectionError::UnknownType {
                    district: d.id.clone(),
                    election_type: d.election_type.clone(),
                });
            }
            if d.id.type_segment() != d.election_type.as_str() {
                return Err(ElectionError::DistrictTypeMismatch {
                    district: d.id.clone(),
                    election_type: d.election_type.clone(),
                });
            }
            if d.prefectures.is_empty() {
                return Err(ElectionError::NoPrefectures(d.id.clone()));
            }
            if let Some(code) = d.prefectures.iter().find(|c| !is_prefecture_code(c)) {
                return Err(ElectionError::InvalidPrefecture {
                    district: d.id.clone(),
                    code: code.clone(),
                });
            }
        }

        // 表示順に並べた投票用紙。候補者は、選挙区ごとに、入力の順で持つ。
        let mut sorted: Vec<District> = districts;
        sorted.sort_by(|a, b| {
            let key = |d: &District| (type_order[&d.election_type], d.order, d.id.clone());
            key(a).cmp(&key(b))
        });
        let mut contests: Vec<Contest> = sorted
            .into_iter()
            .map(|district| Contest {
                id: ContestId::new(&id, &district.id),
                district,
                candidates: Vec::new(),
            })
            .collect();
        let district_index: HashMap<DistrictId, usize> = contests
            .iter()
            .enumerate()
            .map(|(i, c)| (c.district.id.clone(), i))
            .collect();
        let contest_index: HashMap<ContestId, usize> = contests
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id.clone(), i))
            .collect();

        let mut seen_candidates = HashSet::new();
        for candidate in candidates {
            if !seen_candidates.insert(candidate.id.clone()) {
                return Err(ElectionError::DuplicateCandidate(candidate.id));
            }
            let district = DistrictId::new(candidate.id.district_part()).ok();
            let Some(&i) = district.as_ref().and_then(|d| district_index.get(d)) else {
                return Err(ElectionError::UnknownDistrict {
                    candidate: candidate.id,
                });
            };
            contests[i].candidates.push(candidate);
        }
        if let Some(empty) = contests.iter().find(|c| c.candidates.is_empty()) {
            return Err(ElectionError::EmptyDistrict(empty.district.id.clone()));
        }
        Ok(Self {
            id,
            name,
            types,
            contests,
            contest_index,
            district_index,
        })
    }

    pub fn id(&self) -> &ElectionId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 選挙の種類（表示順）。
    pub fn types(&self) -> &[ElectionType] {
        &self.types
    }

    pub fn election_type(&self, code: &ElectionTypeCode) -> Option<&ElectionType> {
        self.types.iter().find(|t| &t.code == code)
    }

    /// 投票用紙（表示順: 選挙の種類の順、次に選挙区の順）。
    pub fn contests(&self) -> &[Contest] {
        &self.contests
    }

    pub fn contest(&self, id: &ContestId) -> Option<&Contest> {
        self.contest_index.get(id).map(|&i| &self.contests[i])
    }

    /// 表示順の中での位置（小さいほど先）。
    pub fn contest_position(&self, id: &ContestId) -> Option<usize> {
        self.contest_index.get(id).copied()
    }

    pub fn contest_for_district(&self, district: &DistrictId) -> Option<&Contest> {
        self.district_index
            .get(district)
            .map(|&i| &self.contests[i])
    }

    pub fn candidate_count(&self) -> usize {
        self.contests.iter().map(|c| c.candidates.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn etype(code: &str, order: u32) -> ElectionType {
        ElectionType {
            code: ElectionTypeCode::new(code).expect("valid"),
            name: format!("種類 {code}"),
            order,
            method: VotingMethod::SingleChoice,
        }
    }

    fn district(id: &str, order: u32, prefectures: &[&str]) -> District {
        let id = DistrictId::new(id).expect("valid");
        District {
            election_type: ElectionTypeCode::new(id.type_segment()).expect("valid"),
            name: format!("選挙区 {id}"),
            prefectures: prefectures.iter().map(|p| (*p).to_string()).collect(),
            order,
            id,
        }
    }

    fn candidate(district: &str, seq: u64) -> Candidate {
        let district = DistrictId::new(district).expect("valid");
        Candidate {
            id: CandidateCode::new(&district, seq).expect("valid"),
            name: format!("候補者 {seq}"),
            party: "無所属".to_string(),
            profile: String::new(),
        }
    }

    fn build(
        types: Vec<ElectionType>,
        districts: Vec<District>,
        candidates: Vec<Candidate>,
    ) -> Result<Election, ElectionError> {
        Election::new(
            ElectionId::new("2026-general").expect("valid"),
            "選挙".to_string(),
            types,
            districts,
            candidates,
        )
    }

    fn base_types() -> Vec<ElectionType> {
        vec![etype("shugiin_smd", 20), etype("governor", 10)]
    }

    fn base_districts() -> Vec<District> {
        vec![
            district("shugiin_smd.13.02", 2, &["13"]),
            district("shugiin_smd.13.01", 1, &["13"]),
            district("governor.13", 1, &["13"]),
        ]
    }

    fn base_candidates() -> Vec<Candidate> {
        vec![
            candidate("shugiin_smd.13.01", 1),
            candidate("shugiin_smd.13.01", 2),
            candidate("shugiin_smd.13.02", 1),
            candidate("governor.13", 1),
        ]
    }

    #[test]
    fn contests_are_in_display_order_types_first_then_districts() {
        let e = build(base_types(), base_districts(), base_candidates()).expect("valid");
        let ids: Vec<&str> = e.contests().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "2026-general/governor.13",
                "2026-general/shugiin_smd.13.01",
                "2026-general/shugiin_smd.13.02",
            ]
        );
        let types: Vec<&str> = e.types().iter().map(|t| t.code.as_str()).collect();
        assert_eq!(types, vec!["governor", "shugiin_smd"]);
        assert_eq!(e.candidate_count(), 4);
    }

    #[test]
    fn lookups_work_by_contest_district_and_position() {
        let e = build(base_types(), base_districts(), base_candidates()).expect("valid");
        let id = ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid");
        let contest = e.contest(&id).expect("exists");
        assert_eq!(contest.candidates.len(), 2);
        assert!(
            contest.has_candidate(&CandidateCode::parse("shugiin_smd.13.01.c2").expect("valid"))
        );
        assert!(
            !contest.has_candidate(&CandidateCode::parse("shugiin_smd.13.02.c1").expect("valid"))
        );
        assert_eq!(e.contest_position(&id), Some(1));
        let district = DistrictId::new("governor.13").expect("valid");
        assert_eq!(
            e.contest_for_district(&district).map(|c| c.id.as_str()),
            Some("2026-general/governor.13")
        );
        assert!(
            e.contest(&ContestId::parse("2026-general/governor.99").expect("valid"))
                .is_none()
        );
        assert!(
            e.election_type(&ElectionTypeCode::new("governor").expect("valid"))
                .is_some()
        );
    }

    #[test]
    fn a_contest_accepts_its_own_candidates_and_blank_only_when_allowed() {
        let e = build(base_types(), base_districts(), base_candidates()).expect("valid");
        let contest = e
            .contest(&ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"))
            .expect("exists");
        let own = CandidateId::parse("shugiin_smd.13.01.c1").expect("valid");
        let other = CandidateId::parse("shugiin_smd.13.02.c1").expect("valid");
        for allow_blank in [true, false] {
            assert!(contest.accepts(&own, allow_blank));
            assert!(!contest.accepts(&other, allow_blank));
        }
        assert!(contest.accepts(&CandidateId::Blank, true));
        assert!(!contest.accepts(&CandidateId::Blank, false));
    }

    #[test]
    fn a_district_may_span_several_prefectures_without_changing_its_id() {
        let mut districts = base_districts();
        districts.push(district("shugiin_smd.31_32.01", 9, &["31", "32"]));
        let mut candidates = base_candidates();
        candidates.push(candidate("shugiin_smd.31_32.01", 1));
        let e = build(base_types(), districts, candidates).expect("valid");
        let d = DistrictId::new("shugiin_smd.31_32.01").expect("valid");
        assert_eq!(
            e.contest_for_district(&d)
                .map(|c| c.district.prefectures.clone()),
            Some(vec!["31".to_string(), "32".to_string()])
        );
    }

    #[test]
    fn detects_duplicates() {
        let mut types = base_types();
        types.push(etype("governor", 5));
        assert!(matches!(
            build(types, base_districts(), base_candidates()),
            Err(ElectionError::DuplicateType(_))
        ));
        let mut districts = base_districts();
        districts.push(district("governor.13", 3, &["13"]));
        assert!(matches!(
            build(base_types(), districts, base_candidates()),
            Err(ElectionError::DuplicateDistrict(_))
        ));
        let mut candidates = base_candidates();
        candidates.push(candidate("governor.13", 1));
        assert!(matches!(
            build(base_types(), base_districts(), candidates),
            Err(ElectionError::DuplicateCandidate(_))
        ));
    }

    #[test]
    fn detects_dangling_references_and_empty_districts() {
        // 存在しない選挙区を参照する候補者。
        let mut candidates = base_candidates();
        candidates.push(candidate("governor.99", 1));
        assert!(matches!(
            build(base_types(), base_districts(), candidates),
            Err(ElectionError::UnknownDistrict { .. })
        ));
        // 定義されていない選挙の種類の選挙区。
        let mut districts = base_districts();
        districts.push(district("sangiin_pr.national", 1, &["13"]));
        let mut candidates = base_candidates();
        candidates.push(candidate("sangiin_pr.national", 1));
        assert!(matches!(
            build(base_types(), districts, candidates),
            Err(ElectionError::UnknownType { .. })
        ));
        // 候補者のいない選挙区。
        let candidates = base_candidates()
            .into_iter()
            .filter(|c| !c.id.as_str().starts_with("governor"))
            .collect();
        assert!(matches!(
            build(base_types(), base_districts(), candidates),
            Err(ElectionError::EmptyDistrict(_))
        ));
    }

    #[test]
    fn detects_inconsistent_district_attributes() {
        // ID の先頭が選挙の種類と一致しない。
        let mut districts = base_districts();
        districts[0].election_type = ElectionTypeCode::new("governor").expect("valid");
        assert!(matches!(
            build(base_types(), districts, base_candidates()),
            Err(ElectionError::DistrictTypeMismatch { .. })
        ));
        // 都道府県が無い / 不正。
        let mut districts = base_districts();
        districts[0].prefectures.clear();
        assert!(matches!(
            build(base_types(), districts, base_candidates()),
            Err(ElectionError::NoPrefectures(_))
        ));
        let mut districts = base_districts();
        districts[0].prefectures = vec!["48".to_string()];
        assert!(matches!(
            build(base_types(), districts, base_candidates()),
            Err(ElectionError::InvalidPrefecture { .. })
        ));
        assert_eq!(
            build(vec![], base_districts(), base_candidates()),
            Err(ElectionError::NoTypes)
        );
        assert_eq!(
            build(base_types(), vec![], base_candidates()),
            Err(ElectionError::NoDistricts)
        );
    }
}
