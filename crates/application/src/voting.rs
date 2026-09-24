//! 投票のユースケース。
//!
//! 有権者は、名簿（[`VoterRoll`]）で自分に属する選挙区の投票用紙だけを、表示順（選挙の種類の順、次に選挙区の順）で
//! 見て、投票できる。投票する順番の固定は、`ballot_status` の並びと画面の flow で担う（この層は順番を強制しない）。

use std::collections::HashSet;
use std::num::NonZeroU16;
use std::sync::Arc;

use domain::{
    Ballot, BallotId, Candidate, CandidateId, Contest, ContestId, ElectionTypeCode, VoterId,
    VotingMethod, shard_for,
};

use crate::ports::{
    BallotIdSource, CastError, ContestCounts, ElectionRepository, StoreError, VoteStore, VoterRoll,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("見つかりません")]
    ContestNotFound,
    #[error("指定した候補者は対象にいません")]
    InvalidCandidate,
    /// 白票を受け付けない選挙（open の時点で固定した `vote.allow_blank` が偽）で、白票が指定された。
    #[error("この選挙では白票を受け付けていません")]
    BlankNotAllowed,
    #[error("投票済みです")]
    AlreadyVoted,
    /// 名簿で、この有権者に属さない選挙区（名簿に無い有権者を含む）。
    #[error("この有権者の対象ではありません")]
    NotEligible,
    #[error("サービスが利用できません")]
    Unavailable,
}

impl From<StoreError> for ServiceError {
    fn from(_: StoreError) -> Self {
        Self::Unavailable
    }
}

impl From<CastError> for ServiceError {
    fn from(e: CastError) -> Self {
        match e {
            CastError::AlreadyVoted => Self::AlreadyVoted,
            CastError::Store(_) => Self::Unavailable,
        }
    }
}

/// 有権者から見た、投票用紙 1 枚の状況。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BallotStatus {
    pub contest_id: ContestId,
    /// 選挙区の名前。
    pub name: String,
    pub election_type: ElectionTypeCode,
    /// 選挙の種類の表示名。
    pub type_name: String,
    pub method: VotingMethod,
    pub voted: bool,
}

/// UUIDv4 の `ballot_id` を乱数から作る。
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomBallotIds;

impl BallotIdSource for RandomBallotIds {
    fn next_ballot_id(&self) -> BallotId {
        BallotId::from_random_bytes(rand::random())
    }
}

pub struct VotingService {
    elections: Arc<dyn ElectionRepository>,
    roll: Arc<dyn VoterRoll>,
    store: Arc<dyn VoteStore>,
    ids: Arc<dyn BallotIdSource>,
    shard_count: NonZeroU16,
}

impl VotingService {
    pub fn new(
        elections: Arc<dyn ElectionRepository>,
        roll: Arc<dyn VoterRoll>,
        store: Arc<dyn VoteStore>,
        ids: Arc<dyn BallotIdSource>,
        shard_count: NonZeroU16,
    ) -> Self {
        Self {
            elections,
            roll,
            store,
            ids,
            shard_count,
        }
    }

    /// 有権者に関係する投票用紙だけを、表示順（選挙の種類の順、次に選挙区の順）で返す。
    /// 名簿に無い有権者には、空のリスト。
    pub async fn ballot_status(&self, voter: &VoterId) -> Result<Vec<BallotStatus>, ServiceError> {
        let election = self.elections.election().await?;
        let districts = self.roll.districts_of(voter).await?.unwrap_or_default();
        let mut mine: Vec<(usize, &Contest)> = districts
            .iter()
            .filter_map(|district| election.contest_for_district(district))
            .filter_map(|contest| Some((election.contest_position(&contest.id)?, contest)))
            .collect();
        mine.sort_by_key(|(position, _)| *position);
        let voted: HashSet<ContestId> = self
            .store
            .voted_contests(voter)
            .await?
            .into_iter()
            .collect();
        Ok(mine
            .into_iter()
            .map(|(_, contest)| {
                let election_type = &contest.district.election_type;
                let (type_name, method) = election.election_type(election_type).map_or_else(
                    || (election_type.to_string(), VotingMethod::SingleChoice),
                    |t| (t.name.clone(), t.method),
                );
                BallotStatus {
                    contest_id: contest.id.clone(),
                    name: contest.district.name.clone(),
                    election_type: election_type.clone(),
                    type_name,
                    method,
                    voted: voted.contains(&contest.id),
                }
            })
            .collect())
    }

