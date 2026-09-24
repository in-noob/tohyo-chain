//! 正規化バイナリエンコード。
//!
//! ハッシュ対象はすべて、ビッグエンディアンの固定幅のフィールドで手組みする。
//! serde / JSON はハッシュ対象の生成に使わない（原則4）。
//!
//! - 票   = `ballot_id(16) ‖ len(contest_id)(2) ‖ contest_id ‖ len(candidate_id)(2) ‖ candidate_id`
//!   （ID は文字列 [`crate::ids`] の ASCII バイト列。長さは 2 バイトの固定幅・ビッグエンディアンで、
//!   先頭に置くので、フィールドの境界が曖昧にならない）。最大 197 バイト。ブロックの形式の版 2 から。
//! - ヘッダ = `version(2) ‖ height(8) ‖ prev_hash(32) ‖ merkle_root(32)
//!            ‖ ballot_count(4) ‖ sealed_at_minute(8)`                  = 86 バイト（固定長）
//! - `block_hash = SHA256("vote/block/v1" ‖ ヘッダ)`

use sha2::{Digest, Sha256};

use crate::ids::{CANDIDATE_ID_MAX_LEN, CONTEST_ID_MAX_LEN, CandidateId, ContestId, IdError};
use crate::types::{BALLOT_ID_LEN, Ballot, BallotId, BlockHeader, HASH_LEN, Hash32};

/// 票の正規化バイト列の最大長。
pub const MAX_BALLOT_LEN: usize = BALLOT_ID_LEN + 2 + CONTEST_ID_MAX_LEN + 2 + CANDIDATE_ID_MAX_LEN;
pub const HEADER_LEN: usize = 86;

/// ブロックハッシュのドメイン分離タグ。
pub const BLOCK_HASH_DOMAIN: &[u8] = b"vote/block/v1";
/// 票の並び順キーのドメイン分離タグ。
pub const BALLOT_ORDER_DOMAIN: &[u8] = b"vote/ballot-order/v1";

const OFF_VERSION: usize = 0;
const OFF_HEIGHT: usize = 2;
const OFF_PREV_HASH: usize = 10;
const OFF_MERKLE_ROOT: usize = OFF_PREV_HASH + HASH_LEN;
const OFF_COUNT: usize = OFF_MERKLE_ROOT + HASH_LEN;
const OFF_MINUTE: usize = OFF_COUNT + 4;

/// 複数のバイト列を連結して SHA-256 を取る。
pub fn sha256_parts(parts: &[&[u8]]) -> Hash32 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// 票の正規化バイト列: `ballot_id(16) ‖ len(2) ‖ contest_id ‖ len(2) ‖ candidate_id`。
pub fn encode_ballot(ballot: &Ballot) -> Vec<u8> {
    let contest = ballot.contest_id.as_str().as_bytes();
    let candidate = ballot.candidate_id.as_str().as_bytes();
    let mut out = Vec::with_capacity(BALLOT_ID_LEN + 4 + contest.len() + candidate.len());
    out.extend_from_slice(&ballot.ballot_id.0);
    // ID の最大長は検証済み（97・80 バイト）なので、`u16` に収まる。
    out.extend_from_slice(&(contest.len() as u16).to_be_bytes());
    out.extend_from_slice(contest);
    out.extend_from_slice(&(candidate.len() as u16).to_be_bytes());
    out.extend_from_slice(candidate);
    out
}

/// 票の正規化バイト列の解釈の失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BallotDecodeError {
    #[error("票のバイト列が途中で終わっています")]
    Truncated,
    #[error("票のバイト列の後ろに余計なバイトがあります")]
    TrailingBytes,
    #[error("票の ID が不正です: {0}")]
    InvalidId(#[from] IdError),
}

/// [`encode_ballot`] の逆。ID の検証（[`crate::ids`]）も行う。
pub fn decode_ballot(bytes: &[u8]) -> Result<Ballot, BallotDecodeError> {
    let mut rest = bytes;
    let mut ballot_id = [0u8; BALLOT_ID_LEN];
    ballot_id.copy_from_slice(take_slice(&mut rest, BALLOT_ID_LEN)?);
    let contest = take_string(&mut rest)?;
    let candidate = take_string(&mut rest)?;
    if !rest.is_empty() {
        return Err(BallotDecodeError::TrailingBytes);
    }
    Ok(Ballot {
        ballot_id: BallotId(ballot_id),
        contest_id: ContestId::parse(&contest)?,
        candidate_id: CandidateId::parse(&candidate)?,
    })
}

/// 先頭から `n` バイトを切り出す（`rest` は残りに進める）。
fn take_slice<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], BallotDecodeError> {
    if rest.len() < n {
        return Err(BallotDecodeError::Truncated);
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

/// 2 バイトの長さ接頭辞つきの文字列を読む。ASCII でなければ、不正な ID として扱う。
fn take_string(rest: &mut &[u8]) -> Result<String, BallotDecodeError> {
    let len = take_slice(rest, 2)?;
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    let bytes = take_slice(rest, len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        BallotDecodeError::InvalidId(IdError::Malformed {
            kind: "ballot",
            reason: "ID が UTF-8 ではありません",
        })
    })
}

pub fn encode_header(header: &BlockHeader) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    put(&mut out, OFF_VERSION, &header.version.to_be_bytes());
    put(&mut out, OFF_HEIGHT, &header.height.to_be_bytes());
    put(&mut out, OFF_PREV_HASH, &header.prev_hash);
    put(&mut out, OFF_MERKLE_ROOT, &header.merkle_root);
    put(&mut out, OFF_COUNT, &header.ballot_count.to_be_bytes());
    put(&mut out, OFF_MINUTE, &header.sealed_at_minute.to_be_bytes());
    out
}

