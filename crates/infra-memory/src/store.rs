//! インメモリのストア（`VoteStore` / `ChainRead` / `SealStore`）。
//!
//! 秘密投票のため、participation と ballot_pool は**別々の構造**で持ち、両者を結ぶキーはない。
//! - participation: 投票者 → 投票済みの投票用紙と、受理した票の数（`seq`）。ballot_id・候補者・時刻・到着順を持たない。
//! - slot_state: 再投票の slot → 最後の `seq` と票のハッシュ（再投票を認める選挙だけ。キーは slot）。
//! - ballot_pool: シャードごとの未封印の票（到着順）。投票者を特定する情報を持たない。
//!
//! 封印済みチェーンも同じロックの下で持つので、「ブロック追加 + プールからの削除」を不可分に行える。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroU16;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use application::{
    AuditEvent, CastError, ChainRead, ContestCounts, ElectionAuditEntry, ElectionStateSnapshot,
    ElectionStateStore, LeaseStore, SealStore, SlotState, StoreError, VoteStore, VotedContest,
};
use async_trait::async_trait;
use domain::{
    Anchor, Ballot, BallotId, Block, ContestId, ElectionPhase, ElectionRules, Period, ShardId,
    Slot, VoterId, ballot_hash,
};

#[derive(Default)]
struct Inner {
    /// 投票者 → (投票用紙 → 受理した票の数 `seq`)。ballot_id・候補者・時刻・到着順・slot を持たない。
    participation: HashMap<VoterId, HashMap<ContestId, u32>>,
    /// 再投票の状態（slot → 最後の seq と票のハッシュ）。キーは voter_id ではなく slot（ADR 0022）。
    slot_states: HashMap<Slot, SlotState>,
    /// 添字がシャード番号。未封印の票（到着順）。
    pools: Vec<VecDeque<Ballot>>,
    /// 添字がシャード番号。封印済みブロック（添字が高さ）。
    chains: Vec<Vec<Block>>,
    /// アンカー（`anchors[i].seq == i + 1`）。
    anchors: Vec<Anchor>,
    /// 登録済みの署名の公開鍵。
    signer: Option<[u8; 32]>,
    /// リース: 名前 → (owner, 期限)。
    leases: HashMap<String, (String, Instant)>,
    /// 選挙状態（原則17）。`None` は `ensure_initialized` 前（memory モードでは、プロセス開始直後）。
    election_state: Option<ElectionState>,
    /// 状態遷移の監査ログ（新しい順）。
    election_audit: Vec<ElectionAuditEntry>,
}

#[derive(Debug, Clone, Copy)]
struct ElectionState {
    phase: ElectionPhase,
    period: Period,
    opened_at: Option<i64>,
    closing_started_at: Option<i64>,
    /// open に遷移した時点で固定した選挙のルール（原則19）。
    rules: Option<ElectionRules>,
}

pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl InMemoryStore {
    pub fn new(shard_count: NonZeroU16) -> Self {
        let n = usize::from(shard_count.get());
        Self {
            inner: Mutex::new(Inner {
                participation: HashMap::new(),
                pools: (0..n).map(|_| VecDeque::new()).collect(),
                chains: (0..n).map(|_| Vec::new()).collect(),
                ..Inner::default()
            }),
        }
    }

    /// 内部状態を排他的に借りる。
    ///
    /// 別スレッドの panic でロックが汚染されていても続行する。各操作は panic し得る処理を
    /// 挟まずに更新するため、汚染されても中途半端な状態は残らない。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// シャード内の未封印の票（到着順）。テスト用の読み出しで、API からは公開しない。
    pub fn ballots_in_shard(&self, shard: ShardId) -> Vec<Ballot> {
        self.lock()
            .pools
            .get(usize::from(shard.0))
            .map(|pool| pool.iter().cloned().collect())
            .unwrap_or_default()
    }
}

// 誤ってログに出しても票や投票者が漏れないよう、Debug は件数だけを出す。
impl fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("InMemoryStore")
            .field("participants", &inner.participation.len())
            .field(
                "pending",
                &inner.pools.iter().map(VecDeque::len).sum::<usize>(),
            )
            .field("blocks", &inner.chains.iter().map(Vec::len).sum::<usize>())
            .finish()
    }
}

#[async_trait]
impl VoteStore for InMemoryStore {
    async fn cast(&self, voter: &VoterId, shard: ShardId, ballot: Ballot) -> Result<(), CastError> {
        // ロックは await をまたがずに保持する。二重投票の判定と追加は、このロックの中で不可分に行われる。
        let mut inner = self.lock();
        let Inner {
            participation,
            slot_states,
            pools,
            ..
        } = &mut *inner;

        let pool = pools
            .get_mut(usize::from(shard.0))
            .ok_or(StoreError::InvalidShard)?;
        if participation
            .get(voter)
            .is_some_and(|voted| voted.contains_key(&ballot.contest_id))
        {
            return Err(CastError::AlreadyVoted);
        }
        participation
            .entry(voter.clone())
            .or_default()
            .insert(ballot.contest_id.clone(), 1);
        if let Some(link) = ballot.revote {
            slot_states.insert(
                link.slot,
                SlotState {
                    seq: link.seq,
                    last_ballot_hash: ballot_hash(&ballot),
                },
            );
        }
        pool.push_back(ballot);
        Ok(())
    }