    /// 有権者の投票用紙 `contest` の候補者。有権者の対象でなければ `NotEligible`。
    pub async fn candidates(
        &self,
        voter: &VoterId,
        contest: &ContestId,
    ) -> Result<Vec<Candidate>, ServiceError> {
        let election = self.elections.election().await?;
        let found = election
            .contest(contest)
            .ok_or(ServiceError::ContestNotFound)?;
        self.ensure_eligible(voter, found).await?;
        Ok(found.candidates.clone())
    }

    /// 1 票を投じる。`ballot_id` はここで新規に採番し、`voter` とは無関係な乱数にする。
    ///
    /// `candidate` が白票（[`CandidateId::Blank`]）のときは、`allow_blank`（open の時点で固定した選挙のルール）が
    /// 真のときだけ受け付ける。
    pub async fn cast_vote(
        &self,
        voter: &VoterId,
        contest: ContestId,
        candidate: CandidateId,
        allow_blank: bool,
    ) -> Result<(), ServiceError> {
        let election = self.elections.election().await?;
        let found = election
            .contest(&contest)
            .ok_or(ServiceError::ContestNotFound)?;
        self.ensure_eligible(voter, found).await?;
        if !found.accepts(&candidate, allow_blank) {
            return Err(if candidate.is_blank() {
                ServiceError::BlankNotAllowed
            } else {
                ServiceError::InvalidCandidate
            });
        }

        let ballot = Ballot {
            ballot_id: self.ids.next_ballot_id(),
            contest_id: contest,
            candidate_id: candidate,
        };
        // シャードは ballot_id から決める（voter_id からは決めない）。
        let shard = shard_for(&ballot.ballot_id, self.shard_count);
        self.store.cast(voter, shard, ballot).await?;
        Ok(())
    }

    /// 名簿で、この投票用紙の選挙区が有権者に属していること。
    async fn ensure_eligible(
        &self,
        voter: &VoterId,
        contest: &Contest,
    ) -> Result<(), ServiceError> {
        let districts = self.roll.districts_of(voter).await?.unwrap_or_default();
        if districts.contains(&contest.district.id) {
            Ok(())
        } else {
            Err(ServiceError::NotEligible)
        }
    }

    /// 投票用紙ごとの participation 件数と未封印の票の件数（監査用の集計値のみ）。
    pub async fn audit_counts(&self) -> Result<Vec<ContestCounts>, ServiceError> {
        Ok(self.store.audit_counts().await?)
    }

