//! ドメイン層: ハッシュチェーン（簡易ブロックチェーン）の型・正規化エンコード・
//! Merkle 木・ブロック封印・チェーン検証・署名。
//!
//! IO・async ランタイム・DB クレートには依存しない。乱数も持たず、
//! 鍵の種などは呼び出し側から受け取る。

pub mod anchor;
pub mod chain;
pub mod election;
pub mod election_state;
pub mod encoding;
pub mod ids;
pub mod merkle;
pub mod revote;
pub mod seal_policy;
pub mod shard;
pub mod signature;
pub mod types;
pub mod voter;

pub use anchor::{
    Anchor, AnchorError, HeadsChange, ShardHead, build_anchor, compare_heads, compute_anchor_hash,
    verify_anchor, verify_anchor_link,
};
pub use chain::{BLOCK_VERSION, ChainError, SealError, genesis, seal_block, verify_chain};
pub use election::{
    Candidate, Contest, District, Election, ElectionError, ElectionType, VotingMethod,
};
pub use election_state::{
    ElectionPhase, ElectionRules, Period, VoteGate, automatic_transition, vote_gate,
    voting_started_at,
};
pub use encoding::ballot_hash;
pub use ids::{
    BLANK_CANDIDATE_ID, CandidateCode, CandidateId, ContestId, DistrictId, ElectionId,
    ElectionTypeCode, IdError,
};
pub use merkle::{InclusionProof, Side, inclusion_proof, merkle_root, verify_inclusion};
pub use revote::{PlacedBallot, RevoteAnalysis, RevoteError, analyze_revotes};
pub use seal_policy::{PolicyError, SealDecision, SealPolicy, decide, decide_close, window_start};
pub use shard::{ShardId, shard_for, shard_for_slot};
pub use signature::{Ed25519Signer, Ed25519Verifier, SignatureError, Signer, Verifier};
pub use types::{Ballot, BallotId, Block, BlockHeader, Hash32, RevoteLink, SignatureBytes, Slot};
pub use voter::{VoterId, VoterIdError};
