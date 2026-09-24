//! ブロックの封印とチェーン検証。

use crate::encoding::{ballot_order_key, block_hash};
use crate::merkle::{empty_root, merkle_root};
use crate::signature::{Signer, Verifier};
use crate::types::{Ballot, Block, BlockHeader, Hash32};

/// ブロックの形式の版。2: 票の `contest_id` / `candidate_id` を、数値（各 4 バイト）から、長さ接頭辞つきの
/// 文字列 ID（[`crate::ids`]）に変えた（ADR 0013）。
pub const BLOCK_VERSION: u16 = 2;

/// 封印（`seal_block`）の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("票が 0 件のブロックは作れません")]
    EmptyBallots,
    #[error("票の件数が上限（u32）を超えています")]
    TooManyBallots,
    #[error("ブロック高がオーバーフローします")]
    HeightOverflow,
    #[error("同じ ballot_id の票が重複しています")]
    DuplicateBallot,
}

/// チェーン検証の失敗。`height` は先頭からの位置（ジェネシスが 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("チェーンが空です")]
    EmptyChain,
    #[error("height={height}: ジェネシスブロックとして不正です（{reason}）")]
    GenesisInvalid { height: u64, reason: &'static str },
    #[error("height={height}: ブロック高が連続していません（ヘッダの値: {actual}）")]
    HeightMismatch { height: u64, actual: u64 },
    #[error("height={height}: prev_hash が直前ブロックの block_hash と一致しません")]
    PrevHashMismatch { height: u64 },
    #[error("height={height}: 票が 0 件のブロックです")]
    EmptyBlock { height: u64 },
    #[error(
        "height={height}: ballot_count がヘッダ（{header}）と実際の票数（{actual}）で一致しません"
    )]
    BallotCountMismatch {
        height: u64,
        header: u32,
        actual: usize,
    },
    #[error("height={height}: 票が ballot_id のハッシュ昇順ではありません")]
    BallotOrderInvalid { height: u64 },
    #[error("height={height}: Merkle 根が票から再計算した値と一致しません")]
    MerkleRootMismatch { height: u64 },
    #[error("height={height}: block_hash がヘッダから再計算した値と一致しません")]
    BlockHashMismatch { height: u64 },
    #[error("height={height}: 署名が不正です")]
    SignatureInvalid { height: u64 },
}

impl ChainError {
    /// 失敗したブロックの位置。チェーン全体に関わるエラーでは `None`。
    pub fn height(&self) -> Option<u64> {
        match *self {
            Self::EmptyChain => None,
            Self::GenesisInvalid { height, .. }
            | Self::HeightMismatch { height, .. }
            | Self::PrevHashMismatch { height }
            | Self::EmptyBlock { height }
            | Self::BallotCountMismatch { height, .. }
            | Self::BallotOrderInvalid { height }
            | Self::MerkleRootMismatch { height }
            | Self::BlockHashMismatch { height }
            | Self::SignatureInvalid { height } => Some(height),
        }
    }

    /// エラー種別名（表示・ログ用）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmptyChain => "EmptyChain",
            Self::GenesisInvalid { .. } => "GenesisInvalid",
            Self::HeightMismatch { .. } => "HeightMismatch",
            Self::PrevHashMismatch { .. } => "PrevHashMismatch",
            Self::EmptyBlock { .. } => "EmptyBlock",
            Self::BallotCountMismatch { .. } => "BallotCountMismatch",
            Self::BallotOrderInvalid { .. } => "BallotOrderInvalid",
            Self::MerkleRootMismatch { .. } => "MerkleRootMismatch",
            Self::BlockHashMismatch { .. } => "BlockHashMismatch",
            Self::SignatureInvalid { .. } => "SignatureInvalid",
        }
    }
}

const ZERO_HASH: Hash32 = [0u8; 32];

/// ヘッダからハッシュと署名を付けてブロックを組み立てる。
fn assemble(header: BlockHeader, ballots: Vec<Ballot>, signer: &dyn Signer) -> Block {
    let hash = block_hash(&header);
    Block {
        header,
        ballots,
        block_hash: hash,
        signature: signer.sign(&hash),
    }
}

/// ジェネシスブロック: 高さ 0、`prev_hash` は全ゼロ、票 0 件。
pub fn genesis(signer: &dyn Signer, sealed_at_minute: u64) -> Block {
    let header = BlockHeader {
        version: BLOCK_VERSION,
        height: 0,
        prev_hash: ZERO_HASH,
        merkle_root: empty_root(),
        ballot_count: 0,
        sealed_at_minute,
    };
    assemble(header, Vec::new(), signer)
}