pub fn decode_header(bytes: &[u8; HEADER_LEN]) -> BlockHeader {
    BlockHeader {
        version: u16::from_be_bytes(take(bytes, OFF_VERSION)),
        height: u64::from_be_bytes(take(bytes, OFF_HEIGHT)),
        prev_hash: take(bytes, OFF_PREV_HASH),
        merkle_root: take(bytes, OFF_MERKLE_ROOT),
        ballot_count: u32::from_be_bytes(take(bytes, OFF_COUNT)),
        sealed_at_minute: u64::from_be_bytes(take(bytes, OFF_MINUTE)),
    }
}

/// `SHA256("vote/block/v1" ‖ encode_header(header))`。
pub fn block_hash(header: &BlockHeader) -> Hash32 {
    sha256_parts(&[BLOCK_HASH_DOMAIN, &encode_header(header)])
}

/// ブロック内の票の並び順キー。`ballot_id` のハッシュ昇順に並べる（原則3）。
/// 到着順と並びが相関しないため、到着順の漏洩を防げる。
pub fn ballot_order_key(id: &BallotId) -> Hash32 {
    sha256_parts(&[BALLOT_ORDER_DOMAIN, &id.0])
}

fn put(out: &mut [u8; HEADER_LEN], offset: usize, src: &[u8]) {
    out[offset..offset + src.len()].copy_from_slice(src);
}

fn take<const N: usize, const M: usize>(src: &[u8; M], offset: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&src[offset..offset + N]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> BlockHeader {
        BlockHeader {
            version: 0x0102,
            height: 0x0304_0506_0708_090a,
            prev_hash: [0xaa; 32],
            merkle_root: [0xbb; 32],
            ballot_count: 0x0b0c_0d0e,
            sealed_at_minute: 0x0f10_1112_1314_1516,
        }
    }

    fn sample_ballot() -> Ballot {
        Ballot {
            ballot_id: BallotId([0x11; 16]),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
            candidate_id: CandidateId::parse("shugiin_smd.13.01.c3").expect("valid"),
        }
    }

    #[test]
    fn ballot_encoding_is_length_prefixed_big_endian() {
        let ballot = sample_ballot();
        let mut expected = vec![0x11u8; 16];
        expected.extend_from_slice(&[0x00, 30]); // contest_id は 30 バイト
        expected.extend_from_slice(b"2026-general/shugiin_smd.13.01");
        expected.extend_from_slice(&[0x00, 20]); // candidate_id は 20 バイト
        expected.extend_from_slice(b"shugiin_smd.13.01.c3");
        assert_eq!(encode_ballot(&ballot), expected);
        assert_eq!(decode_ballot(&encode_ballot(&ballot)), Ok(ballot));
    }

    #[test]
    fn ballot_decoding_rejects_malformed_bytes() {
        let bytes = encode_ballot(&sample_ballot());
        // 途中で切れている（どの位置で切っても失敗）。
        for cut in 0..bytes.len() {
            assert!(decode_ballot(&bytes[..cut]).is_err(), "cut={cut}");
        }
        // 後ろに余計なバイト。
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(decode_ballot(&long), Err(BallotDecodeError::TrailingBytes));
        // ID の文字種が不正（大文字）。
        let mut bad = bytes.clone();
        bad[18] = b'X';
        assert!(matches!(
            decode_ballot(&bad),
            Err(BallotDecodeError::InvalidId(_))
        ));
    }

    #[test]
    fn maximum_ballot_length_matches_the_id_limits() {
        assert_eq!(MAX_BALLOT_LEN, 197);
    }

    #[test]
    fn header_encoding_is_fixed_big_endian() {
        let encoded = encode_header(&sample_header());
        let mut expected = vec![0x01, 0x02];
        expected.extend_from_slice(&[3, 4, 5, 6, 7, 8, 9, 10]);
        expected.extend_from_slice(&[0xaa; 32]);
        expected.extend_from_slice(&[0xbb; 32]);
        expected.extend_from_slice(&[0x0b, 0x0c, 0x0d, 0x0e]);
        expected.extend_from_slice(&[0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16]);
        assert_eq!(encoded.len(), HEADER_LEN);
        assert_eq!(encoded.to_vec(), expected);
        assert_eq!(decode_header(&encoded), sample_header());
    }

    #[test]
    fn block_hash_known_vector() {
        // 独立実装（Python hashlib）で計算した期待値:
        // sha256(b"vote/block/v1" + encode_header(sample_header()))
        let expected = "7dc101f7a01ce00e288c069bb402098f4d36b42d7e2e1a6929e341c971817c4e";
        let actual: String = block_hash(&sample_header())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn block_hash_changes_with_any_header_field() {
        let base = sample_header();
        let h0 = block_hash(&base);
        let variants = [
            BlockHeader { version: 3, ..base },
            BlockHeader { height: 1, ..base },
            BlockHeader {
                prev_hash: [0; 32],
                ..base
            },
            BlockHeader {
                merkle_root: [0; 32],
                ..base
            },
            BlockHeader {
                ballot_count: 1,
                ..base
            },
            BlockHeader {
                sealed_at_minute: 1,
                ..base
            },
        ];
        for v in variants {
            assert_ne!(block_hash(&v), h0);
        }
    }
}
