//! `verifier verify`: API から封印済みチェーンを取得して検証する。
//!
//! 取得元は [`ChainSource`] トレイトで抽象化し、本番は HTTP（ureq）、テストはメモリ上のフェイクを使う。

use std::time::Duration;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, bail};
use domain::{
    Anchor, Ballot, BallotId, Block, BlockHeader, CandidateId, ChainError, ContestId,
    Ed25519Verifier, ShardHead, verify_anchor, verify_chain,
};
use serde::de::DeserializeOwned;
use shared_types::{AnchorDto, AuditCountsResponse, BlockDto, HeadDto, hex};

/// チェーンの取得元。`None` は「存在しない（404）」を表す。
pub trait ChainSource {
    fn head(&self, shard: u16) -> anyhow::Result<Option<HeadDto>>;
    fn block(&self, shard: u16, height: u64) -> anyhow::Result<Option<BlockDto>>;
    /// 最新のアンカー。まだ無ければ `None`。
    fn anchor(&self) -> anyhow::Result<Option<AnchorDto>>;
    /// 投票用紙ごとの participation 件数と未封印の票の件数。
    fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse>;
}

/// API サーバ（`/api/v1/chains/...`）から取得する。
pub struct HttpSource {
    agent: ureq::Agent,
    base: String,
}

impl HttpSource {
    pub fn new(base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// `GET /api/v1/election-status`（認証不要）。tally が、選挙状態（closed かどうか）を判定するのに使う
    /// （原則17。時刻ではなく、api が持つ状態を見る）。
    pub fn election_status(&self) -> anyhow::Result<shared_types::ElectionStatusResponse> {
        self.get_json("/api/v1/election-status")?
            .context("/api/v1/election-status が見つかりません（API が古い可能性があります）")
    }

    /// 404 は `None`、それ以外の失敗（通信エラー・5xx・不正な JSON）は `Err`。
    fn get_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = format!("{}{path}", self.base);
        match self.agent.get(&url).call() {
            Ok(mut response) => response
                .body_mut()
                .read_json::<T>()
                .with_context(|| format!("{url} の応答を解釈できません"))
                .map(Some),
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{url} の取得に失敗しました: {e}")),
        }
    }
}

/// API が、票の中身を返さなかった（`chain.reveal_ballots=after_close` の締切前）。ブロックのハッシュは票から再計算するので、
/// 検証も集計もできない。改ざんの証拠ではなく、「まだ検証できない」という状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "API が票の中身を返しません（chain.reveal_ballots=after_close の締切前）。ブロックのハッシュは票から再計算するので、締切後に実行してください"
)]
pub struct BallotsHidden;

impl ChainSource for HttpSource {
    fn head(&self, shard: u16) -> anyhow::Result<Option<HeadDto>> {
        self.get_json(&format!("/api/v1/chains/{shard}/head"))
    }

    fn block(&self, shard: u16, height: u64) -> anyhow::Result<Option<BlockDto>> {
        let block: Option<BlockDto> =
            self.get_json(&format!("/api/v1/chains/{shard}/blocks/{height}"))?;
        // 票が伏せられた応答（票 0 件に見える）を、そのまま検証すると、改ざんに見えてしまう。区別して返す。
        match block {
            Some(b) if !b.ballots_revealed => Err(BallotsHidden.into()),
            other => Ok(other),
        }
    }

    fn anchor(&self) -> anyhow::Result<Option<AnchorDto>> {
        self.get_json("/api/v1/anchors/latest")
    }

    fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse> {
        self.get_json("/api/v1/audit/counts")?
            .context("/api/v1/audit/counts が見つかりません（API が古い可能性があります）")
    }
}

/// 検証に成功したシャードの概要（突合・アンカーの検証に使う情報を含む）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardReport {
    pub shard: u16,
    pub blocks: usize,
    pub ballots: usize,
    /// 高さ順のブロックハッシュ（アンカーの head との照合用）。
    pub block_hashes: Vec<[u8; 32]>,
    /// 投票用紙ごとの、チェーン内の票数。
    pub contests: BTreeMap<ContestId, u64>,
    /// 投票用紙ごと・候補者ごとの、チェーン内の票数（`tally` の入力。検証したブロックの票だけ）。
    pub votes: BTreeMap<ContestId, BTreeMap<CandidateId, u64>>,
    /// チェーン内の全 `ballot_id`（シャードをまたいだ重複の検出用）。
    pub ballot_ids: Vec<[u8; 16]>,
    /// 署名の検証に使った公開鍵。
    pub public_key: [u8; 32],
}