/// `prev` の次のブロックを封印する。
///
/// `ballots` は所有権ごと受け取り、`ballot_id` のハッシュ昇順に並べ替えて
/// ブロックに移す（呼び出し側は到着順のまま渡してよい）。
pub fn seal_block(
    prev: &Block,
    ballots: Vec<Ballot>,
    sealed_at_minute: u64,
    signer: &dyn Signer,
) -> Result<Block, SealError> {
    if ballots.is_empty() {
        return Err(SealError::EmptyBallots);
    }
    let ballot_count = u32::try_from(ballots.len()).map_err(|_| SealError::TooManyBallots)?;
    let height = prev
        .header
        .height
        .checked_add(1)
        .ok_or(SealError::HeightOverflow)?;

    let mut keyed: Vec<(Hash32, Ballot)> = ballots
        .into_iter()
        .map(|b| (ballot_order_key(&b.ballot_id), b))
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    if keyed.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(SealError::DuplicateBallot);
    }
    let ballots: Vec<Ballot> = keyed.into_iter().map(|(_, b)| b).collect();

    let header = BlockHeader {
        version: BLOCK_VERSION,
        height,
        prev_hash: prev.block_hash,
        merkle_root: merkle_root(&ballots),
        ballot_count,
        sealed_at_minute,
    };
    Ok(assemble(header, ballots, signer))
}

/// チェーン全体を検証する。最初に見つかった不整合を返す。
///
/// ブロックは借用して読むだけで、所有権は移さない。
pub fn verify_chain(blocks: &[Block], verifier: &dyn Verifier) -> Result<(), ChainError> {
    if blocks.is_empty() {
        return Err(ChainError::EmptyChain);
    }
    let mut prev: Option<&Block> = None;
    for (index, block) in blocks.iter().enumerate() {
        verify_block(block, prev, index as u64, verifier)?;
        prev = Some(block);
    }
    Ok(())
}

