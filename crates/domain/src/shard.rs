//! シャード決定。
//!
//! シャードは `ballot_id` から決める。`voter_id` から決めると、シャードが投票者と
//! 結びついて秘密投票の原則に反するため。`ballot_id` は乱数（UUIDv4）なので均等に散る。
//!
//! 再投票を認める選挙（`vote.allow_revote = true`）では、`slot`（再投票の仮名）から決める（[`shard_for_slot`]）。
//! 同じ slot の全版が同じシャード（1 本のチェーン）に入るので、順序と前の票へのつながりを、1 本のチェーンの中で
//! 検証できる（ADR 0022）。slot は投票者 ID には戻せない HMAC なので、シャードは投票者と結び付かない。

use std::num::NonZeroU16;

use crate::encoding::sha256_parts;
use crate::types::{BallotId, Slot};

/// シャード番号（0 始まり）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(pub u16);

const SHARD_DOMAIN: &[u8] = b"vote/shard/v1";
const SLOT_SHARD_DOMAIN: &[u8] = b"vote/shard-slot/v1";

/// `ballot_id` が属するシャード。結果は常に `0..shard_count` の範囲。
pub fn shard_for(ballot_id: &BallotId, shard_count: NonZeroU16) -> ShardId {
    shard_of(sha256_parts(&[SHARD_DOMAIN, &ballot_id.0]), shard_count)
}

/// 再投票の slot が属するシャード（`hash(slot) mod shard_count`）。同じ slot の票は、常に同じシャード。
pub fn shard_for_slot(slot: &Slot, shard_count: NonZeroU16) -> ShardId {
    shard_of(sha256_parts(&[SLOT_SHARD_DOMAIN, &slot.0]), shard_count)
}

fn shard_of(digest: crate::types::Hash32, shard_count: NonZeroU16) -> ShardId {
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let index = u64::from_be_bytes(head) % u64::from(shard_count.get());
    // index < shard_count <= u16::MAX なので切り捨ては起きない。
    ShardId(index as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u32) -> BallotId {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&i.to_be_bytes());
        BallotId::from_random_bytes(b)
    }

    #[test]
    fn always_in_range_and_deterministic() {
        for count in [1u16, 2, 4, 7, 100] {
            let count = NonZeroU16::new(count).expect("non-zero");
            for i in 0..500 {
                let s = shard_for(&id(i), count);
                assert!(s.0 < count.get());
                assert_eq!(s, shard_for(&id(i), count));
            }
        }
    }

    #[test]
    fn slots_map_deterministically_into_range_and_spread() {
        let count = NonZeroU16::new(4).expect("non-zero");
        let mut hits = [0usize; 4];
        for i in 0..4_000u32 {
            let mut slot = [0u8; 32];
            slot[..4].copy_from_slice(&i.to_be_bytes());
            let s = shard_for_slot(&Slot(slot), count);
            assert_eq!(s, shard_for_slot(&Slot(slot), count));
            hits[usize::from(s.0)] += 1;
        }
        assert!(hits.iter().all(|&h| (800..=1_200).contains(&h)), "{hits:?}");
    }

    #[test]
    fn single_shard_maps_everything_to_zero() {
        let one = NonZeroU16::MIN;
        assert!((0..100).all(|i| shard_for(&id(i), one) == ShardId(0)));
    }

    #[test]
    fn spreads_roughly_evenly() {
        let count = NonZeroU16::new(4).expect("non-zero");
        let mut hits = [0usize; 4];
        for i in 0..4_000 {
            hits[usize::from(shard_for(&id(i), count).0)] += 1;
        }
        // 期待値 1000。極端な偏りがないこと（統計的に十分緩い範囲）。
        assert!(hits.iter().all(|&h| (800..=1_200).contains(&h)), "{hits:?}");
    }
}
