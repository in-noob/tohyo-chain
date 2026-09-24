//! Merkle 木と包含証明。
//!
//! - 葉   = `SHA256(0x00 ‖ 票)`
//! - 内部 = `SHA256(0x01 ‖ 左 ‖ 右)`
//! - 空   = `SHA256(0x02)`
//!
//! 葉と内部ノードでプレフィックスを分け（ドメイン分離）、内部ノードを葉として
//! 提示する第二原像攻撃を防ぐ。要素数が奇数のとき末尾は複製せず、そのまま
//! 上の段に持ち上げる（複製すると別の票列が同じ根になり得るため）。

use crate::encoding::{encode_ballot, sha256_parts};
use crate::types::{Ballot, Hash32};

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;
const EMPTY_PREFIX: u8 = 0x02;

/// 証明経路上の兄弟ノードが、自分から見てどちら側にあるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// 包含証明。葉から根へ向かう順の兄弟ノード列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InclusionProof {
    pub steps: Vec<(Side, Hash32)>,
}

pub fn leaf_hash(ballot: &Ballot) -> Hash32 {
    sha256_parts(&[&[LEAF_PREFIX], &encode_ballot(ballot)])
}

fn node_hash(left: &Hash32, right: &Hash32) -> Hash32 {
    sha256_parts(&[&[NODE_PREFIX], left, right])
}

/// 票が 0 件のときの根。
pub fn empty_root() -> Hash32 {
    sha256_parts(&[&[EMPTY_PREFIX]])
}

/// 1 段上のノード列を作る。奇数個の末尾はそのまま持ち上げる。
fn next_level(level: &[Hash32]) -> Vec<Hash32> {
    let (pairs, rest) = level.as_chunks::<2>();
    let mut next: Vec<Hash32> = pairs
        .iter()
        .map(|[left, right]| node_hash(left, right))
        .collect();
    next.extend_from_slice(rest);
    next
}

/// 票の並び順のまま Merkle 根を計算する。
pub fn merkle_root(ballots: &[Ballot]) -> Hash32 {
    let mut level: Vec<Hash32> = ballots.iter().map(leaf_hash).collect();
    loop {
        match level.as_slice() {
            [] => return empty_root(),
            [root] => return *root,
            _ => level = next_level(&level),
        }
    }
}

/// `index` 番目の票の包含証明を作る。範囲外なら `None`。
pub fn inclusion_proof(ballots: &[Ballot], index: usize) -> Option<InclusionProof> {
    if index >= ballots.len() {
        return None;
    }
    let mut level: Vec<Hash32> = ballots.iter().map(leaf_hash).collect();
    let mut idx = index;
    let mut steps = Vec::new();
    while level.len() > 1 {
        if idx % 2 == 1 {
            steps.push((Side::Left, *level.get(idx - 1)?));
        } else if let Some(sibling) = level.get(idx + 1) {
            steps.push((Side::Right, *sibling));
        }
        // 兄弟がない（持ち上げられる）場合は段だけ進める。
        idx /= 2;
        level = next_level(&level);
    }
    Some(InclusionProof { steps })
}

/// `ballot` が根 `root` の木に含まれることを証明で検証する。
pub fn verify_inclusion(ballot: &Ballot, proof: &InclusionProof, root: &Hash32) -> bool {
    let computed = proof
        .steps
        .iter()
        .fold(leaf_hash(ballot), |acc, (side, sibling)| match side {
            Side::Left => node_hash(sibling, &acc),
            Side::Right => node_hash(&acc, sibling),
        });
    computed == *root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CandidateId, ContestId, DistrictId, ElectionId};
    use crate::types::BallotId;

    fn ballots(n: usize) -> Vec<Ballot> {
        let district = DistrictId::new("shugiin_smd.13.01").expect("valid");
        let contest = ContestId::new(&ElectionId::new("2026-general").expect("valid"), &district);
        (0..n)
            .map(|i| Ballot {
                ballot_id: BallotId([i as u8; 16]),
                contest_id: contest.clone(),
                candidate_id: CandidateId::new(&district, i as u64 + 1).expect("valid"),
            })
            .collect()
    }

    fn hex(h: &Hash32) -> String {
        h.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn known_vectors() {
        // 独立実装（Python hashlib）で計算した期待値。
        assert_eq!(
            hex(&empty_root()),
            "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986"
        );
        // 票の形式（版 2）: sha256(0x00 ‖ ballot_id ‖ len ‖ contest_id ‖ len ‖ candidate_id)。
        let b = Ballot {
            ballot_id: BallotId([0x11; 16]),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
            candidate_id: CandidateId::parse("shugiin_smd.13.01.c3").expect("valid"),
        };
        assert_eq!(
            hex(&leaf_hash(&b)),
            "47a277def541a974bc80c91068c15aba6c42adaef77322a9454874ef38d54ea8"
        );
    }

    #[test]
    fn empty_and_single() {
        assert_eq!(merkle_root(&[]), empty_root());
        let one = ballots(1);
        assert_eq!(merkle_root(&one), leaf_hash(&one[0]));
    }

    #[test]
    fn leaf_and_node_are_domain_separated() {
        // 2 葉の根は、葉ハッシュ 2 つを連結して葉として扱った値とは異なる。
        let two = ballots(2);
        let root = merkle_root(&two);
        let concatenated =
            sha256_parts(&[&[LEAF_PREFIX], &leaf_hash(&two[0]), &leaf_hash(&two[1])]);
        assert_ne!(root, concatenated);
    }

    #[test]
    fn odd_tail_is_not_duplicated() {
        // 3 個の根は、4 個目に 3 個目を複製した根と異なる。
        let three = ballots(3);
        let mut dup = three.clone();
        dup.push(three[2].clone());
        assert_ne!(merkle_root(&three), merkle_root(&dup));
    }

    #[test]
    fn every_leaf_has_valid_proof() {
        for n in 1..=17 {
            let bs = ballots(n);
            let root = merkle_root(&bs);
            for (i, b) in bs.iter().enumerate() {
                let proof = inclusion_proof(&bs, i).expect("index in range");
                assert!(verify_inclusion(b, &proof, &root), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn proof_fails_for_other_ballot_or_root() {
        let bs = ballots(7);
        let root = merkle_root(&bs);
        let proof = inclusion_proof(&bs, 3).expect("index in range");
        // 別の票
        assert!(!verify_inclusion(&bs[4], &proof, &root));
        // 候補者を書き換えた票
        let mut forged = bs[3].clone();
        forged.candidate_id = CandidateId::parse("shugiin_smd.13.01.c99").expect("valid");
        assert!(!verify_inclusion(&forged, &proof, &root));
        // 別の根
        assert!(!verify_inclusion(&bs[3], &proof, &[0u8; 32]));
    }

    #[test]
    fn out_of_range_index_has_no_proof() {
        assert!(inclusion_proof(&[], 0).is_none());
        assert!(inclusion_proof(&ballots(3), 3).is_none());
    }
}