fn verify_block(
    block: &Block,
    prev: Option<&Block>,
    height: u64,
    verifier: &dyn Verifier,
) -> Result<(), ChainError> {
    let header = &block.header;

    match prev {
        None => {
            let genesis_err = |reason| ChainError::GenesisInvalid { height, reason };
            if header.height != 0 {
                return Err(genesis_err("height が 0 ではありません"));
            }
            if header.prev_hash != ZERO_HASH {
                return Err(genesis_err("prev_hash が全ゼロではありません"));
            }
            if !block.ballots.is_empty() {
                return Err(genesis_err("票を含んでいます"));
            }
        }
        Some(prev) => {
            if header.height != height {
                return Err(ChainError::HeightMismatch {
                    height,
                    actual: header.height,
                });
            }
            if header.prev_hash != prev.block_hash {
                return Err(ChainError::PrevHashMismatch { height });
            }
            if block.ballots.is_empty() {
                return Err(ChainError::EmptyBlock { height });
            }
        }
    }

    if u64::from(header.ballot_count) != block.ballots.len() as u64 {
        return Err(ChainError::BallotCountMismatch {
            height,
            header: header.ballot_count,
            actual: block.ballots.len(),
        });
    }

    let keys: Vec<Hash32> = block
        .ballots
        .iter()
        .map(|b| ballot_order_key(&b.ballot_id))
        .collect();
    if !keys.windows(2).all(|w| w[0] < w[1]) {
        return Err(ChainError::BallotOrderInvalid { height });
    }

    if header.merkle_root != merkle_root(&block.ballots) {
        return Err(ChainError::MerkleRootMismatch { height });
    }

    let recomputed = block_hash(header);
    if block.block_hash != recomputed {
        return Err(ChainError::BlockHashMismatch { height });
    }

    verifier
        .verify(&recomputed, &block.signature)
        .map_err(|_| ChainError::SignatureInvalid { height })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CandidateId, ContestId, DistrictId, ElectionId};
    use crate::signature::Ed25519Signer;
    use crate::types::BallotId;

    fn ballot(i: u32) -> Ballot {
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&i.to_be_bytes());
        let district = DistrictId::new("shugiin_smd.13.01").expect("valid");
        Ballot {
            ballot_id: BallotId::from_random_bytes(id),
            contest_id: ContestId::new(&ElectionId::new("2026-general").expect("valid"), &district),
            candidate_id: CandidateId::new(&district, u64::from(i % 3) + 1).expect("valid"),
        }
    }

    /// ジェネシス + 3 ブロック（5・4・3 票）のチェーン。
    fn sample_chain(signer: &Ed25519Signer) -> Vec<Block> {
        let mut chain = vec![genesis(signer, 100)];
        let mut next = 0u32;
        for (n, minute) in [(5u32, 101u64), (4, 102), (3, 103)] {
            let ballots: Vec<Ballot> = (next..next + n).map(ballot).collect();
            next += n;
            let block = seal_block(&chain[chain.len() - 1], ballots, minute, signer)
                .expect("seal should succeed");
            chain.push(block);
        }
        chain
    }

    fn signer() -> Ed25519Signer {
        Ed25519Signer::from_seed(&[42u8; 32])
    }

    #[test]
    fn valid_chain_passes() {
        let signer = signer();
        let chain = sample_chain(&signer);
        assert_eq!(chain.len(), 4);
        assert_eq!(verify_chain(&chain, &signer.verifier()), Ok(()));
        // 検証は借用のみなので、検証後も同じチェーンを使える。
        assert_eq!(chain[1].header.height, 1);
    }

    #[test]
    fn genesis_shape() {
        let g = genesis(&signer(), 7);
        assert_eq!(g.header.height, 0);
        assert_eq!(g.header.prev_hash, [0u8; 32]);
        assert_eq!(g.header.ballot_count, 0);
        assert_eq!(g.header.merkle_root, empty_root());
        assert!(g.ballots.is_empty());
    }

    #[test]
    fn seal_sorts_by_ballot_id_hash_regardless_of_arrival_order() {
        let signer = signer();
        let g = genesis(&signer, 0);
        let ballots: Vec<Ballot> = (0..20).map(ballot).collect();
        let mut reversed = ballots.clone();
        reversed.reverse();

        let a = seal_block(&g, ballots, 1, &signer).expect("seal");
        let b = seal_block(&g, reversed, 1, &signer).expect("seal");
        assert_eq!(a, b);
        let keys: Vec<Hash32> = a
            .ballots
            .iter()
            .map(|x| ballot_order_key(&x.ballot_id))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn seal_rejects_empty_and_duplicate() {
        let signer = signer();
        let g = genesis(&signer, 0);
        assert_eq!(
            seal_block(&g, vec![], 1, &signer),
            Err(SealError::EmptyBallots)
        );
        // 同じ ballot_id で候補者だけ違う票（二重投票）も拒否する。
        let a = ballot(1);
        let b = Ballot {
            candidate_id: CandidateId::parse("shugiin_smd.13.01.c99").expect("valid"),
            ..a.clone()
        };
        assert_eq!(
            seal_block(&g, vec![a, b], 1, &signer),
            Err(SealError::DuplicateBallot)
        );
    }

    /// `mutate` でチェーンを壊し、期待するエラー種別・位置になることを確認する。
    fn assert_error(mutate: impl FnOnce(&mut Vec<Block>), kind: &str, height: u64) {
        let signer = signer();
        let mut chain = sample_chain(&signer);
        mutate(&mut chain);
        let err = verify_chain(&chain, &signer.verifier()).expect_err("should fail");
        assert_eq!((err.kind(), err.height()), (kind, Some(height)), "{err}");
    }

    #[test]
    fn empty_chain() {
        let signer = signer();
        assert_eq!(
            verify_chain(&[], &signer.verifier()),
            Err(ChainError::EmptyChain)
        );
    }

    #[test]
    fn detects_genesis_invalid() {
        assert_error(|c| c[0].header.prev_hash[0] = 1, "GenesisInvalid", 0);
        assert_error(|c| c[0].header.height = 1, "GenesisInvalid", 0);
        assert_error(|c| c[0].ballots.push(ballot(0)), "GenesisInvalid", 0);
    }

    #[test]
    fn detects_height_mismatch() {
        assert_error(|c| c[2].header.height = 5, "HeightMismatch", 2);
    }

    #[test]
    fn detects_prev_hash_mismatch() {
        assert_error(|c| c[3].header.prev_hash[0] ^= 1, "PrevHashMismatch", 3);
        // block_hash 自体が書き換わった場合は、そのブロック自身の再計算不一致で検出される。
        assert_error(|c| c[1].block_hash[0] ^= 1, "BlockHashMismatch", 1);
    }

    #[test]
    fn detects_empty_block() {
        assert_error(
            |c| {
                c[1].ballots.clear();
                c[1].header.ballot_count = 0;
                c[1].header.merkle_root = empty_root();
            },
            "EmptyBlock",
            1,
        );
    }

    #[test]
    fn detects_ballot_count_mismatch() {
        assert_error(|c| c[2].header.ballot_count += 1, "BallotCountMismatch", 2);
        assert_error(
            |c| {
                c[2].ballots.pop();
            },
            "BallotCountMismatch",
            2,
        );
    }

    #[test]
    fn detects_ballot_order_invalid() {
        assert_error(|c| c[2].ballots.swap(0, 1), "BallotOrderInvalid", 2);
    }

    #[test]
    fn detects_merkle_root_mismatch() {
        assert_error(
            |c| {
                c[2].ballots[0].candidate_id =
                    CandidateId::parse("shugiin_smd.13.01.c9").expect("valid")
            },
            "MerkleRootMismatch",
            2,
        );
        assert_error(|c| c[1].header.merkle_root[0] ^= 1, "MerkleRootMismatch", 1);
    }

    #[test]
    fn detects_block_hash_mismatch() {
        assert_error(
            |c| c[2].header.sealed_at_minute += 1,
            "BlockHashMismatch",
            2,
        );
        assert_error(|c| c[3].block_hash[31] ^= 1, "BlockHashMismatch", 3);
    }

    #[test]
    fn detects_signature_invalid() {
        assert_error(|c| c[1].signature[0] ^= 1, "SignatureInvalid", 1);
    }

    #[test]
    fn rejects_chain_signed_by_other_key() {
        let chain = sample_chain(&signer());
        let other = Ed25519Signer::from_seed(&[1u8; 32]).verifier();
        let err = verify_chain(&chain, &other).expect_err("should fail");
        assert_eq!(err, ChainError::SignatureInvalid { height: 0 });
    }
}
