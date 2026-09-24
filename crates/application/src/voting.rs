//! 投票のユースケース。
//!
//! 有権者は、名簿（[`VoterRoll`]）で自分に属する選挙区の投票用紙だけを、表示順（選挙の種類の順、次に選挙区の順）で
//! 見て、投票できる。投票する順番の固定は、`ballot_status` の並びと画面の flow で担う（この層は順番を強制しない）。
//!
//! 再投票（ADR 0022）: 選挙のルール `allow_revote`（open の時点で固定）が真なら、票に再投票の仮名 `slot`
//! （[`crate::revote::RevoteKey::slot`]）と `seq`・`supersedes` を付け、シャードを `hash(slot)` で決める。偽なら slot を
//! 記録せず（不要な情報は持たない）、シャードは `hash(ballot_id)`、2 回目の投票は拒否する。

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::sync::Arc;

use domain::{
    Ballot, BallotId, Candidate, CandidateId, Contest, ContestId, ElectionRules, ElectionTypeCode,
    RevoteLink, ShardId, VoterId, VotingMethod, shard_for, shard_for_slot,
};

use crate::ports::{
    BallotIdSource, CastError, ContestCounts, ElectionRepository, StoreError, VoteStore, VoterRoll,
};
use crate::revote::RevoteKeyVault;

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
    /// 再投票を認めない選挙（open の時点で固定した `vote.allow_revote` が偽）で、再投票が指定された。
    #[error("この選挙では再投票できません")]
    RevoteNotAllowed,
    /// 再投票の上限（`vote.max_revotes`）に達している。
    #[error("再投票の上限に達しています")]
    RevoteLimitReached,
    /// 同時に送られた別の再投票が先に受理された（何も保存していない）。
    #[error("同時に送られた別の再投票と競合しました")]
    RevoteConflict,
    /// まだ投票していない投票用紙に、再投票が指定された。
    #[error("まだ投票していません")]
    NotVotedYet,
    /// 名簿で、この有権者に属さない選挙区（名簿に無い有権者を含む）。
    #[error("この有権者の対象ではありません")]
    NotEligible,
    #[error("サービスが利用できません")]
    Unavailable,
    /// 再投票を認める選挙なのに、再投票の鍵（`secrets/revote_key`）が無い（破棄済み・未設定）。
    #[error("再投票の鍵がありません")]
    RevoteKeyUnavailable,
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
            CastError::RevoteConflict => Self::RevoteConflict,
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
    /// この投票用紙に受理された票の数（未投票は 0、初回の投票だけなら 1、再投票するたびに 1 増える）。
    /// 票の中身（前回の投票先）は含まない。
    pub ballots_cast: u32,
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
    /// 再投票の鍵（slot の計算に使う。締切の手続きで破棄される）。
    revote_keys: Arc<RevoteKeyVault>,
}

