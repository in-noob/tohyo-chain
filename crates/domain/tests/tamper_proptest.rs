//! 性質テスト: チェーン中の「任意の 1 票」「任意の 1 バイト」を改ざんすると、
//! `verify_chain` が必ず失敗する。公開 API のみを使う。

use std::collections::BTreeMap;

use domain::encoding::{HEADER_LEN, decode_ballot, decode_header, encode_ballot, encode_header};
use domain::{
    Ballot, BallotId, Block, CandidateId, ContestId, DistrictId, Ed25519Signer, ElectionId,
    RevoteLink, Slot, genesis, seal_block, verify_chain,
};
use proptest::prelude::*;

const HASH_LEN: usize = 32;
const SIG_LEN: usize = 64;

/// ブロックごとの票（ballot_id → candidate_id。ブロック内で ID は一意）。
fn arb_groups() -> impl Strategy<Value = Vec<BTreeMap<[u8; 16], u32>>> {
    prop::collection::vec(
        prop::collection::btree_map(any::<[u8; 16]>(), any::<u32>(), 1..8),
        1..5,
    )
}

/// 票の `contest_id` / `candidate_id` は文字列の ID（可変長）。
fn sample_ids(cand: u32) -> (ContestId, CandidateId) {
    let district = DistrictId::new(&format!("shugiin_smd.13.0{}", 1 + cand % 3)).expect("valid");
    let election = ElectionId::new("2026-general").expect("valid");
    (
        ContestId::new(&election, &district),
        CandidateId::new(&district, u64::from(cand) + 1).expect("valid"),
    )
}

fn build_chain(seed: &[u8; 32], groups: &[BTreeMap<[u8; 16], u32>]) -> (Vec<Block>, Ed25519Signer) {
    let signer = Ed25519Signer::from_seed(seed);
    let mut chain = vec![genesis(&signer, 1_000)];
    for (i, group) in groups.iter().enumerate() {
        let ballots: Vec<Ballot> = group
            .iter()
            .map(|(id, cand)| {
                let (contest_id, candidate_id) = sample_ids(*cand);
                // 一部の票に、再投票のつながり（版 3）を付ける。改ざんは、つながりのバイトも対象になる。
                let revote = (cand % 2 == 0).then(|| {
                    let seq = cand % 4 + 1;
                    RevoteLink {
                        slot: Slot([id[0]; 32]),
                        seq,
                        supersedes: (seq > 1).then_some([id[1]; 32]),
                    }
                });
                Ballot {
                    ballot_id: BallotId(*id),
                    contest_id,
                    candidate_id,
                    revote,
                }
            })
            .collect();
        let prev = &chain[chain.len() - 1];
        let block = seal_block(prev, ballots, 1_001 + i as u64, &signer).expect("seal");
        chain.push(block);
    }
    (chain, signer)
}

/// 票の正規化バイト列の長さ（可変長）。
fn ballot_len(ballot: &Ballot) -> usize {
    encode_ballot(ballot).len()
}

/// 1 ブロックが持つ「改ざん対象バイト」の総数。
fn block_bytes(block: &Block) -> usize {
    block.ballots.iter().map(ballot_len).sum::<usize>() + HEADER_LEN + HASH_LEN + SIG_LEN
}

/// 票の正規化バイト列の 1 バイトを XOR する。改ざん後のバイト列が、票として解釈できない
/// （長さ・ID の文字種が不正）ときは `false`（そのような票はチェーンに戻せない = 検証以前に弾かれる）。
fn flip_ballot_byte(ballot: &mut Ballot, offset: usize, mask: u8) -> bool {
    let mut bytes = encode_ballot(ballot);
    bytes[offset] ^= mask;
    match decode_ballot(&bytes) {
        Ok(tampered) => {
            *ballot = tampered;
            true
        }
        Err(_) => false,
    }
}

/// チェーン全体を通したバイト位置 `pos` を、`mask` で XOR して必ず 1 バイト変える。
/// 票として解釈できない変更（`flip_ballot_byte` を参照）のときは、チェーンを変えずに `false`。
fn tamper_at(chain: &mut [Block], mut pos: usize, mask: u8) -> bool {
    for block in chain.iter_mut() {
        let size = block_bytes(block);
        if pos >= size {
            pos -= size;
            continue;
        }
        for ballot in &mut block.ballots {
            let len = ballot_len(ballot);
            if pos < len {
                return flip_ballot_byte(ballot, pos, mask);
            }
            pos -= len;
        }
        if pos < HEADER_LEN {
            let mut bytes = encode_header(&block.header);
            bytes[pos] ^= mask;
            block.header = decode_header(&bytes);
            return true;
        }
        pos -= HEADER_LEN;
        if pos < HASH_LEN {
            block.block_hash[pos] ^= mask;
            return true;
        }
        pos -= HASH_LEN;
        block.signature[pos] ^= mask;
        return true;
    }
    unreachable!("pos は総バイト数の範囲内");
}

proptest! {
    /// 任意の 1 バイト（票・ヘッダ・block_hash・署名のどれか）を改ざんすると失敗する。
    #[test]
    fn any_single_byte_tamper_is_detected(
        seed in any::<[u8; 32]>(),
        groups in arb_groups(),
        pos in any::<prop::sample::Index>(),
        mask in 1u8..=255,
    ) {
        let (mut chain, signer) = build_chain(&seed, &groups);
        let verifier = signer.verifier();
        prop_assert_eq!(verify_chain(&chain, &verifier), Ok(()));

        let total: usize = chain.iter().map(block_bytes).sum();
        let representable = tamper_at(&mut chain, pos.index(total), mask);
        prop_assert!(!representable || verify_chain(&chain, &verifier).is_err());
    }

    /// 任意の 1 票の任意の 1 バイトを改ざんすると失敗する。
    #[test]
    fn any_single_ballot_byte_tamper_is_detected(
        seed in any::<[u8; 32]>(),
        groups in arb_groups(),
        which in any::<prop::sample::Index>(),
        offset in any::<prop::sample::Index>(),
        mask in 1u8..=255,
    ) {
        let (mut chain, signer) = build_chain(&seed, &groups);
        let verifier = signer.verifier();

        let total: usize = chain.iter().map(|b| b.ballots.len()).sum();
        let mut n = which.index(total);
        let mut representable = true;
        for block in chain.iter_mut() {
            if n < block.ballots.len() {
                let ballot = &mut block.ballots[n];
                representable = flip_ballot_byte(ballot, offset.index(ballot_len(ballot)), mask);
                break;
            }
            n -= block.ballots.len();
        }
        prop_assert!(!representable || verify_chain(&chain, &verifier).is_err());
    }
}
