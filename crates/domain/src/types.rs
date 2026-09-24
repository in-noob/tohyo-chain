//! ドメイン型。
//!
//! 秘密投票の原則により、票（[`Ballot`]）は投票者 ID を一切持たない。
//! 時刻も票には持たせず、ブロックヘッダの分単位の封印時刻だけに置く。

use crate::ids::{CandidateId, ContestId};

pub const HASH_LEN: usize = 32;
pub const BALLOT_ID_LEN: usize = 16;
pub const SIGNATURE_LEN: usize = 64;

pub type Hash32 = [u8; HASH_LEN];
pub type SignatureBytes = [u8; SIGNATURE_LEN];

/// 票 ID（UUIDv4 の 16 バイト）。時刻を含む UUIDv7 等は使わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BallotId(pub [u8; BALLOT_ID_LEN]);

impl BallotId {
    /// 乱数 16 バイトに UUIDv4 のバージョン・バリアントビットを立てて作る。
    /// 乱数の生成は呼び出し側の責務（domain は乱数に依存しない）。
    pub fn from_random_bytes(mut bytes: [u8; BALLOT_ID_LEN]) -> Self {
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(bytes)
    }

    /// UUIDv4（バージョン 4・RFC 4122 バリアント）の形式か。
    pub fn is_v4(&self) -> bool {
        self.0[6] >> 4 == 0x4 && self.0[8] >> 6 == 0b10
    }
}

/// 1 票。投票者を特定できる情報は含めない。
///
/// `contest_id` を持つので、シャード単位のチェーンからも投票用紙ごとに集計できる。
/// ID は文字列（[`crate::ids`]）なので、`Copy` ではない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ballot {
    pub ballot_id: BallotId,
    pub contest_id: ContestId,
    pub candidate_id: CandidateId,
}

/// ブロックヘッダ。ハッシュ対象は [`crate::encoding::encode_header`] の固定長バイナリ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub version: u16,
    pub height: u64,
    pub prev_hash: Hash32,
    pub merkle_root: Hash32,
    pub ballot_count: u32,
    /// 封印時刻。UNIX エポックからの経過「分」（秒以下は保存しない）。
    pub sealed_at_minute: u64,
}

/// 封印済みブロック。`ballots` は `ballot_id` のハッシュ昇順。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub header: BlockHeader,
    pub ballots: Vec<Ballot>,
    pub block_hash: Hash32,
    /// `block_hash` に対する Ed25519 署名。
    pub signature: SignatureBytes,
}

/// UNIX 秒を分単位に丸める（原則3: 時刻は分単位で保存）。
pub fn unix_minutes(unix_secs: u64) -> u64 {
    unix_secs / 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_random_bytes_sets_v4_bits() {
        for b in [[0u8; 16], [0xff; 16], [0x5a; 16]] {
            assert!(BallotId::from_random_bytes(b).is_v4());
        }
        assert!(!BallotId([0u8; 16]).is_v4());
    }

    #[test]
    fn unix_minutes_rounds_down() {
        assert_eq!(unix_minutes(0), 0);
        assert_eq!(unix_minutes(59), 0);
        assert_eq!(unix_minutes(60), 1);
        assert_eq!(unix_minutes(119), 1);
    }
}