impl VotingService {
    pub fn new(
        elections: Arc<dyn ElectionRepository>,
        roll: Arc<dyn VoterRoll>,
        store: Arc<dyn VoteStore>,
        ids: Arc<dyn BallotIdSource>,
        shard_count: NonZeroU16,
        revote_keys: Arc<RevoteKeyVault>,
    ) -> Self {
        Self {
            elections,
            roll,
            store,
            ids,
            shard_count,
            revote_keys,
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
        let voted: HashMap<ContestId, u32> = self
            .store
            .voted_contests(voter)
            .await?
            .into_iter()
            .map(|v| (v.contest, v.ballots))
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
                    voted: voted.contains_key(&contest.id),
                    ballots_cast: voted.get(&contest.id).copied().unwrap_or(0),
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

    /// 初回の投票。`ballot_id` はここで新規に採番し、`voter` とは無関係な乱数にする。
    ///
    /// `rules` は open の時点で固定した選挙のルール。`candidate` が白票（[`CandidateId::Blank`]）のときは、
    /// `allow_blank` が真のときだけ受け付ける。`allow_revote` が真なら、票に slot（`seq = 1`）を付ける。
    pub async fn cast_vote(
        &self,
        voter: &VoterId,
        contest: ContestId,
        candidate: CandidateId,
        rules: ElectionRules,
    ) -> Result<(), ServiceError> {
        let election = self.elections.election().await?;
        let found = election
            .contest(&contest)
            .ok_or(ServiceError::ContestNotFound)?;
        self.ensure_eligible(voter, found).await?;
        self.ensure_accepts(found, &candidate, rules)?;

        let mut ballot = Ballot {
            ballot_id: self.ids.next_ballot_id(),
            contest_id: contest,
            candidate_id: candidate,
            revote: None,
        };
        let shard = if rules.allow_revote {
            let slot = self
                .revote_key()?
                .slot(election.id(), voter, &ballot.contest_id);
            ballot.revote = Some(RevoteLink {
                slot,
                seq: 1,
                supersedes: None,
            });
            // 同じ slot の全版を同じチェーンに入れる。
            shard_for_slot(&slot, self.shard_count)
        } else {
            // シャードは ballot_id から決める（voter_id からは決めない）。
            shard_for(&ballot.ballot_id, self.shard_count)
        };
        self.store.cast(voter, shard, ballot).await?;
        Ok(())
    }

    /// 再投票（投票期間中に、投票済みの投票用紙に投票し直す）。前回の投票内容は読まない・返さない。
    ///
    /// `expected` は、画面が見た受理済みの票の数（`ballots_cast`）。この値のときだけ再投票する（同じ画面から 2 つの
    /// 再投票が同時に送られても、1 件だけが成功する。二重送信で 2 回やり直したことにならない）。
    ///
    /// 1. participation の `seq`（受理した票の数 n）を読む。`expected` と違えば競合、上限（`max_revotes + 1`）なら拒否。
    /// 2. slot_state から、最後の票のハッシュを読む（`seq` が n でなければ、別の再投票と競合している）。
    /// 3. `seq = n + 1`・`supersedes` = 最後の票のハッシュ の票を、条件付き書き込み（`seq = n` のときだけ）で保存する。
    pub async fn revote(
        &self,
        voter: &VoterId,
        contest: ContestId,
        candidate: CandidateId,
        rules: ElectionRules,
        expected: u32,
    ) -> Result<(), ServiceError> {
        if !rules.allow_revote {
            return Err(ServiceError::RevoteNotAllowed);
        }
        let election = self.elections.election().await?;
        let found = election
            .contest(&contest)
            .ok_or(ServiceError::ContestNotFound)?;
        self.ensure_eligible(voter, found).await?;
        self.ensure_accepts(found, &candidate, rules)?;

        let prev_seq = self
            .store
            .voted_contests(voter)
            .await?
            .into_iter()
            .find(|v| v.contest == contest)
            .map(|v| v.ballots)
            .ok_or(ServiceError::NotVotedYet)?;
        if prev_seq != expected {
            return Err(ServiceError::RevoteConflict);
        }
        if prev_seq >= rules.max_seq() {
            return Err(ServiceError::RevoteLimitReached);
        }
        let slot = self.revote_key()?.slot(election.id(), voter, &contest);
        let last = match self.store.slot_state(&slot).await? {
            Some(state) if state.seq == prev_seq => state.last_ballot_hash,
            // participation と slot_state の seq が食い違う: 別の再投票が、ちょうど書き込み中（または、その途中で失敗した）。
            _ => return Err(ServiceError::RevoteConflict),
        };
        let ballot = Ballot {
            ballot_id: self.ids.next_ballot_id(),
            contest_id: contest,
            candidate_id: candidate,
            revote: Some(RevoteLink {
                slot,
                seq: prev_seq.saturating_add(1),
                supersedes: Some(last),
            }),
        };
        let shard: ShardId = shard_for_slot(&slot, self.shard_count);
        self.store.revote(voter, shard, ballot, prev_seq).await?;
        Ok(())
    }

    /// 投票先を、この投票用紙が受け付けるか（白票は `allow_blank` のときだけ）。
    fn ensure_accepts(
        &self,
        contest: &Contest,
        candidate: &CandidateId,
        rules: ElectionRules,
    ) -> Result<(), ServiceError> {
        if contest.accepts(candidate, rules.allow_blank) {
            Ok(())
        } else if candidate.is_blank() {
            Err(ServiceError::BlankNotAllowed)
        } else {
            Err(ServiceError::InvalidCandidate)
        }
    }

    /// 再投票の鍵。再投票を認める選挙なのに無い（破棄済み・未設定）なら、投票を受け付けない。
    fn revote_key(&self) -> Result<Arc<crate::revote::RevoteKey>, ServiceError> {
        self.revote_keys
            .key()
            .ok_or(ServiceError::RevoteKeyUnavailable)
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

    /// 呼び出しを記録するだけのストア。`fail` なら常に障害を返す。`voted` は投票済みの投票用紙（受理した票の数つき）。
    /// `slots` は slot_state、`revote_conflict` なら再投票の条件付き書き込みが負ける。
    #[derive(Default)]
    struct RecordingStore {
        fail: bool,
        voted: Vec<(ContestId, u32)>,
        slots: Mutex<HashMap<domain::Slot, crate::ports::SlotState>>,
        revote_conflict: bool,
        casts: Mutex<Vec<(ShardId, Ballot)>>,
        revotes: Mutex<Vec<(ShardId, Ballot, u32)>>,
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
            if let Some(link) = ballot.revote {
                self.slots.lock().expect("test lock").insert(
                    link.slot,
                    crate::ports::SlotState {
                        seq: link.seq,
                        last_ballot_hash: domain::ballot_hash(&ballot),
                    },
                );
            }
            self.casts.lock().expect("test lock").push((shard, ballot));
            Ok(())
        }
        async fn revote(
            &self,
            _voter: &VoterId,
            shard: ShardId,
            ballot: Ballot,
            prev_seq: u32,
        ) -> Result<(), CastError> {
            if self.revote_conflict {
                return Err(CastError::RevoteConflict);
            }
            self.revotes
                .lock()
                .expect("test lock")
                .push((shard, ballot, prev_seq));
            Ok(())
        }
        async fn voted_contests(
            &self,
            _: &VoterId,
        ) -> Result<Vec<crate::ports::VotedContest>, StoreError> {
            Ok(self
                .voted
                .iter()
                .map(|(contest, ballots)| crate::ports::VotedContest {
                    contest: contest.clone(),
                    ballots: *ballots,
                })
                .collect())
        }
        async fn slot_state(
            &self,
            slot: &domain::Slot,
        ) -> Result<Option<crate::ports::SlotState>, StoreError> {
            Ok(self.slots.lock().expect("test lock").get(slot).copied())
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
            Arc::new(RevoteKeyVault::in_memory(Some(
                crate::revote::RevoteKey::new([9; 32]),
            ))),
        )
    }

    /// 再投票を認めない選挙のルール（白票の可否だけを変える）。
    fn blank(allow_blank: bool) -> ElectionRules {
        ElectionRules {
            allow_blank,
            ..ElectionRules::default()
        }
    }

    /// 再投票を認める選挙のルール（上限 2 回）。
    fn revotes() -> ElectionRules {
        ElectionRules {
            allow_revote: true,
            max_revotes: 2,
            ..ElectionRules::default()
        }
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
            blank(true),
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
                blank(true)
            )
            .await,
            Err(ServiceError::ContestNotFound)
        );
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("shugiin_smd.13.01"),
                cand("shugiin_smd.13.01", 9),
                blank(true)
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
                blank(true)
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
                blank(false)
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
            blank(true),
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
                svc.cast_vote(voter, contest_id(district), cand(district, 1), blank(true))
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
                blank(true)
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
            voted: vec![(contest_id("governor.13"), 1)],
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
        assert_eq!(
            status.iter().map(|b| b.ballots_cast).collect::<Vec<_>>(),
            vec![1, 0]
        );
    }