/// 検証失敗の理由（チェーンが不正である証拠）。取得エラー等の実行時エラーとは区別する。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyFailure {
    #[error("{0}")]
    Chain(#[from] ChainError),
    #[error("head が示す高さ {0} のブロックが取得できません")]
    MissingBlock(u64),
    #[error("head の block_hash が最新ブロックと一致しません")]
    HeadMismatch,
    #[error("不正なデータです: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardVerdict {
    Valid(ShardReport),
    Invalid { shard: u16, failure: VerifyFailure },
}

/// シャード 0 から順に、head が 404 になるまでのすべてのチェーンを検証する。
///
/// `pinned_key` があればそれを、なければ各シャードの head が示す公開鍵を使う。
/// 通信エラーなどの実行時エラーは `Err`、チェーンの不整合は `ShardVerdict::Invalid`。
pub fn verify_all(
    source: &dyn ChainSource,
    pinned_key: Option<&[u8; 32]>,
) -> anyhow::Result<Vec<ShardVerdict>> {
    let mut verdicts = Vec::new();
    for shard in 0..=u16::MAX {
        let Some(head) = source.head(shard)? else {
            break;
        };
        verdicts.push(match verify_shard(source, shard, &head, pinned_key)? {
            Ok(report) => ShardVerdict::Valid(report),
            Err(failure) => ShardVerdict::Invalid { shard, failure },
        });
    }
    if verdicts.is_empty() {
        bail!("チェーンが見つかりません（シャード 0 の head が存在しません）");
    }
    Ok(verdicts)
}

/// 外側の `Result` は取得エラー、内側の `Result` は検証結果。
fn verify_shard(
    source: &dyn ChainSource,
    shard: u16,
    head: &HeadDto,
    pinned_key: Option<&[u8; 32]>,
) -> anyhow::Result<Result<ShardReport, VerifyFailure>> {
    let key = match pinned_key {
        Some(key) => *key,
        None => match hex::decode_array::<32>(&head.signer_public_key) {
            Ok(key) => key,
            Err(e) => return Ok(Err(VerifyFailure::Malformed(format!("公開鍵: {e}")))),
        },
    };
    let Ok(verifier) = Ed25519Verifier::from_public_key(&key) else {
        return Ok(Err(VerifyFailure::Malformed(
            "公開鍵が不正です".to_string(),
        )));
    };

    let mut blocks = Vec::new();
    for height in 0..=head.height {
        let Some(dto) = source.block(shard, height)? else {
            return Ok(Err(VerifyFailure::MissingBlock(height)));
        };
        match block_from_dto(&dto) {
            Ok(block) => blocks.push(block),
            Err(failure) => return Ok(Err(failure)),
        }
    }

    if let Err(e) = verify_chain(&blocks, &verifier) {
        return Ok(Err(e.into()));
    }
    // head の主張（最新ブロックのハッシュ）が、取得したチェーンと一致していること。
    if blocks.last().map(|b| hex::encode(&b.block_hash)).as_deref() != Some(&head.block_hash) {
        return Ok(Err(VerifyFailure::HeadMismatch));
    }
    let mut contests: BTreeMap<ContestId, u64> = BTreeMap::new();
    let mut votes: BTreeMap<ContestId, BTreeMap<CandidateId, u64>> = BTreeMap::new();
    for ballot in blocks.iter().flat_map(|b| &b.ballots) {
        *contests.entry(ballot.contest_id.clone()).or_default() += 1;
        *votes
            .entry(ballot.contest_id.clone())
            .or_default()
            .entry(ballot.candidate_id.clone())
            .or_default() += 1;
    }
    Ok(Ok(ShardReport {
        shard,
        blocks: blocks.len(),
        ballots: blocks.iter().map(|b| b.ballots.len()).sum(),
        block_hashes: blocks.iter().map(|b| b.block_hash).collect(),
        contests,
        votes,
        ballot_ids: blocks
            .iter()
            .flat_map(|b| b.ballots.iter().map(|x| x.ballot_id.0))
            .collect(),
        public_key: key,
    }))
}