    async fn revote(
        &self,
        voter: &VoterId,
        shard: ShardId,
        ballot: Ballot,
        prev_seq: u32,
    ) -> Result<(), CastError> {
        let link = ballot.revote.ok_or(StoreError::Conflict)?;
        let mut inner = self.lock();
        let Inner {
            participation,
            slot_states,
            pools,
            ..
        } = &mut *inner;
        let pool = pools
            .get_mut(usize::from(shard.0))
            .ok_or(StoreError::InvalidShard)?;
        // 条件付き書き込み（seq = prev_seq のときだけ prev_seq + 1 にする）。ロックの中なので、同時に来ても 1 件だけ。
        let Some(seq) = participation
            .get_mut(voter)
            .and_then(|voted| voted.get_mut(&ballot.contest_id))
            .filter(|seq| **seq == prev_seq)
        else {
            return Err(CastError::RevoteConflict);
        };
        *seq = link.seq;
        slot_states.insert(
            link.slot,
            SlotState {
                seq: link.seq,
                last_ballot_hash: ballot_hash(&ballot),
            },
        );
        pool.push_back(ballot);
        Ok(())
    }

    async fn voted_contests(&self, voter: &VoterId) -> Result<Vec<VotedContest>, StoreError> {
        let inner = self.lock();
        Ok(inner
            .participation
            .get(voter)
            .map(|voted| {
                voted
                    .iter()
                    .map(|(contest, seq)| VotedContest {
                        contest: contest.clone(),
                        ballots: *seq,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn slot_state(&self, slot: &Slot) -> Result<Option<SlotState>, StoreError> {
        Ok(self.lock().slot_states.get(slot).copied())
    }

    async fn pending_by_shard(&self) -> Result<Vec<usize>, StoreError> {
        Ok(self.lock().pools.iter().map(VecDeque::len).collect())
    }

    async fn audit_counts(&self) -> Result<Vec<ContestCounts>, StoreError> {
        let inner = self.lock();
        // participation と票は、それぞれ投票用紙別に数えるだけで、突き合わせない。
        let mut participation: BTreeMap<ContestId, (u64, u64)> = BTreeMap::new();
        for voted in inner.participation.values() {
            for (contest, seq) in voted {
                let entry = participation.entry(contest.clone()).or_default();
                entry.0 += 1;
                entry.1 += u64::from(*seq);
            }
        }
        let mut pending: BTreeMap<ContestId, (u64, u64)> = BTreeMap::new();
        for pool in &inner.pools {
            for ballot in pool {
                let entry = pending.entry(ballot.contest_id.clone()).or_default();
                entry.0 += 1;
                if ballot.revote.is_none_or(|link| link.seq <= 1) {
                    entry.1 += 1;
                }
            }
        }
        let contests: BTreeSet<ContestId> = participation
            .keys()
            .chain(pending.keys())
            .cloned()
            .collect();
        Ok(contests
            .into_iter()
            .map(|contest| {
                let (participation, cast) = participation.get(&contest).copied().unwrap_or((0, 0));
                let (pending, pending_initial) = pending.get(&contest).copied().unwrap_or((0, 0));
                ContestCounts {
                    participation,
                    cast,
                    pending,
                    pending_initial,
                    contest,
                }
            })
            .collect())
    }
}

#[async_trait]
impl ChainRead for InMemoryStore {
    async fn head(&self, shard: ShardId) -> Result<Option<Block>, StoreError> {
        Ok(self
            .lock()
            .chains
            .get(usize::from(shard.0))
            .and_then(|chain| chain.last())
            .cloned())
    }

    async fn block(&self, shard: ShardId, height: u64) -> Result<Option<Block>, StoreError> {
        let Ok(index) = usize::try_from(height) else {
            return Ok(None);
        };
        Ok(self
            .lock()
            .chains
            .get(usize::from(shard.0))
            .and_then(|chain| chain.get(index))
            .cloned())
    }

    async fn latest_anchor(&self) -> Result<Option<Anchor>, StoreError> {
        Ok(self.lock().anchors.last().cloned())
    }

    async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError> {
        Ok(self.lock().signer)
    }

    async fn blocks_before(
        &self,
        shard: ShardId,
        before_height: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Block>, StoreError> {
        let inner = self.lock();
        let Some(chain) = inner.chains.get(usize::from(shard.0)) else {
            return Ok(Vec::new());
        };
        // 高さ = 添字。`before_height` が先頭より先なら、先頭までに切り詰める。
        let end = before_height.map_or(chain.len(), |b| usize::try_from(b).unwrap_or(usize::MAX));
        Ok(chain[..end.min(chain.len())]
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }

    async fn latest_anchors(&self, limit: usize) -> Result<Vec<Anchor>, StoreError> {
        Ok(self
            .lock()
            .anchors
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl SealStore for InMemoryStore {
    async fn pending_len(&self, shard: ShardId) -> Result<usize, StoreError> {
        self.lock()
            .pools
            .get(usize::from(shard.0))
            .map(VecDeque::len)
            .ok_or(StoreError::InvalidShard)
    }

    async fn peek_pending(&self, shard: ShardId, n: usize) -> Result<Vec<Ballot>, StoreError> {
        self.lock()
            .pools
            .get(usize::from(shard.0))
            .map(|pool| pool.iter().take(n).cloned().collect())
            .ok_or(StoreError::InvalidShard)
    }

    async fn commit(
        &self,
        shard: ShardId,
        block: Block,
        consumed: usize,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let Inner { pools, chains, .. } = &mut *inner;
        let index = usize::from(shard.0);
        let pool = pools.get_mut(index).ok_or(StoreError::InvalidShard)?;
        let chain = chains.get_mut(index).ok_or(StoreError::InvalidShard)?;

        // 以降はすべて検証のみ。失敗しても何も変更しない。
        let continues = match chain.last() {
            None => block.header.height == 0,
            Some(head) => {
                head.header.height.checked_add(1) == Some(block.header.height)
                    && block.header.prev_hash == head.block_hash
            }
        };
        // ブロックの票は、プール先頭の `consumed` 件（順不同）と一致していなければならない。
        let head_ids: HashSet<BallotId> = pool.iter().take(consumed).map(|b| b.ballot_id).collect();
        let matches_pool = consumed <= pool.len()
            && block.ballots.len() == consumed
            && head_ids.len() == consumed
            && block
                .ballots
                .iter()
                .all(|b| head_ids.contains(&b.ballot_id));
        if !continues || !matches_pool {
            return Err(StoreError::Conflict);
        }

        pool.drain(..consumed);
        chain.push(block);
        Ok(())
    }

    async fn register_signer(&self, public_key: [u8; 32]) -> Result<(), StoreError> {
        let mut inner = self.lock();
        match inner.signer {
            None => {
                inner.signer = Some(public_key);
                Ok(())
            }
            Some(existing) if existing == public_key => Ok(()),
            Some(_) => Err(StoreError::Conflict),
        }
    }

    async fn append_anchor(&self, anchor: &Anchor) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        // 次の番号（既存の件数 + 1）だけを受け付ける。競り負けなら false。
        let next = u64::try_from(inner.anchors.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(StoreError::Corrupt)?;
        if anchor.seq != next {
            return Ok(false);
        }
        inner.anchors.push(anchor.clone());
        Ok(true)
    }
}

/// リース（このプロセス内だけで有効）。期限は `Instant` で管理する。
#[async_trait]
impl LeaseStore for InMemoryStore {
    async fn try_acquire(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        match inner.leases.get(name) {
            Some((_, expires)) if *expires > now => Ok(false),
            _ => {
                inner
                    .leases
                    .insert(name.to_string(), (owner.to_string(), now + ttl));
                Ok(true)
            }
        }
    }

    async fn renew(&self, name: &str, owner: &str, ttl: Duration) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        match inner.leases.get_mut(name) {
            Some((current, expires)) if current == owner && *expires > now => {
                *expires = now + ttl;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if inner
            .leases
            .get(name)
            .is_some_and(|(current, _)| current == owner)
        {
            inner.leases.remove(name);
        }
        Ok(())
    }
}

#[async_trait]
impl ElectionStateStore for InMemoryStore {
    async fn ensure_initialized(
        &self,
        period: Period,
    ) -> Result<ElectionStateSnapshot, StoreError> {
        let mut inner = self.lock();
        let state = inner.election_state.get_or_insert(ElectionState {
            phase: ElectionPhase::Scheduled,
            period,
            opened_at: None,
            closing_started_at: None,
            rules: None,
        });
        Ok(snapshot(*state))
    }

    async fn get(&self) -> Result<ElectionStateSnapshot, StoreError> {
        let inner = self.lock();
        match inner.election_state {
            Some(state) => Ok(snapshot(state)),
            None => Err(StoreError::Unavailable),
        }
    }

    async fn schedule(&self, period: Period, _at_unix_secs: i64) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        match inner.election_state.as_mut() {
            Some(state) if state.phase == ElectionPhase::Scheduled => {
                state.period = period;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => Err(StoreError::Unavailable),
        }
    }

    async fn transition(
        &self,
        from: ElectionPhase,
        to: ElectionPhase,
        rules: ElectionRules,
        actor: &str,
        at_unix_secs: i64,
    ) -> Result<bool, StoreError> {
        if !from.can_advance_to(to) {
            // 原則17: 1 段の順序どおりの遷移だけを許す（呼び出し側の不具合を、ここで止める）。
            return Ok(false);
        }
        let mut inner = self.lock();
        let Some(state) = inner.election_state.as_mut() else {
            return Err(StoreError::Unavailable);
        };
        if state.phase != from {
            return Ok(false);
        }
        state.phase = to;
        if to == ElectionPhase::Open {
            state.opened_at = Some(at_unix_secs);
            // 原則19: 選挙のルールは open の時点で固定する（以降の遷移では変えない）。
            state.rules = Some(rules);
        }
        if to == ElectionPhase::Closing {
            state.closing_started_at = Some(at_unix_secs);
        }
        inner.election_audit.insert(
            0,
            ElectionAuditEntry {
                at_unix_secs,
                from,
                to,
                actor: actor.to_string(),
                event: AuditEvent::Transition,
            },
        );
        Ok(true)
    }

    async fn record_event(
        &self,
        event: AuditEvent,
        phase: ElectionPhase,
        actor: &str,
        at_unix_secs: i64,
    ) -> Result<(), StoreError> {
        self.lock().election_audit.insert(
            0,
            ElectionAuditEntry {
                at_unix_secs,
                from: phase,
                to: phase,
                actor: actor.to_string(),
                event,
            },
        );
        Ok(())
    }

    async fn recent_audit(&self, limit: usize) -> Result<Vec<ElectionAuditEntry>, StoreError> {
        let inner = self.lock();
        Ok(inner.election_audit.iter().take(limit).cloned().collect())
    }
}

fn snapshot(state: ElectionState) -> ElectionStateSnapshot {
    ElectionStateSnapshot {
        phase: state.phase,
        period: state.period,
        opened_at: state.opened_at,
        closing_started_at: state.closing_started_at,
        rules: state.rules,
    }
}

/// 改ざんデモ用（feature "dev-tools"）。メモリ上の封印済みの票を 1 件書き換える。
#[cfg(feature = "dev-tools")]
mod dev {
    use domain::{CandidateId, DistrictId};

    use super::*;

    /// 書き換えた場所。票の中身は含めない。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TamperedAt {
        pub shard: ShardId,
        pub height: u64,
        pub index: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    pub enum TamperError {
        #[error("書き換え対象のブロックまたは票がありません")]
        NotFound,
    }

    impl InMemoryStore {
        /// 封印済みブロックの票 1 件の `candidate_id` を書き換える（ヘッダやハッシュは触らない）。
        ///
        /// 省略時の既定: シャード 0、票を持つ最新のブロック、その 0 番目。
        pub fn tamper_ballot(
            &self,
            shard: Option<ShardId>,
            height: Option<u64>,
            index: Option<usize>,
        ) -> Result<TamperedAt, TamperError> {
            let shard = shard.unwrap_or(ShardId(0));
            let mut inner = self.lock();
            let chain = inner
                .chains
                .get_mut(usize::from(shard.0))
                .ok_or(TamperError::NotFound)?;
            let height_index = match height {
                Some(h) => usize::try_from(h).map_err(|_| TamperError::NotFound)?,
                None => chain
                    .iter()
                    .rposition(|b| !b.ballots.is_empty())
                    .ok_or(TamperError::NotFound)?,
            };
            let index = index.unwrap_or(0);
            let ballot = chain
                .get_mut(height_index)
                .and_then(|b| b.ballots.get_mut(index))
                .ok_or(TamperError::NotFound)?;
            // 候補者の票は連番を 1 つ進め、白票はその選挙区の 1 番目の候補者にする。どちらも、必ず別の
            // （形式は正しい）投票先になる。
            let district = DistrictId::new(ballot.contest_id.district_part())
                .map_err(|_| TamperError::NotFound)?;
            let seq = match &ballot.candidate_id {
                CandidateId::Blank => 1,
                CandidateId::Candidate(code) => code.sequence() + 1,
            };
            ballot.candidate_id =
                CandidateId::new(&district, seq).map_err(|_| TamperError::NotFound)?;
            Ok(TamperedAt {
                shard,
                height: height_index as u64,
                index,
            })
        }
    }
}

#[cfg(feature = "dev-tools")]
pub use dev::{TamperError, TamperedAt};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use domain::{CandidateId, Ed25519Signer, genesis, seal_block};

    use super::*;

    fn store(shards: u16) -> InMemoryStore {
        InMemoryStore::new(NonZeroU16::new(shards).expect("non-zero"))
    }

    fn voter(name: &str) -> VoterId {
        VoterId::new(name).expect("valid")
    }

    /// 番号 `n` の投票用紙（東京の小選挙区 `n` 区）。
    fn contest(n: u32) -> ContestId {
        ContestId::parse(&format!("2026-general/shugiin_smd.13.{n:02}")).expect("valid")
    }

    fn candidate(contest: u32, seq: u32) -> CandidateId {
        CandidateId::parse(&format!("shugiin_smd.13.{contest:02}.c{seq}")).expect("valid")
    }

    fn ballot(n: u8, contest_no: u32, candidate_seq: u32) -> Ballot {
        Ballot {
            ballot_id: BallotId::from_random_bytes([n; 16]),
            contest_id: contest(contest_no),
            candidate_id: candidate(contest_no, candidate_seq),
            revote: None,
        }
    }

    fn signer() -> Ed25519Signer {
        Ed25519Signer::from_seed(&[5u8; 32])
    }

    // --- VoteStore ---

    #[tokio::test]
    async fn first_vote_is_stored_and_second_is_rejected() {
        let s = store(2);
        let alice = voter("alice");
        assert_eq!(s.cast(&alice, ShardId(1), ballot(1, 1, 101)).await, Ok(()));
        assert_eq!(
            s.cast(&alice, ShardId(0), ballot(2, 1, 102)).await,
            Err(CastError::AlreadyVoted)
        );
        // 拒否された票はプールに入らない。
        assert_eq!(s.pending_by_shard().await, Ok(vec![0, 1]));
        assert_eq!(contests_of(&s, &alice).await, vec![contest(1)]);
    }

    /// 投票者が投票済みの投票用紙（昇順）。
    async fn contests_of(s: &InMemoryStore, v: &VoterId) -> Vec<ContestId> {
        let mut voted: Vec<ContestId> = s
            .voted_contests(v)
            .await
            .expect("voted")
            .into_iter()
            .map(|c| c.contest)
            .collect();
        voted.sort();
        voted
    }

    /// slot `s` の `seq` 番目の票（`prev` の次の版）。
    fn linked(n: u8, candidate_seq: u32, s: u8, seq: u32, prev: Option<&Ballot>) -> Ballot {
        Ballot {
            revote: Some(domain::RevoteLink {
                slot: Slot([s; 32]),
                seq,
                supersedes: prev.map(ballot_hash),
            }),
            ..ballot(n, 1, candidate_seq)
        }
    }

    #[tokio::test]
    async fn revotes_bump_the_seq_only_from_the_expected_value_and_track_the_slot() {
        let s = store(1);
        let alice = voter("alice");
        let first = linked(1, 101, 7, 1, None);
        s.cast(&alice, ShardId(0), first.clone())
            .await
            .expect("cast");
        assert_eq!(
            s.slot_state(&Slot([7; 32])).await,
            Ok(Some(SlotState {
                seq: 1,
                last_ballot_hash: ballot_hash(&first)
            }))
        );
        let second = linked(2, 102, 7, 2, Some(&first));
        s.revote(&alice, ShardId(0), second.clone(), 1)
            .await
            .expect("revote");
        // 同じ前提（seq=1）の、もう 1 つの再投票は負ける（何も保存しない）。
        let racing = linked(3, 103, 7, 2, Some(&first));
        assert_eq!(
            s.revote(&alice, ShardId(0), racing, 1).await,
            Err(CastError::RevoteConflict)
        );
        // 投票していない有権者の再投票も、条件を満たさない。
        assert_eq!(
            s.revote(&voter("bob"), ShardId(0), linked(4, 101, 8, 2, None), 1)
                .await,
            Err(CastError::RevoteConflict)
        );
        let voted = s.voted_contests(&alice).await.expect("voted");
        assert_eq!(
            voted,
            vec![VotedContest {
                contest: contest(1),
                ballots: 2
            }]
        );
        assert_eq!(
            s.slot_state(&Slot([7; 32])).await,
            Ok(Some(SlotState {
                seq: 2,
                last_ballot_hash: ballot_hash(&second)
            }))
        );
        // participation は 1 人、受理した票は 2、未封印は 2（そのうち最初の票は 1）。
        let counts = s.audit_counts().await.expect("counts");
        assert_eq!(
            (
                counts[0].participation,
                counts[0].cast,
                counts[0].pending,
                counts[0].pending_initial
            ),
            (1, 2, 2, 1)
        );
        // 未封印の票は到着順（前の版が先）。
        let pool = s.ballots_in_shard(ShardId(0));
        assert_eq!(pool, vec![first, second]);
    }

    #[tokio::test]
    async fn same_voter_can_vote_in_other_contests_and_other_voters_in_same_contest() {
        let s = store(1);
        let (alice, bob) = (voter("alice"), voter("bob"));
        for (v, n, contest) in [(&alice, 1, 1), (&alice, 2, 2), (&bob, 3, 1)] {
            assert_eq!(
                s.cast(v, ShardId(0), ballot(n, contest, 100 + u32::from(n)))
                    .await,
                Ok(())
            );
        }
        assert_eq!(contests_of(&s, &alice).await, vec![contest(1), contest(2)]);
        assert_eq!(s.voted_contests(&voter("carol")).await, Ok(vec![]));
        assert_eq!(s.pending_by_shard().await, Ok(vec![3]));
    }

    #[tokio::test]
    async fn rejects_unknown_shard_without_recording_participation() {
        let s = store(2);
        let alice = voter("alice");
        assert_eq!(
            s.cast(&alice, ShardId(2), ballot(1, 1, 101)).await,
            Err(CastError::Store(StoreError::InvalidShard))
        );
        // 失敗した投票で「投票済み」にしてしまわない。
        assert_eq!(s.voted_contests(&alice).await, Ok(vec![]));
        assert_eq!(s.pending_by_shard().await, Ok(vec![0, 0]));
    }

    #[tokio::test]
    async fn pool_keeps_arrival_order_within_shard() {
        let s = store(1);
        for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
            let n = i as u8 + 1;
            s.cast(&voter(name), ShardId(0), ballot(n, 1, 100 + u32::from(n)))
                .await
                .expect("cast");
        }
        let got: Vec<u32> = s
            .ballots_in_shard(ShardId(0))
            .iter()
            .map(|b| b.candidate_id.candidate().expect("candidate").sequence() as u32)
            .collect();
        assert_eq!(got, vec![101, 102, 103]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_votes_by_same_voter_succeed_exactly_once() {
        let s = Arc::new(store(4));
        let alice = voter("alice");
        let mut tasks = Vec::new();
        for i in 0..100u8 {
            let (s, alice) = (s.clone(), alice.clone());
            tasks.push(tokio::spawn(async move {
                s.cast(&alice, ShardId(u16::from(i % 4)), ballot(i, 1, 101))
                    .await
            }));
        }
        let mut ok = 0;
        let mut rejected = 0;
        for t in tasks {
            match t.await.expect("task") {
                Ok(()) => ok += 1,
                Err(CastError::AlreadyVoted) => rejected += 1,
                Err(e) => panic!("unexpected: {e}"),
            }
        }
        assert_eq!((ok, rejected), (1, 99));
        assert_eq!(
            s.pending_by_shard()
                .await
                .expect("counts")
                .iter()
                .sum::<usize>(),
            1
        );
    }

    #[tokio::test]
    async fn debug_output_contains_only_counts() {
        let s = store(2);
        s.cast(
            &voter("very-secret-voter"),
            ShardId(0),
            ballot(9, 1, 424242),
        )
        .await
        .expect("cast");
        let shown = format!("{s:?}");
        assert!(!shown.contains("very-secret-voter"));
        assert!(!shown.contains("424242"));
        assert!(shown.contains("participants: 1"));
        assert!(shown.contains("pending: 1"));
    }

    // --- ChainRead / SealStore ---

    async fn cast_n(s: &InMemoryStore, shard: ShardId, from: u8, n: u8) {
        for i in from..from + n {
            s.cast(
                &voter(&format!("v{i}")),
                shard,
                ballot(i, 1, 100 + u32::from(i)),
            )
            .await
            .expect("cast");
        }
    }

    #[tokio::test]
    async fn uninitialized_chain_has_no_head_or_blocks() {
        let s = store(2);
        assert_eq!(s.head(ShardId(0)).await, Ok(None));
        assert_eq!(s.block(ShardId(0), 0).await, Ok(None));
        assert_eq!(s.head(ShardId(9)).await, Ok(None));
        assert_eq!(s.block(ShardId(0), u64::MAX).await, Ok(None));
    }

    #[tokio::test]
    async fn commit_appends_block_and_removes_consumed_ballots_from_pool() {
        let (s, signer) = (store(2), signer());
        s.commit(ShardId(0), genesis(&signer, 1), 0)
            .await
            .expect("genesis");
        cast_n(&s, ShardId(0), 1, 5).await;
        cast_n(&s, ShardId(1), 100, 2).await;

        let head = s.head(ShardId(0)).await.expect("head").expect("some");
        let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");
        assert_eq!(batch.len(), 3);
        // peek は削除しない。
        assert_eq!(s.pending_len(ShardId(0)).await, Ok(5));

        let block = seal_block(&head, batch.clone(), 2, &signer).expect("seal");
        s.commit(ShardId(0), block.clone(), 3)
            .await
            .expect("commit");

        // 先頭 3 件だけが消え、残りの 2 件は到着順のまま。他シャードは無傷。
        assert_eq!(s.pending_len(ShardId(0)).await, Ok(2));
        assert_eq!(s.pending_len(ShardId(1)).await, Ok(2));
        let rest: Vec<u32> = s
            .ballots_in_shard(ShardId(0))
            .iter()
            .map(|b| b.candidate_id.candidate().expect("candidate").sequence() as u32)
            .collect();
        assert_eq!(rest, vec![104, 105]);
        assert_eq!(s.head(ShardId(0)).await, Ok(Some(block.clone())));
        assert_eq!(s.block(ShardId(0), 1).await, Ok(Some(block)));
        // 各シャードのチェーンは独立。
        assert_eq!(s.head(ShardId(1)).await, Ok(None));
    }

    #[tokio::test]
    async fn commit_rejects_broken_continuity_or_mismatched_ballots_without_changes() {
        let (s, signer) = (store(1), signer());
        let g = genesis(&signer, 1);
        // 空のチェーンにジェネシス以外は置けない。
        cast_n(&s, ShardId(0), 1, 3).await;
        let orphan = seal_block(
            &g,
            s.peek_pending(ShardId(0), 3).await.expect("peek"),
            2,
            &signer,
        )
        .expect("seal");
        assert_eq!(
            s.commit(ShardId(0), orphan, 3).await,
            Err(StoreError::Conflict)
        );

        s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
        // 二重のジェネシス（高さが連続しない）。
        assert_eq!(
            s.commit(ShardId(0), g.clone(), 0).await,
            Err(StoreError::Conflict)
        );

        let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");
        // prev_hash が合わない。
        let mut bad_prev = seal_block(&g, batch.clone(), 2, &signer).expect("seal");
        bad_prev.header.prev_hash[0] ^= 1;
        assert_eq!(
            s.commit(ShardId(0), bad_prev, 3).await,
            Err(StoreError::Conflict)
        );
        // 消費件数がブロックの票数と合わない / プールを超える。
        let good = seal_block(&g, batch, 2, &signer).expect("seal");
        assert_eq!(
            s.commit(ShardId(0), good.clone(), 2).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(
            s.commit(ShardId(0), good.clone(), 4).await,
            Err(StoreError::Conflict)
        );
        // プールにない票を含むブロック。
        let foreign = seal_block(&g, vec![ballot(200, 1, 1)], 2, &signer).expect("seal");
        assert_eq!(
            s.commit(ShardId(0), foreign, 1).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(
            s.commit(ShardId(7), g.clone(), 0).await,
            Err(StoreError::InvalidShard)
        );

        // どれも状態を変えていない。
        assert_eq!(s.pending_len(ShardId(0)).await, Ok(3));
        assert_eq!(s.head(ShardId(0)).await, Ok(Some(g)));
        // 正しいものは受理される。
        s.commit(ShardId(0), good, 3).await.expect("commit");
        assert_eq!(s.pending_len(ShardId(0)).await, Ok(0));
    }

    #[tokio::test]
    async fn seal_store_rejects_unknown_shard() {
        let s = store(1);
        assert_eq!(
            s.pending_len(ShardId(1)).await,
            Err(StoreError::InvalidShard)
        );
        assert_eq!(
            s.peek_pending(ShardId(1), 1).await,
            Err(StoreError::InvalidShard)
        );
    }

    // --- 監査用の集計・アンカー・署名鍵・リース ---

    #[tokio::test]
    async fn audit_counts_are_per_contest_and_count_pending_separately() {
        let s = store(2);
        assert_eq!(s.audit_counts().await, Ok(vec![]));
        for (name, shard, contest, n) in [("a", 0u16, 1u32, 1u8), ("b", 1, 1, 2), ("c", 0, 2, 3)] {
            s.cast(
                &voter(name),
                ShardId(shard),
                ballot(n, contest, 100 + u32::from(n)),
            )
            .await
            .expect("cast");
        }
        // alice は 2 つの投票用紙に投票する（participation は (投票者, 投票用紙) の数）。
        s.cast(&voter("a"), ShardId(0), ballot(9, 2, 200))
            .await
            .expect("cast");
        let counts = s.audit_counts().await.expect("counts");
        let rows: Vec<(ContestId, u64, u64)> = counts
            .iter()
            .map(|c| (c.contest.clone(), c.participation, c.pending))
            .collect();
        assert_eq!(rows, vec![(contest(1), 2, 2), (contest(2), 2, 2)]);

        // 封印してプールから消えると、pending は減り、participation は変わらない。
        let signer = signer();
        let g = genesis(&signer, 1);
        s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
        let batch = s.peek_pending(ShardId(0), 3).await.expect("peek");
        let sealed = seal_block(&g, batch, 2, &signer).expect("seal");
        s.commit(ShardId(0), sealed, 3).await.expect("commit");
        let rows: Vec<(ContestId, u64, u64)> = s
            .audit_counts()
            .await
            .expect("counts")
            .iter()
            .map(|c| (c.contest.clone(), c.participation, c.pending))
            .collect();
        assert_eq!(rows, vec![(contest(1), 2, 1), (contest(2), 2, 0)]);
    }

    fn anchor(seq: u64) -> Anchor {
        domain::build_anchor(
            seq,
            10,
            domain::anchor::GENESIS_ANCHOR_PREV,
            vec![domain::ShardHead {
                shard: 0,
                height: seq,
                block_hash: [seq as u8; 32],
            }],
            &signer(),
        )
        .expect("anchor")
    }

    #[tokio::test]
    async fn anchors_are_appended_in_sequence_and_losers_get_false() {
        let s = store(1);
        assert_eq!(s.latest_anchor().await, Ok(None));
        assert_eq!(
            s.append_anchor(&anchor(2)).await,
            Ok(false),
            "seq 1 より先には書けない"
        );
        assert_eq!(s.append_anchor(&anchor(1)).await, Ok(true));
        assert_eq!(
            s.append_anchor(&anchor(1)).await,
            Ok(false),
            "同じ seq は競り負け"
        );
        assert_eq!(s.append_anchor(&anchor(2)).await, Ok(true));
        assert_eq!(s.latest_anchor().await, Ok(Some(anchor(2))));
    }

    #[tokio::test]
    async fn signer_key_can_be_registered_once() {
        let s = store(1);
        assert_eq!(s.signer_public_key().await, Ok(None));
        s.register_signer([1; 32]).await.expect("first");
        s.register_signer([1; 32]).await.expect("same key again");
        assert_eq!(s.register_signer([2; 32]).await, Err(StoreError::Conflict));
        assert_eq!(s.signer_public_key().await, Ok(Some([1; 32])));
    }

    #[tokio::test]
    async fn leases_are_exclusive_renewable_and_expire() {
        let s = store(1);
        let ttl = Duration::from_millis(80);
        assert_eq!(s.try_acquire("shard-0", "a", ttl).await, Ok(true));
        assert_eq!(s.try_acquire("shard-0", "b", ttl).await, Ok(false));
        assert_eq!(
            s.renew("shard-0", "b", ttl).await,
            Ok(false),
            "他の owner は更新できない"
        );
        assert_eq!(s.renew("shard-0", "a", ttl).await, Ok(true));
        s.release("shard-0", "b").await.expect("no-op");
        assert_eq!(
            s.try_acquire("shard-0", "b", ttl).await,
            Ok(false),
            "他人の解放は効かない"
        );

        // 期限切れ: 失った側は更新できず、別の owner が取得できる。
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(s.renew("shard-0", "a", ttl).await, Ok(false));
        assert_eq!(s.try_acquire("shard-0", "b", ttl).await, Ok(true));

        // 解放すると、すぐ他の owner が取得できる。別の名前のリースとは独立。
        assert_eq!(s.try_acquire("anchor", "a", ttl).await, Ok(true));
        s.release("shard-0", "b").await.expect("release");
        assert_eq!(s.try_acquire("shard-0", "a", ttl).await, Ok(true));
    }

    #[tokio::test]
    async fn blocks_are_paged_newest_first_and_clamped_to_the_chain() {
        let (s, signer) = (store(2), signer());
        let mut chain = vec![genesis(&signer, 1)];
        s.commit(ShardId(0), chain[0].clone(), 0)
            .await
            .expect("genesis");
        for i in 0..4u8 {
            cast_n(&s, ShardId(0), i * 2 + 1, 1).await;
            let batch = s.peek_pending(ShardId(0), 1).await.expect("peek");
            let block = seal_block(
                chain.last().expect("head"),
                batch,
                2 + u64::from(i),
                &signer,
            )
            .expect("seal");
            s.commit(ShardId(0), block.clone(), 1)
                .await
                .expect("commit");
            chain.push(block);
        }
        let heights =
            |blocks: Vec<Block>| blocks.iter().map(|b| b.header.height).collect::<Vec<_>>();
        let page = |before, limit| {
            let s = &s;
            async move {
                heights(
                    s.blocks_before(ShardId(0), before, limit)
                        .await
                        .expect("page"),
                )
            }
        };
        assert_eq!(page(None, 3).await, vec![4, 3, 2]);
        assert_eq!(page(Some(2), 10).await, vec![1, 0]);
        assert_eq!(page(Some(0), 10).await, Vec::<u64>::new());
        assert_eq!(page(Some(99), 2).await, vec![4, 3]);
        assert_eq!(page(Some(u64::MAX), 1).await, vec![4]);
        assert_eq!(page(None, 0).await, Vec::<u64>::new());
        // チェーンが無い・存在しないシャードは空。
        assert_eq!(s.blocks_before(ShardId(1), None, 5).await, Ok(vec![]));
        assert_eq!(s.blocks_before(ShardId(9), None, 5).await, Ok(vec![]));
    }

    #[tokio::test]
    async fn anchors_are_listed_newest_first_up_to_the_limit() {
        let s = store(1);
        assert_eq!(s.latest_anchors(3).await, Ok(vec![]));
        let mut prev = [0u8; 32];
        let mut all = Vec::new();
        for seq in 1..=3u64 {
            let a = domain::build_anchor(
                seq,
                100 + seq,
                prev,
                vec![domain::ShardHead {
                    shard: 0,
                    height: seq,
                    block_hash: [seq as u8; 32],
                }],
                &signer(),
            )
            .expect("anchor");
            prev = a.anchor_hash;
            assert_eq!(s.append_anchor(&a).await, Ok(true));
            all.push(a);
        }
        let seqs = |anchors: Vec<Anchor>| anchors.iter().map(|a| a.seq).collect::<Vec<_>>();
        assert_eq!(
            seqs(s.latest_anchors(10).await.expect("all")),
            vec![3, 2, 1]
        );
        assert_eq!(seqs(s.latest_anchors(2).await.expect("two")), vec![3, 2]);
        assert_eq!(s.latest_anchors(0).await, Ok(vec![]));
    }

    #[tokio::test]
    async fn election_rules_are_fixed_when_the_election_opens() {
        let s = store(1);
        let on = ElectionRules {
            allow_blank: true,
            ..ElectionRules::default()
        };
        let off = ElectionRules {
            allow_blank: false,
            ..ElectionRules::default()
        };
        let initial = s.ensure_initialized(Period::default()).await.expect("init");
        assert_eq!(initial.rules, None, "scheduled の間は、まだ固定しない");
        assert!(
            s.transition(ElectionPhase::Scheduled, ElectionPhase::Open, off, "t", 10)
                .await
                .expect("open")
        );
        assert_eq!(s.get().await.expect("get").rules, Some(off));
        // 後の遷移に別のルールを渡しても、open の時点で固定した値のまま。
        for (from, to) in [
            (ElectionPhase::Open, ElectionPhase::Closing),
            (ElectionPhase::Closing, ElectionPhase::Closed),
        ] {
            assert!(s.transition(from, to, on, "t", 20).await.expect("advance"));
            assert_eq!(s.get().await.expect("get").rules, Some(off));
        }
        // 失敗した遷移（既に動いている）でも変わらない。
        assert!(
            !s.transition(ElectionPhase::Scheduled, ElectionPhase::Open, on, "t", 30)
                .await
                .expect("stale")
        );
        assert_eq!(s.get().await.expect("get").rules, Some(off));
    }

    #[cfg(feature = "dev-tools")]
    #[tokio::test]
    async fn tamper_changes_one_sealed_ballot_only() {
        let (s, signer) = (store(1), signer());
        let g = genesis(&signer, 1);
        s.commit(ShardId(0), g.clone(), 0).await.expect("genesis");
        cast_n(&s, ShardId(0), 1, 4).await;
        let batch = s.peek_pending(ShardId(0), 4).await.expect("peek");
        let block = seal_block(&g, batch, 2, &signer).expect("seal");
        s.commit(ShardId(0), block.clone(), 4)
            .await
            .expect("commit");

        // 既定: 票を持つ最新ブロックの 0 番目。
        let at = s.tamper_ballot(None, None, None).expect("tamper");
        assert_eq!((at.shard, at.height, at.index), (ShardId(0), 1, 0));
        let after = s.head(ShardId(0)).await.expect("head").expect("some");
        let changed: Vec<usize> = (0..4)
            .filter(|&i| after.ballots[i] != block.ballots[i])
            .collect();
        assert_eq!(changed, vec![0]);
        assert_eq!(after.header, block.header);
        assert_eq!(after.block_hash, block.block_hash);

        assert_eq!(
            s.tamper_ballot(Some(ShardId(5)), None, None),
            Err(TamperError::NotFound)
        );
        assert_eq!(
            s.tamper_ballot(None, Some(9), None),
            Err(TamperError::NotFound)
        );
        assert_eq!(
            s.tamper_ballot(None, Some(1), Some(99)),
            Err(TamperError::NotFound)
        );
    }
}