    /// シャードごとの未封印件数（件数のみ）。
    pub async fn pending_by_shard(&self) -> Result<Vec<usize>, ServiceError> {
        Ok(self.store.pending_by_shard().await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use domain::{District, DistrictId, Election, ElectionId, ElectionType, ShardId};

    use super::*;

    struct FixedElection(Arc<Election>);

    #[async_trait]
    impl ElectionRepository for FixedElection {
        async fn election(&self) -> Result<Arc<Election>, StoreError> {
            Ok(self.0.clone())
        }
    }

    struct FixedRoll(HashMap<VoterId, Vec<DistrictId>>);

    #[async_trait]
    impl VoterRoll for FixedRoll {
        async fn districts_of(
            &self,
            voter: &VoterId,
        ) -> Result<Option<Vec<DistrictId>>, StoreError> {
            Ok(self.0.get(voter).cloned())
        }
    }

    /// 呼び出しを記録するだけのストア。`fail` なら常に障害を返す。`voted` は投票済みの投票用紙。
    #[derive(Default)]
    struct RecordingStore {
        fail: bool,
        voted: Vec<ContestId>,
        casts: Mutex<Vec<(ShardId, Ballot)>>,
    }

    #[async_trait]
    impl VoteStore for RecordingStore {
        async fn cast(
            &self,
            _voter: &VoterId,
            shard: ShardId,
            ballot: Ballot,
        ) -> Result<(), CastError> {
            if self.fail {
                return Err(StoreError::Unavailable.into());
            }
            self.casts.lock().expect("test lock").push((shard, ballot));
            Ok(())
        }
        async fn voted_contests(&self, _: &VoterId) -> Result<Vec<ContestId>, StoreError> {
            Ok(self.voted.clone())
        }
        async fn pending_by_shard(&self) -> Result<Vec<usize>, StoreError> {
            Ok(vec![])
        }
        async fn audit_counts(&self) -> Result<Vec<ContestCounts>, StoreError> {
            Ok(vec![])
        }
    }

    struct FixedId(BallotId);

    impl BallotIdSource for FixedId {
        fn next_ballot_id(&self) -> BallotId {
            self.0
        }
    }

    fn district(id: &str, order: u32) -> District {
        let id = DistrictId::new(id).expect("valid");
        District {
            election_type: ElectionTypeCode::new(id.type_segment()).expect("valid"),
            name: format!("選挙区 {id}"),
            prefectures: vec!["13".to_string()],
            order,
            id,
        }
    }

    fn candidate(district: &str, seq: u64) -> Candidate {
        let district = DistrictId::new(district).expect("valid");
        Candidate {
            id: domain::CandidateCode::new(&district, seq).expect("valid"),
            name: format!("候補 {seq}"),
            party: String::new(),
            profile: String::new(),
        }
    }

    fn contest_id(district: &str) -> ContestId {
        ContestId::parse(&format!("2026-general/{district}")).expect("valid")
    }

    fn cand(district: &str, seq: u64) -> CandidateId {
        CandidateId::new(&DistrictId::new(district).expect("valid"), seq).expect("valid")
    }

    /// 知事（表示順 10）と小選挙区（20）。alice は東京 1 区と知事、bob は東京 2 区だけ。
    fn service(store: Arc<RecordingStore>) -> VotingService {
        let etype = |code: &str, order: u32| ElectionType {
            code: ElectionTypeCode::new(code).expect("valid"),
            name: format!("種類 {code}"),
            order,
            method: VotingMethod::SingleChoice,
        };
        let election = Election::new(
            ElectionId::new("2026-general").expect("valid"),
            "選挙".to_string(),
            vec![etype("shugiin_smd", 20), etype("governor", 10)],
            vec![
                district("shugiin_smd.13.02", 2),
                district("shugiin_smd.13.01", 1),
                district("governor.13", 1),
            ],
            vec![
                candidate("shugiin_smd.13.01", 1),
                candidate("shugiin_smd.13.01", 2),
                candidate("shugiin_smd.13.02", 1),
                candidate("governor.13", 1),
            ],
        )
        .expect("valid");
        let roll = HashMap::from([
            (
                VoterId::new("alice").expect("valid"),
                vec![
                    DistrictId::new("shugiin_smd.13.01").expect("valid"),
                    DistrictId::new("governor.13").expect("valid"),
                ],
            ),
            (
                VoterId::new("bob").expect("valid"),
                vec![DistrictId::new("shugiin_smd.13.02").expect("valid")],
            ),
        ]);
        VotingService::new(
            Arc::new(FixedElection(Arc::new(election))),
            Arc::new(FixedRoll(roll)),
            store,
            Arc::new(FixedId(BallotId::from_random_bytes([7; 16]))),
            NonZeroU16::new(4).expect("non-zero"),
        )
    }

    fn alice() -> VoterId {
        VoterId::new("alice").expect("valid")
    }

    #[tokio::test]
    async fn cast_stores_ballot_in_shard_derived_from_ballot_id() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        svc.cast_vote(
            &alice(),
            contest_id("shugiin_smd.13.01"),
            cand("shugiin_smd.13.01", 2),
            true,
        )
        .await
        .expect("cast");

        let casts = store.casts.lock().expect("test lock");
        let (shard, ballot) = &casts[0];
        assert_eq!(ballot.contest_id, contest_id("shugiin_smd.13.01"));
        assert_eq!(ballot.candidate_id, cand("shugiin_smd.13.01", 2));
        assert!(ballot.ballot_id.is_v4());
        assert_eq!(
            *shard,
            shard_for(&ballot.ballot_id, NonZeroU16::new(4).expect("non-zero"))
        );
    }

    #[tokio::test]
    async fn rejects_unknown_contest_foreign_candidate_and_other_districts() {
        let svc = service(Arc::new(RecordingStore::default()));
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("governor.99"),
                cand("governor.99", 1),
                true
            )
            .await,
            Err(ServiceError::ContestNotFound)
        );
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("shugiin_smd.13.01"),
                cand("shugiin_smd.13.01", 9),
                true
            )
            .await,
            Err(ServiceError::InvalidCandidate)
        );
        // 別の選挙区の候補者。
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("shugiin_smd.13.01"),
                cand("shugiin_smd.13.02", 1),
                true
            )
            .await,
            Err(ServiceError::InvalidCandidate)
        );
        assert_eq!(
            svc.candidates(&alice(), &contest_id("governor.99")).await,
            Err(ServiceError::ContestNotFound)
        );
    }

    #[tokio::test]
    async fn a_blank_vote_is_stored_as_blank_only_when_blank_is_allowed() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        // 白票を受け付けない選挙: 拒否して、何も保存しない。
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("governor.13"),
                CandidateId::Blank,
                false
            )
            .await,
            Err(ServiceError::BlankNotAllowed)
        );
        assert!(store.casts.lock().expect("test lock").is_empty());
        // 受け付ける選挙: 候補者とは別の、白票の票として保存する。
        svc.cast_vote(
            &alice(),
            contest_id("governor.13"),
            CandidateId::Blank,
            true,
        )
        .await
        .expect("blank vote");
        let casts = store.casts.lock().expect("test lock");
        assert_eq!(casts.len(), 1);
        assert_eq!(casts[0].1.candidate_id, CandidateId::Blank);
        assert_eq!(casts[0].1.contest_id, contest_id("governor.13"));
    }

    #[tokio::test]
    async fn voters_cannot_vote_or_list_candidates_outside_their_districts() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        let bob = VoterId::new("bob").expect("valid");
        let stranger = VoterId::new("stranger").expect("valid");
        // bob は東京 2 区の有権者で、東京 1 区・知事には投票できない。
        for (voter, district) in [
            (&bob, "shugiin_smd.13.01"),
            (&bob, "governor.13"),
            (&stranger, "governor.13"),
        ] {
            assert_eq!(
                svc.cast_vote(voter, contest_id(district), cand(district, 1), true)
                    .await,
                Err(ServiceError::NotEligible),
                "{voter:?} {district}"
            );
            assert_eq!(
                svc.candidates(voter, &contest_id(district)).await,
                Err(ServiceError::NotEligible)
            );
        }
        assert!(
            store.casts.lock().expect("test lock").is_empty(),
            "保存されていない"
        );
        assert!(
            svc.candidates(&bob, &contest_id("shugiin_smd.13.02"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn store_failure_becomes_unavailable() {
        let svc = service(Arc::new(RecordingStore {
            fail: true,
            ..Default::default()
        }));
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("governor.13"),
                cand("governor.13", 1),
                true
            )
            .await,
            Err(ServiceError::Unavailable)
        );
    }

    #[tokio::test]
    async fn ballot_status_lists_only_the_voters_ballots_in_display_order() {
        let svc = service(Arc::new(RecordingStore::default()));
        // alice: 知事（種類の表示順 10）が、小選挙区（20）より先。
        let status = svc.ballot_status(&alice()).await.expect("status");
        let ids: Vec<&str> = status.iter().map(|b| b.contest_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["2026-general/governor.13", "2026-general/shugiin_smd.13.01"]
        );
        assert!(status.iter().all(|b| !b.voted));
        assert_eq!(status[0].type_name, "種類 governor");
        assert_eq!(status[0].method, VotingMethod::SingleChoice);
        // bob は、東京 2 区だけ。
        let bob = svc
            .ballot_status(&VoterId::new("bob").expect("valid"))
            .await
            .expect("status");
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].contest_id.as_str(), "2026-general/shugiin_smd.13.02");
        // 名簿に無い有権者は、空。
        let stranger = svc
            .ballot_status(&VoterId::new("stranger").expect("valid"))
            .await
            .expect("status");
        assert!(stranger.is_empty());
    }

    #[tokio::test]
    async fn ballot_status_marks_voted_ballots_without_changing_the_order() {
        let svc = service(Arc::new(RecordingStore {
            voted: vec![contest_id("governor.13")],
            ..Default::default()
        }));
        let status = svc.ballot_status(&alice()).await.expect("status");
        let voted: Vec<(&str, bool)> = status
            .iter()
            .map(|b| (b.contest_id.as_str(), b.voted))
            .collect();
        assert_eq!(
            voted,
            vec![
                ("2026-general/governor.13", true),
                ("2026-general/shugiin_smd.13.01", false)
            ]
        );
    }

    #[test]
    fn random_ballot_ids_are_v4_and_distinct() {
        let a = RandomBallotIds.next_ballot_id();
        let b = RandomBallotIds.next_ballot_id();
        assert!(a.is_v4() && b.is_v4());
        assert_ne!(a, b);
    }
}