/// API の DTO を検証用のドメイン型に戻す。長さ・形式が不正なら `Malformed`。
fn block_from_dto(dto: &BlockDto) -> Result<Block, VerifyFailure> {
    let bad = |what: &str, e: hex::HexError| VerifyFailure::Malformed(format!("{what}: {e}"));
    let h = &dto.header;
    let mut ballots = Vec::with_capacity(dto.ballots.len());
    for b in &dto.ballots {
        ballots.push(Ballot {
            ballot_id: BallotId(
                hex::decode_array::<16>(&b.ballot_id).map_err(|e| bad("ballot_id", e))?,
            ),
            contest_id: ContestId::parse(&b.contest_id)
                .map_err(|e| VerifyFailure::Malformed(format!("contest_id: {e}")))?,
            candidate_id: CandidateId::parse(&b.candidate_id)
                .map_err(|e| VerifyFailure::Malformed(format!("candidate_id: {e}")))?,
        });
    }
    Ok(Block {
        header: BlockHeader {
            version: h.version,
            height: h.height,
            prev_hash: hex::decode_array::<32>(&h.prev_hash).map_err(|e| bad("prev_hash", e))?,
            merkle_root: hex::decode_array::<32>(&h.merkle_root)
                .map_err(|e| bad("merkle_root", e))?,
            ballot_count: h.ballot_count,
            sealed_at_minute: h.sealed_at_minute,
        },
        ballots,
        block_hash: hex::decode_array::<32>(&dto.block_hash).map_err(|e| bad("block_hash", e))?,
        signature: hex::decode_array::<64>(&dto.signature).map_err(|e| bad("signature", e))?,
    })
}

// ---------------------------------------------------------------------------
// 突合（participation とチェーン内の票数）・重複・アンカー
// ---------------------------------------------------------------------------

/// 投票用紙ごとの突合結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestRow {
    pub contest_id: String,
    /// 投票済みの記録（participation）の件数。
    pub participation: u64,
    /// チェーン内（封印済み）の票数。
    pub sealed: u64,
    /// まだ封印されていない票の数。
    pub pending: u64,
}