    // --- 再投票（ADR 0022）---

    #[tokio::test]
    async fn without_revotes_no_slot_is_recorded_and_the_shard_comes_from_the_ballot_id() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        svc.cast_vote(
            &alice(),
            contest_id("governor.13"),
            cand("governor.13", 1),
            blank(true),
        )
        .await
        .expect("cast");
        // ロックの中身は複製して、ロックはこの文の終わりで手放す（await をまたいで持たない）。
        let (shard, ballot) = store.casts.lock().expect("test lock")[0].clone();
        assert_eq!(ballot.revote, None);
        assert_eq!(
            shard,
            shard_for(&ballot.ballot_id, NonZeroU16::new(4).expect("non-zero"))
        );
        assert_eq!(
            svc.revote(
                &alice(),
                contest_id("governor.13"),
                cand("governor.13", 1),
                blank(true),
                1
            )
            .await,
            Err(ServiceError::RevoteNotAllowed)
        );
    }

    #[tokio::test]
    async fn with_revotes_the_first_vote_carries_seq_1_and_the_shard_comes_from_the_slot() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        svc.cast_vote(
            &alice(),
            contest_id("governor.13"),
            cand("governor.13", 1),
            revotes(),
        )
        .await
        .expect("cast");
        let casts = store.casts.lock().expect("test lock");
        let link = casts[0].1.revote.expect("slot");
        assert_eq!((link.seq, link.supersedes), (1, None));
        let key = crate::revote::RevoteKey::new([9; 32]);
        let expected = key.slot(
            &domain::ElectionId::new("2026-general").expect("valid"),
            &alice(),
            &contest_id("governor.13"),
        );
        assert_eq!(link.slot, expected);
        assert_eq!(
            casts[0].0,
            shard_for_slot(&expected, NonZeroU16::new(4).expect("non-zero"))
        );
    }

    #[tokio::test]
    async fn a_revote_links_to_the_last_ballot_and_respects_the_limit() {
        let store = Arc::new(RecordingStore::default());
        let svc = service(store.clone());
        let contest = contest_id("governor.13");
        svc.cast_vote(&alice(), contest.clone(), cand("governor.13", 1), revotes())
            .await
            .expect("cast");
        let first = store.casts.lock().expect("test lock")[0].1.clone();

        // 投票済みの記録（seq=1）がある状態で、再投票（白票）。
        let store2 = Arc::new(RecordingStore {
            voted: vec![(contest.clone(), 1)],
            slots: Mutex::new(store.slots.lock().expect("test lock").clone()),
            ..Default::default()
        });
        let svc2 = service(store2.clone());
        svc2.revote(&alice(), contest.clone(), CandidateId::Blank, revotes(), 1)
            .await
            .expect("revote");
        let (shard, ballot, prev_seq) = store2.revotes.lock().expect("test lock")[0].clone();
        let link = ballot.revote.expect("slot");
        assert_eq!(prev_seq, 1);
        assert_eq!(link.seq, 2);
        assert_eq!(link.slot, first.revote.expect("slot").slot);
        assert_eq!(link.supersedes, Some(domain::ballot_hash(&first)));
        assert_eq!(shard, store.casts.lock().expect("test lock")[0].0);
        assert_eq!(ballot.candidate_id, CandidateId::Blank);
        assert_ne!(ballot.ballot_id, BallotId([0; 16]));

        // 上限（max_revotes=2 → seq は 3 まで）に達していたら拒否する。
        let full = service(Arc::new(RecordingStore {
            voted: vec![(contest.clone(), 3)],
            ..Default::default()
        }));
        assert_eq!(
            full.revote(&alice(), contest.clone(), CandidateId::Blank, revotes(), 3)
                .await,
            Err(ServiceError::RevoteLimitReached)
        );
        // まだ投票していない。
        assert_eq!(
            service(Arc::new(RecordingStore::default()))
                .revote(&alice(), contest.clone(), CandidateId::Blank, revotes(), 1)
                .await,
            Err(ServiceError::NotVotedYet)
        );
    }

    #[tokio::test]
    async fn a_revote_that_loses_the_race_is_a_conflict() {
        let contest = contest_id("governor.13");
        // 画面が見た票の数（1）が古い: 別の再投票が先に受理されて、もう 2。
        let stale = service(Arc::new(RecordingStore {
            voted: vec![(contest.clone(), 2)],
            ..Default::default()
        }));
        assert_eq!(
            stale
                .revote(&alice(), contest.clone(), CandidateId::Blank, revotes(), 1)
                .await,
            Err(ServiceError::RevoteConflict)
        );
        // slot_state が participation より遅れている（別の再投票の書き込み中）。
        let behind = service(Arc::new(RecordingStore {
            voted: vec![(contest.clone(), 2)],
            ..Default::default()
        }));
        assert_eq!(
            behind
                .revote(&alice(), contest.clone(), CandidateId::Blank, revotes(), 2)
                .await,
            Err(ServiceError::RevoteConflict)
        );
        // 条件付き書き込みに負けた。
        let store = Arc::new(RecordingStore::default());
        service(store.clone())
            .cast_vote(&alice(), contest.clone(), cand("governor.13", 1), revotes())
            .await
            .expect("cast");
        let lost = service(Arc::new(RecordingStore {
            voted: vec![(contest.clone(), 1)],
            slots: Mutex::new(store.slots.lock().expect("test lock").clone()),
            revote_conflict: true,
            ..Default::default()
        }));
        assert_eq!(
            lost.revote(&alice(), contest, CandidateId::Blank, revotes(), 1)
                .await,
            Err(ServiceError::RevoteConflict)
        );
    }

    #[tokio::test]
    async fn without_a_revote_key_a_revote_election_accepts_no_votes() {
        let svc = VotingService {
            revote_keys: Arc::new(RevoteKeyVault::none()),
            ..service(Arc::new(RecordingStore::default()))
        };
        assert_eq!(
            svc.cast_vote(
                &alice(),
                contest_id("governor.13"),
                cand("governor.13", 1),
                revotes()
            )
            .await,
            Err(ServiceError::RevoteKeyUnavailable)
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