impl ContestRow {
    /// 投票済みの数が、封印済み + 封印待ちの票の数と一致すること。
    /// 不一致は、票の消失（`participation` が多い）や二重封印・不正な票（票が多い）を示す。
    pub fn is_consistent(&self) -> bool {
        self.participation == self.sealed.saturating_add(self.pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorCheck {
    /// アンカーがまだ作られていない（失敗ではない）。
    Missing,
    Valid {
        seq: u64,
        shards: usize,
    },
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub contests: Vec<ContestRow>,
    /// シャードをまたいで 2 回以上現れた `ballot_id` の数。
    pub duplicate_ballots: usize,
    pub anchor: AnchorCheck,
}

impl AuditReport {
    pub fn is_ok(&self) -> bool {
        self.contests.iter().all(ContestRow::is_consistent)
            && self.duplicate_ballots == 0
            && !matches!(self.anchor, AnchorCheck::Invalid(_))
    }
}

/// 全シャードの検証結果（すべて成功していること）から、突合・重複・アンカーの確認をする。
///
/// participation とチェーンを**票ごとに結び付けるのではなく**、投票用紙別の件数だけを比べる
/// （秘密投票のため、投票者と票は突き合わせられない）。
pub fn audit(source: &dyn ChainSource, reports: &[&ShardReport]) -> anyhow::Result<AuditReport> {
    let counts = source.audit_counts()?;

    let mut sealed: BTreeMap<String, u64> = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut duplicate_ballots = 0;
    for report in reports {
        for (contest, n) in &report.contests {
            *sealed.entry(contest.to_string()).or_default() += n;
        }
        for id in &report.ballot_ids {
            if !seen.insert(*id) {
                duplicate_ballots += 1;
            }
        }
    }
    // 投票用紙が数千枚ある（47 都道府県規模）ので、API の集計は ID で引ける形にしておく。
    let api_counts: HashMap<&str, &shared_types::ContestCountsDto> = counts
        .contests
        .iter()
        .map(|c| (c.contest_id.as_str(), c))
        .collect();
    let contests: BTreeSet<&str> = sealed
        .keys()
        .map(String::as_str)
        .chain(api_counts.keys().copied())
        .collect();
    let rows = contests
        .into_iter()
        .map(|contest_id| {
            let api = api_counts.get(contest_id);
            ContestRow {
                contest_id: contest_id.to_string(),
                participation: api.map_or(0, |c| c.participation),
                sealed: sealed.get(contest_id).copied().unwrap_or(0),
                pending: api.map_or(0, |c| c.pending),
            }
        })
        .collect();

    let anchor = match source.anchor()? {
        None => AnchorCheck::Missing,
        Some(dto) => check_anchor(&dto, reports),
    };
    Ok(AuditReport {
        contests: rows,
        duplicate_ballots,
        anchor,
    })
}

/// アンカーの署名・ハッシュと、各 head が実際のチェーンのブロックと一致することを確認する。
fn check_anchor(dto: &AnchorDto, reports: &[&ShardReport]) -> AnchorCheck {
    let invalid = |msg: String| AnchorCheck::Invalid(msg);
    let anchor = match anchor_from_dto(dto) {
        Ok(anchor) => anchor,
        Err(e) => return invalid(e),
    };
    let Some(first) = reports.first() else {
        return invalid("検証済みのシャードがありません".to_string());
    };
    let verifier = match Ed25519Verifier::from_public_key(&first.public_key) {
        Ok(v) => v,
        Err(_) => return invalid("公開鍵が不正です".to_string()),
    };
    if let Err(e) = verify_anchor(&anchor, &verifier) {
        return invalid(e.to_string());
    }
    if anchor.heads.len() != reports.len() {
        return invalid(format!(
            "アンカーのシャード数（{}）が検証したシャード数（{}）と異なります",
            anchor.heads.len(),
            reports.len()
        ));
    }
    for head in &anchor.heads {
        let Some(report) = reports.iter().find(|r| r.shard == head.shard) else {
            return invalid(format!(
                "アンカーにあるシャード {} を検証していません",
                head.shard
            ));
        };
        let actual = usize::try_from(head.height)
            .ok()
            .and_then(|h| report.block_hashes.get(h));
        // アンカーの時点のブロックが、今のチェーンにそのまま残っていること（巻き戻し・差し替えの検出）。
        match actual {
            Some(hash) if *hash == head.block_hash => {}
            Some(_) => {
                return invalid(format!(
                    "シャード {} の高さ {} のブロックが、アンカーの記録と異なります",
                    head.shard, head.height
                ));
            }
            None => {
                return invalid(format!(
                    "シャード {} は、アンカーの高さ {} に達していません（巻き戻された可能性）",
                    head.shard, head.height
                ));
            }
        }
    }
    AnchorCheck::Valid {
        seq: anchor.seq,
        shards: anchor.heads.len(),
    }
}

fn anchor_from_dto(dto: &AnchorDto) -> Result<Anchor, String> {
    let bad = |what: &str, e: hex::HexError| format!("{what}: {e}");
    let mut heads = Vec::with_capacity(dto.heads.len());
    for h in &dto.heads {
        heads.push(ShardHead {
            shard: h.shard,
            height: h.height,
            block_hash: hex::decode_array::<32>(&h.block_hash).map_err(|e| bad("block_hash", e))?,
        });
    }
    Ok(Anchor {
        seq: dto.seq,
        anchor_minute: dto.anchor_minute,
        prev_anchor_hash: hex::decode_array::<32>(&dto.prev_anchor_hash)
            .map_err(|e| bad("prev_anchor_hash", e))?,
        heads,
        anchor_hash: hex::decode_array::<32>(&dto.anchor_hash)
            .map_err(|e| bad("anchor_hash", e))?,
        signature: hex::decode_array::<64>(&dto.signature).map_err(|e| bad("signature", e))?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use domain::{Ed25519Signer, Signer, genesis, seal_block};
    use shared_types::{BallotDto, HeaderDto};

    use super::*;

    /// メモリ上のシャード（各シャードの DTO 列）を返すフェイク。
    pub(crate) struct FakeSource {
        pub(crate) shards: Vec<Vec<BlockDto>>,
        pub(crate) public_key: String,
        pub(crate) anchor: Option<AnchorDto>,
        pub(crate) counts: AuditCountsResponse,
    }

    impl ChainSource for FakeSource {
        fn head(&self, shard: u16) -> anyhow::Result<Option<HeadDto>> {
            Ok(self.shards.get(usize::from(shard)).and_then(|blocks| {
                let last = blocks.last()?;
                Some(HeadDto {
                    shard,
                    height: last.header.height,
                    block_hash: last.block_hash.clone(),
                    ballot_count: last.header.ballot_count,
                    sealed_at_minute: last.header.sealed_at_minute,
                    signer_public_key: self.public_key.clone(),
                })
            }))
        }

        fn block(&self, shard: u16, height: u64) -> anyhow::Result<Option<BlockDto>> {
            Ok(self
                .shards
                .get(usize::from(shard))
                .and_then(|blocks| blocks.get(usize::try_from(height).ok()?))
                .cloned())
        }

        fn anchor(&self) -> anyhow::Result<Option<AnchorDto>> {
            Ok(self.anchor.clone())
        }

        fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse> {
            Ok(self.counts.clone())
        }
    }

    fn to_dto(b: &Block) -> BlockDto {
        BlockDto {
            header: HeaderDto {
                version: b.header.version,
                height: b.header.height,
                prev_hash: hex::encode(&b.header.prev_hash),
                merkle_root: hex::encode(&b.header.merkle_root),
                ballot_count: b.header.ballot_count,
                sealed_at_minute: b.header.sealed_at_minute,
            },
            ballots: b
                .ballots
                .iter()
                .map(|x| BallotDto {
                    ballot_id: hex::encode(&x.ballot_id.0),
                    contest_id: x.contest_id.to_string(),
                    candidate_id: x.candidate_id.to_string(),
                    blank: x.candidate_id.is_blank(),
                    district_name: None,
                    candidate_name: None,
                    party: None,
                })
                .collect(),
            block_hash: hex::encode(&b.block_hash),
            signature: hex::encode(&b.signature),
            ballots_revealed: true,
            signer_public_key: None,
        }
    }

    fn signer() -> Ed25519Signer {
        Ed25519Signer::from_seed(&[3u8; 32])
    }

    /// ジェネシス + `sizes` の各ブロックからなるチェーン。
    pub(crate) fn chain(sizes: &[u32], salt: u8) -> Vec<BlockDto> {
        let signer = signer();
        let mut blocks = vec![genesis(&signer, 100)];
        let mut next = 0u32;
        for (i, &n) in sizes.iter().enumerate() {
            let ballots = (next..next + n)
                .map(|k| {
                    let mut id = [salt; 16];
                    id[..4].copy_from_slice(&k.to_be_bytes());
                    Ballot {
                        ballot_id: BallotId::from_random_bytes(id),
                        contest_id: ContestId::parse(&format!(
                            "2026-general/shugiin_smd.13.0{}",
                            1 + k % 2
                        ))
                        .expect("valid"),
                        candidate_id: CandidateId::parse(&format!(
                            "shugiin_smd.13.0{}.c{}",
                            1 + k % 2,
                            1 + k % 4
                        ))
                        .expect("valid"),
                    }
                })
                .collect();
            next += n;
            let prev = blocks.last().expect("non-empty");
            blocks.push(seal_block(prev, ballots, 101 + i as u64, &signer).expect("seal"));
        }
        blocks.iter().map(to_dto).collect()
    }

    pub(crate) fn source(shards: Vec<Vec<BlockDto>>) -> FakeSource {
        FakeSource {
            shards,
            public_key: hex::encode(&signer().public_key()),
            anchor: None,
            counts: AuditCountsResponse { contests: vec![] },
        }
    }

    #[test]
    fn valid_chains_pass_and_report_counts() {
        let src = source(vec![chain(&[5, 3], 1), chain(&[4], 2)]);
        let verdicts = verify_all(&src, None).expect("fetch");
        let summary: Vec<(u16, usize, usize)> = verdicts
            .iter()
            .map(|v| match v {
                ShardVerdict::Valid(r) => (r.shard, r.blocks, r.ballots),
                other => panic!("unexpected: {other:?}"),
            })
            .collect();
        assert_eq!(summary, vec![(0, 3, 8), (1, 2, 4)]);
    }

    #[test]
    fn tampered_ballot_is_reported_for_that_shard_only() {
        let mut shards = vec![chain(&[5, 3], 1), chain(&[4], 2)];
        shards[1][1].ballots[0].candidate_id = "shugiin_smd.13.01.c99".to_string();
        let verdicts = verify_all(&source(shards), None).expect("fetch");
        assert!(matches!(verdicts[0], ShardVerdict::Valid(_)));
        assert_eq!(
            verdicts[1],
            ShardVerdict::Invalid {
                shard: 1,
                failure: VerifyFailure::Chain(ChainError::MerkleRootMismatch { height: 1 })
            }
        );
    }

    #[test]
    fn pinned_key_overrides_the_key_served_by_the_api() {
        let src = source(vec![chain(&[2], 1)]);
        // API が示す鍵は正しいが、利用者が別の鍵を固定した場合は署名不一致になる。
        let other = Ed25519Signer::from_seed(&[4u8; 32]).public_key();
        let verdicts = verify_all(&src, Some(&other)).expect("fetch");
        assert_eq!(
            verdicts[0],
            ShardVerdict::Invalid {
                shard: 0,
                failure: VerifyFailure::Chain(ChainError::SignatureInvalid { height: 0 })
            }
        );
        // 正しい鍵を固定すれば通る。
        let right = signer().public_key();
        assert!(matches!(
            verify_all(&src, Some(&right)).expect("fetch")[0],
            ShardVerdict::Valid(_)
        ));
    }

    #[test]
    fn detects_inconsistent_head_missing_block_and_malformed_data() {
        // head の block_hash が偽り。
        struct LyingHead(FakeSource);
        impl ChainSource for LyingHead {
            fn head(&self, shard: u16) -> anyhow::Result<Option<HeadDto>> {
                Ok(self.0.head(shard)?.map(|mut h| {
                    h.block_hash = hex::encode(&[0u8; 32]);
                    h
                }))
            }
            fn block(&self, shard: u16, height: u64) -> anyhow::Result<Option<BlockDto>> {
                self.0.block(shard, height)
            }
            fn anchor(&self) -> anyhow::Result<Option<AnchorDto>> {
                self.0.anchor()
            }
            fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse> {
                self.0.audit_counts()
            }
        }
        let v = verify_all(&LyingHead(source(vec![chain(&[2], 1)])), None).expect("fetch");
        assert_eq!(
            v[0],
            ShardVerdict::Invalid {
                shard: 0,
                failure: VerifyFailure::HeadMismatch
            }
        );

        // head は高さ 2 と言うが、ブロック 1 が取得できない。
        struct Hole(FakeSource);
        impl ChainSource for Hole {
            fn head(&self, shard: u16) -> anyhow::Result<Option<HeadDto>> {
                self.0.head(shard)
            }
            fn block(&self, shard: u16, height: u64) -> anyhow::Result<Option<BlockDto>> {
                Ok(if height == 1 {
                    None
                } else {
                    self.0.block(shard, height)?
                })
            }
            fn anchor(&self) -> anyhow::Result<Option<AnchorDto>> {
                self.0.anchor()
            }
            fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse> {
                self.0.audit_counts()
            }
        }
        let v = verify_all(&Hole(source(vec![chain(&[2, 2], 1)])), None).expect("fetch");
        assert_eq!(
            v[0],
            ShardVerdict::Invalid {
                shard: 0,
                failure: VerifyFailure::MissingBlock(1)
            }
        );

        // hex が壊れている。
        let mut shards = vec![chain(&[2], 1)];
        shards[0][1].signature = "zz".to_string();
        let v = verify_all(&source(shards), None).expect("fetch");
        assert!(matches!(
            &v[0],
            ShardVerdict::Invalid {
                failure: VerifyFailure::Malformed(_),
                ..
            }
        ));
    }

    #[test]
    fn no_chain_at_all_is_a_runtime_error() {
        assert!(verify_all(&source(vec![]), None).is_err());
    }

    /// 取得エラー（通信障害など）は「不整合」ではなく実行時エラーとして扱う。
    #[test]
    fn fetch_errors_propagate() {
        struct Broken;
        impl ChainSource for Broken {
            fn head(&self, _: u16) -> anyhow::Result<Option<HeadDto>> {
                anyhow::bail!("接続できません")
            }
            fn block(&self, _: u16, _: u64) -> anyhow::Result<Option<BlockDto>> {
                anyhow::bail!("接続できません")
            }
            fn anchor(&self) -> anyhow::Result<Option<AnchorDto>> {
                anyhow::bail!("接続できません")
            }
            fn audit_counts(&self) -> anyhow::Result<AuditCountsResponse> {
                anyhow::bail!("接続できません")
            }
        }
        assert!(verify_all(&Broken, None).is_err());
    }

    // --- 突合・重複・アンカー ---

    pub(crate) fn reports(src: &FakeSource) -> Vec<ShardReport> {
        verify_all(src, None)
            .expect("fetch")
            .into_iter()
            .map(|v| match v {
                ShardVerdict::Valid(r) => r,
                other => panic!("unexpected: {other:?}"),
            })
            .collect()
    }

    /// チェーン内の票数（投票用紙別）に、`extra_participation` / `pending` を足した API の集計値を作る。
    pub(crate) fn counts_for(
        reports: &[ShardReport],
        extra_participation: i64,
        pending: u64,
    ) -> AuditCountsResponse {
        let mut sealed: BTreeMap<String, u64> = BTreeMap::new();
        for r in reports {
            for (c, n) in &r.contests {
                *sealed.entry(c.to_string()).or_default() += n;
            }
        }
        AuditCountsResponse {
            contests: sealed
                .into_iter()
                .map(|(contest_id, n)| shared_types::ContestCountsDto {
                    contest_id,
                    participation: u64::try_from(
                        i64::try_from(n + pending).expect("fits") + extra_participation,
                    )
                    .expect("non-negative"),
                    pending,
                })
                .collect(),
        }
    }

    fn audit_with(mut src: FakeSource, extra: i64, pending: u64) -> AuditReport {
        let r = reports(&src);
        src.counts = counts_for(&r, extra, pending);
        let refs: Vec<&ShardReport> = r.iter().collect();
        audit(&src, &refs).expect("audit")
    }

    #[test]
    fn participation_matches_the_sealed_ballots_per_ballot_item() {
        let report = audit_with(source(vec![chain(&[5, 3], 1), chain(&[4], 2)]), 0, 0);
        assert!(report.is_ok());
        // 2 つの投票用紙の票数が、2 シャードの合計で集計されている。
        let rows: Vec<(&str, u64, u64, u64)> = report
            .contests
            .iter()
            .map(|c| (c.contest_id.as_str(), c.participation, c.sealed, c.pending))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 12);
        assert!(rows.iter().all(|r| r.1 == r.2 && r.3 == 0));
        assert_eq!(report.duplicate_ballots, 0);
        assert_eq!(report.anchor, AnchorCheck::Missing);
    }

    #[test]
    fn pending_ballots_are_part_of_the_reconciliation() {
        // 封印待ちの票があっても、participation = 封印済み + 封印待ち なら一致。
        let report = audit_with(source(vec![chain(&[5], 1)]), 0, 2);
        assert!(report.is_ok());
        assert!(report.contests.iter().all(|c| c.pending == 2));
    }

    #[test]
    fn lost_or_extra_ballots_break_the_reconciliation() {
        // 票の消失: participation が、封印済み + 封印待ちより多い。
        let lost = audit_with(source(vec![chain(&[5], 1)]), 1, 0);
        assert!(!lost.is_ok());
        assert!(lost.contests.iter().all(|c| !c.is_consistent()));
        // 票の水増し・二重封印: participation より、チェーン内の票が多い。
        let extra = audit_with(source(vec![chain(&[5], 1)]), -1, 0);
        assert!(!extra.is_ok());
    }

    #[test]
    fn a_ballot_item_only_in_the_chain_or_only_in_participation_is_a_mismatch() {
        let mut src = source(vec![chain(&[5], 1)]);
        let r = reports(&src);
        // API には別の投票用紙の participation があるが、チェーンには 1 票もない。
        let mut counts = counts_for(&r, 0, 0);
        counts.contests.push(shared_types::ContestCountsDto {
            contest_id: "2026-general/shugiin_smd.13.09".to_string(),
            participation: 4,
            pending: 0,
        });
        src.counts = counts;
        let refs: Vec<&ShardReport> = r.iter().collect();
        let report = audit(&src, &refs).expect("audit");
        assert!(!report.is_ok());
        let nine = report
            .contests
            .iter()
            .find(|c| c.contest_id == "2026-general/shugiin_smd.13.09")
            .expect("row");
        assert_eq!((nine.participation, nine.sealed), (4, 0));

        // 逆に、API の集計にない投票用紙の票がチェーンにある。
        let mut src = source(vec![chain(&[5], 1)]);
        src.counts = AuditCountsResponse { contests: vec![] };
        let refs: Vec<&ShardReport> = r.iter().collect();
        assert!(!audit(&src, &refs).expect("audit").is_ok());
    }

    #[test]
    fn the_same_ballot_id_in_two_shards_is_detected() {
        // 同じ salt のチェーンを 2 つのシャードに置くと、同じ ballot_id が 2 回現れる（二重封印）。
        let report = audit_with(source(vec![chain(&[3], 7), chain(&[3], 7)]), 0, 0);
        assert_eq!(report.duplicate_ballots, 3);
        assert!(!report.is_ok());
    }

    fn anchor_dto(a: &domain::Anchor) -> AnchorDto {
        AnchorDto {
            seq: a.seq,
            anchor_minute: a.anchor_minute,
            prev_anchor_hash: hex::encode(&a.prev_anchor_hash),
            heads: a
                .heads
                .iter()
                .map(|h| shared_types::HeadRefDto {
                    shard: h.shard,
                    height: h.height,
                    block_hash: hex::encode(&h.block_hash),
                })
                .collect(),
            anchor_hash: hex::encode(&a.anchor_hash),
            signature: hex::encode(&a.signature),
        }
    }

    /// 各シャードの `height` 番目のブロックを head とするアンカー。
    fn anchor_at(
        reports: &[ShardReport],
        heights: &[u64],
        signer: &Ed25519Signer,
    ) -> domain::Anchor {
        let heads = reports
            .iter()
            .zip(heights)
            .map(|(r, h)| ShardHead {
                shard: r.shard,
                height: *h,
                block_hash: r
                    .block_hashes
                    .get(usize::try_from(*h).expect("fits"))
                    .copied()
                    .unwrap_or([0xee; 32]),
            })
            .collect();
        domain::build_anchor(1, 30_000_000, [0; 32], heads, signer).expect("anchor")
    }

    fn audit_with_anchor(anchor: Option<AnchorDto>) -> AuditReport {
        let mut src = source(vec![chain(&[5, 3], 1), chain(&[4], 2)]);
        let r = reports(&src);
        src.counts = counts_for(&r, 0, 0);
        src.anchor = anchor;
        let refs: Vec<&ShardReport> = r.iter().collect();
        audit(&src, &refs).expect("audit")
    }

    fn current_reports() -> Vec<ShardReport> {
        reports(&source(vec![chain(&[5, 3], 1), chain(&[4], 2)]))
    }

    #[test]
    fn a_valid_anchor_matches_the_chain() {
        let r = current_reports();
        // アンカーの時点（shard 0 は高さ 1、shard 1 は高さ 1）は、今のチェーンにそのまま残っている。
        let report = audit_with_anchor(Some(anchor_dto(&anchor_at(&r, &[1, 1], &signer()))));
        assert_eq!(report.anchor, AnchorCheck::Valid { seq: 1, shards: 2 });
        assert!(report.is_ok());
    }

    #[test]
    fn a_rolled_back_or_rewritten_chain_is_caught_by_the_anchor() {
        let r = current_reports();
        // アンカーの高さにチェーンが達していない（巻き戻し）。
        let ahead = audit_with_anchor(Some(anchor_dto(&anchor_at(&r, &[2, 5], &signer()))));
        assert!(
            matches!(ahead.anchor, AnchorCheck::Invalid(_)),
            "{:?}",
            ahead.anchor
        );
        assert!(!ahead.is_ok());

        // 同じ高さのブロックが、アンカーの記録と異なる（差し替え）。
        let mut anchor = anchor_at(&r, &[1, 1], &signer());
        anchor.heads[0].block_hash = [0x55; 32];
        anchor.anchor_hash = domain::compute_anchor_hash(
            anchor.seq,
            anchor.anchor_minute,
            &anchor.prev_anchor_hash,
            &anchor.heads,
        )
        .expect("hash");
        anchor.signature = signer().sign(&anchor.anchor_hash);
        let swapped = audit_with_anchor(Some(anchor_dto(&anchor)));
        assert!(
            matches!(swapped.anchor, AnchorCheck::Invalid(_)),
            "{:?}",
            swapped.anchor
        );
    }

    #[test]
    fn forged_or_incomplete_anchors_are_rejected() {
        let r = current_reports();
        let good = anchor_at(&r, &[1, 1], &signer());

        // 別の鍵の署名 / 内容の改ざん / 一部のシャードしかない。
        let other = Ed25519Signer::from_seed(&[99u8; 32]);
        let forged = audit_with_anchor(Some(anchor_dto(&anchor_at(&r, &[1, 1], &other))));
        assert!(matches!(forged.anchor, AnchorCheck::Invalid(_)));

        let mut tampered = anchor_dto(&good);
        tampered.heads[1].height += 1;
        assert!(matches!(
            audit_with_anchor(Some(tampered)).anchor,
            AnchorCheck::Invalid(_)
        ));

        let partial =
            domain::build_anchor(1, 1, [0; 32], vec![good.heads[0]], &signer()).expect("anchor");
        assert!(matches!(
            audit_with_anchor(Some(anchor_dto(&partial))).anchor,
            AnchorCheck::Invalid(_)
        ));

        let mut broken_hex = anchor_dto(&good);
        broken_hex.signature = "zz".to_string();
        assert!(matches!(
            audit_with_anchor(Some(broken_hex)).anchor,
            AnchorCheck::Invalid(_)
        ));
    }
}
