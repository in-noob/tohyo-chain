//! ドメイン型と CQL の行の相互変換（DB に依存しない純粋な関数）。

use application::StoreError;
use domain::encoding::{decode_revote, encode_revote};
use domain::{Anchor, Ballot, BallotId, Block, BlockHeader, CandidateId, ContestId, ShardHead};

/// `ballots` 列の 1 要素: (ballot_id, contest_id, candidate_id, revote)。ID は文字列（`domain::ids`）。
/// `revote` は再投票のつながり（slot・seq・supersedes）の正規化バイト列（`domain::encoding::encode_revote`）。
/// 再投票を認めない選挙の票では空（`0x`。何も記録しない。ADR 0022）。NULL にしないのは、cqlsh が tuple の中の NULL の blob を
/// 表示できない（運用者がブロックを確認できなくなる）ため。読むときは、空も NULL も「つながり無し」。
pub type BallotTuple = (Vec<u8>, String, String, Option<Vec<u8>>);

/// `blocks` テーブルの 1 行（`SELECT` の列順）。
pub type BlockRow = (
    i64,                      // height
    i32,                      // format_version
    Vec<u8>,                  // prev_hash
    Vec<u8>,                  // merkle_root
    i32,                      // ballot_count
    i64,                      // sealed_at_minute
    Vec<u8>,                  // election_hash
    Vec<u8>,                  // block_hash
    Vec<u8>,                  // signature
    Option<Vec<BallotTuple>>, // ballots（空リストは NULL で返る）
);

/// UNIX 秒を、票の受理時刻として保存する「分」と、書き込み時刻（マイクロ秒）に丸める。
/// 書き込み時刻は分の先頭（60 秒の倍数）にする。
pub fn minute_bucket(unix_secs: u64) -> Result<(i64, i64), StoreError> {
    let minute = i64::try_from(unix_secs / 60).map_err(|_| StoreError::Corrupt)?;
    let micros = minute
        .checked_mul(60 * 1_000_000)
        .ok_or(StoreError::Corrupt)?;
    Ok((minute, micros))
}

/// プールの行を削除するときの書き込み時刻（マイクロ秒）。その行の INSERT の書き込み時刻
/// （受理した分の先頭）より必ず 1 だけ新しい値にする。
///
/// 削除の書き込み時刻が INSERT より小さいと、削除は黙って無効になる（票がプールに残る）。
/// 現在時刻を使うと、INSERT した api と削除する sealer の時計がずれたときに起き得るので、
/// 行の `received_minute` から決める。
pub fn pool_delete_timestamp(received_minute: i64) -> Result<i64, StoreError> {
    received_minute
        .checked_mul(60 * 1_000_000)
        .and_then(|micros| micros.checked_add(1))
        .ok_or(StoreError::Corrupt)
}

pub fn ballot_to_tuple(ballot: &Ballot) -> Result<BallotTuple, StoreError> {
    Ok((
        ballot.ballot_id.0.to_vec(),
        ballot.contest_id.as_str().to_string(),
        ballot.candidate_id.as_str().to_string(),
        Some(encode_revote(ballot.revote.as_ref())),
    ))
}

/// 票の再投票のつながりを、DB の `ballot_pool.revote` 列（blob。無ければ NULL）の値にする。
pub fn revote_to_blob(ballot: &Ballot) -> Option<Vec<u8>> {
    ballot.revote.as_ref().map(|link| encode_revote(Some(link)))
}

pub fn ballot_from_tuple(
    (id, contest, candidate, revote): &BallotTuple,
) -> Result<Ballot, StoreError> {
    Ok(Ballot {
        ballot_id: BallotId(id.as_slice().try_into().map_err(|_| StoreError::Corrupt)?),
        contest_id: ContestId::parse(contest).map_err(|_| StoreError::Corrupt)?,
        candidate_id: CandidateId::parse(candidate).map_err(|_| StoreError::Corrupt)?,
        revote: match revote {
            Some(bytes) => decode_revote(bytes).map_err(|_| StoreError::Corrupt)?,
            None => None,
        },
    })
}

/// `anchors.heads` の 1 要素: (shard, height, block_hash)。
pub type HeadTuple = (i32, i64, Vec<u8>);

/// `anchors` テーブルの 1 行（`SELECT` の列順）。
pub type AnchorRow = (
    i64,                    // seq
    i64,                    // anchor_minute
    Vec<u8>,                // prev_anchor_hash
    Option<Vec<HeadTuple>>, // heads
    Vec<u8>,                // anchor_hash
    Vec<u8>,                // signature
);

pub fn head_to_tuple(head: &ShardHead) -> HeadTuple {
    (
        i32::from(head.shard),
        // 高さは i64 に収まる範囲（超える場合は `anchor_to_values` が弾く）。
        i64::try_from(head.height).unwrap_or(i64::MAX),
        head.block_hash.to_vec(),
    )
}

pub struct AnchorValues {
    pub seq: i64,
    pub anchor_minute: i64,
    pub prev_anchor_hash: Vec<u8>,
    pub heads: Vec<HeadTuple>,
    pub anchor_hash: Vec<u8>,
    pub signature: Vec<u8>,
}

pub fn anchor_to_values(anchor: &Anchor) -> Result<AnchorValues, StoreError> {
    if anchor
        .heads
        .iter()
        .any(|h| i64::try_from(h.height).is_err())
    {
        return Err(StoreError::Corrupt);
    }
    Ok(AnchorValues {
        seq: i64::try_from(anchor.seq).map_err(|_| StoreError::Corrupt)?,
        anchor_minute: i64::try_from(anchor.anchor_minute).map_err(|_| StoreError::Corrupt)?,
        prev_anchor_hash: anchor.prev_anchor_hash.to_vec(),
        heads: anchor.heads.iter().map(head_to_tuple).collect(),
        anchor_hash: anchor.anchor_hash.to_vec(),
        signature: anchor.signature.to_vec(),
    })
}

pub fn anchor_from_row(row: AnchorRow) -> Result<Anchor, StoreError> {
    let (seq, minute, prev, heads, hash, signature) = row;
    let heads = heads
        .unwrap_or_default()
        .into_iter()
        .map(|(shard, height, block_hash)| {
            Ok(ShardHead {
                shard: u16::try_from(shard).map_err(|_| StoreError::Corrupt)?,
                height: u64::try_from(height).map_err(|_| StoreError::Corrupt)?,
                block_hash: block_hash
                    .as_slice()
                    .try_into()
                    .map_err(|_| StoreError::Corrupt)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(Anchor {
        seq: u64::try_from(seq).map_err(|_| StoreError::Corrupt)?,
        anchor_minute: u64::try_from(minute).map_err(|_| StoreError::Corrupt)?,
        prev_anchor_hash: prev
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Corrupt)?,
        heads,
        anchor_hash: hash
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Corrupt)?,
        signature: signature
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Corrupt)?,
    })
}

/// 書き込み用のブロックの列（`INSERT` のバインド順は store.rs の文に合わせる）。
pub struct BlockValues {
    pub height: i64,
    pub format_version: i32,
    pub prev_hash: Vec<u8>,
    pub merkle_root: Vec<u8>,
    pub ballot_count: i32,
    pub sealed_at_minute: i64,
    pub election_hash: Vec<u8>,
    pub block_hash: Vec<u8>,
    pub signature: Vec<u8>,
    pub ballots: Vec<BallotTuple>,
}

pub fn block_to_values(block: &Block) -> Result<BlockValues, StoreError> {
    let h = &block.header;
    Ok(BlockValues {
        height: i64::try_from(h.height).map_err(|_| StoreError::Corrupt)?,
        format_version: i32::from(h.version),
        prev_hash: h.prev_hash.to_vec(),
        merkle_root: h.merkle_root.to_vec(),
        ballot_count: i32::try_from(h.ballot_count).map_err(|_| StoreError::Corrupt)?,
        sealed_at_minute: i64::try_from(h.sealed_at_minute).map_err(|_| StoreError::Corrupt)?,
        election_hash: h.election_hash.to_vec(),
        block_hash: block.block_hash.to_vec(),
        signature: block.signature.to_vec(),
        ballots: block
            .ballots
            .iter()
            .map(ballot_to_tuple)
            .collect::<Result<_, _>>()?,
    })
}

pub fn block_from_row(row: BlockRow) -> Result<Block, StoreError> {
    let (height, version, prev, root, count, minute, election, hash, signature, ballots) = row;
    let bad = |_| StoreError::Corrupt;
    Ok(Block {
        header: BlockHeader {
            version: u16::try_from(version).map_err(bad)?,
            height: u64::try_from(height).map_err(bad)?,
            prev_hash: prev
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::Corrupt)?,
            merkle_root: root
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::Corrupt)?,
            ballot_count: u32::try_from(count).map_err(bad)?,
            sealed_at_minute: u64::try_from(minute).map_err(bad)?,
            election_hash: election
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::Corrupt)?,
        },
        ballots: ballots
            .unwrap_or_default()
            .iter()
            .map(ballot_from_tuple)
            .collect::<Result<_, _>>()?,
        block_hash: hash
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Corrupt)?,
        signature: signature
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Corrupt)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{Ed25519Signer, genesis, seal_block};

    fn ballot(n: u8) -> Ballot {
        Ballot {
            ballot_id: BallotId::from_random_bytes([n; 16]),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.03").expect("valid"),
            candidate_id: CandidateId::parse(&format!("shugiin_smd.13.03.c{}", 300 + u32::from(n)))
                .expect("valid"),
            revote: None,
        }
    }

    /// 書き込み用の値から、読み出し用の行の形に組み直す。
    fn as_row(v: BlockValues) -> BlockRow {
        let ballots = (!v.ballots.is_empty()).then_some(v.ballots);
        (
            v.height,
            v.format_version,
            v.prev_hash,
            v.merkle_root,
            v.ballot_count,
            v.sealed_at_minute,
            v.election_hash,
            v.block_hash,
            v.signature,
            ballots,
        )
    }

    #[test]
    fn minute_bucket_rounds_down_to_the_minute() {
        // 書き込み時刻（マイクロ秒）は分の先頭 = 60 秒の倍数。
        for secs in [0, 59, 60, 61, 1_800_000_000, 1_800_000_059] {
            let (minute, micros) = minute_bucket(secs).expect("in range");
            assert_eq!(minute, i64::try_from(secs / 60).expect("fits"));
            assert_eq!(micros % 60_000_000, 0);
            assert!(micros <= i64::try_from(secs).expect("fits") * 1_000_000);
        }
        assert_eq!(minute_bucket(119).expect("ok"), (1, 60_000_000));
        // 同じ分の中の時刻は、書き込み時刻が同一になる（マイクロ秒の差が残らない）。
        assert_eq!(minute_bucket(1_800_000_001), minute_bucket(1_800_000_058));
    }

    #[test]
    fn delete_timestamp_is_always_newer_than_the_insert_of_the_same_row() {
        for secs in [0u64, 59, 60, 1_800_000_000, 1_800_000_059] {
            let (minute, insert_ts) = minute_bucket(secs).expect("in range");
            let delete_ts = pool_delete_timestamp(minute).expect("in range");
            assert_eq!(delete_ts, insert_ts + 1);
        }
        assert_eq!(pool_delete_timestamp(i64::MAX), Err(StoreError::Corrupt));
    }

    #[test]
    fn minute_bucket_rejects_out_of_range() {
        assert_eq!(minute_bucket(u64::MAX), Err(StoreError::Corrupt));
    }

    /// 再投票のつながりを持つ票。
    fn linked(n: u8) -> Ballot {
        Ballot {
            revote: Some(domain::RevoteLink {
                slot: domain::Slot([n; 32]),
                seq: 2,
                supersedes: Some([n; 32]),
            }),
            ..ballot(n)
        }
    }

    #[test]
    fn ballot_roundtrip() {
        let b = ballot(7);
        let tuple = ballot_to_tuple(&b).expect("ok");
        // 再投票のつながりの無い票は、revote が空（slot を記録しない）。
        assert_eq!(tuple.3, Some(Vec::new()));
        assert_eq!(ballot_from_tuple(&tuple), Ok(b));
        let l = linked(8);
        let tuple = ballot_to_tuple(&l).expect("ok");
        assert!(tuple.3.is_some());
        assert_eq!(ballot_from_tuple(&tuple), Ok(l));
    }

    #[test]
    fn ballot_conversion_rejects_bad_data() {
        let (id, contest, candidate, _) = ballot_to_tuple(&ballot(1)).expect("ok");
        // ballot_id の長さが違う。
        assert_eq!(
            ballot_from_tuple(&(vec![1, 2, 3], contest.clone(), candidate.clone(), None)),
            Err(StoreError::Corrupt)
        );
        // ID の形式が不正（DB のデータが壊れている）。
        assert_eq!(
            ballot_from_tuple(&(id.clone(), "1".to_string(), candidate.clone(), None)),
            Err(StoreError::Corrupt)
        );
        assert_eq!(
            ballot_from_tuple(&(
                id.clone(),
                contest.clone(),
                "Bad Candidate".to_string(),
                None
            )),
            Err(StoreError::Corrupt)
        );
        // 再投票のつながりのバイト列が壊れている。
        assert_eq!(
            ballot_from_tuple(&(id, contest, candidate, Some(vec![0x09]))),
            Err(StoreError::Corrupt)
        );
    }

    #[test]
    fn block_roundtrip_including_genesis_with_null_ballots() {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let g = genesis(&signer, 100, [0xe1; 32]);
        let b1 = seal_block(&g, vec![ballot(1), ballot(2), linked(3)], 101, &signer).expect("seal");
        for block in [g, b1] {
            let row = as_row(block_to_values(&block).expect("to values"));
            assert_eq!(block_from_row(row), Ok(block));
        }
    }

    #[test]
    fn anchor_roundtrip_and_corrupt_rows() {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let anchor = domain::build_anchor(
            3,
            30_000_000,
            [7; 32],
            vec![
                ShardHead {
                    shard: 1,
                    height: 9,
                    block_hash: [2; 32],
                },
                ShardHead {
                    shard: 0,
                    height: 4,
                    block_hash: [1; 32],
                },
            ],
            &signer,
        )
        .expect("anchor");
        let v = anchor_to_values(&anchor).expect("values");
        let row: AnchorRow = (
            v.seq,
            v.anchor_minute,
            v.prev_anchor_hash,
            Some(v.heads),
            v.anchor_hash,
            v.signature,
        );
        assert_eq!(anchor_from_row(row.clone()), Ok(anchor));

        let mut bad = row.clone();
        bad.4.pop();
        assert_eq!(anchor_from_row(bad), Err(StoreError::Corrupt));
        let mut bad = row;
        bad.0 = -1;
        assert_eq!(anchor_from_row(bad), Err(StoreError::Corrupt));
    }

    #[test]
    fn block_row_with_wrong_lengths_is_corrupt() {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let g = genesis(&signer, 100, [0xe1; 32]);
        let good = as_row(block_to_values(&g).expect("to values"));

        let mut bad_hash = good.clone();
        bad_hash.6.pop();
        assert_eq!(block_from_row(bad_hash), Err(StoreError::Corrupt));

        let mut bad_sig = good.clone();
        bad_sig.7.push(0);
        assert_eq!(block_from_row(bad_sig), Err(StoreError::Corrupt));

        let mut negative_height = good;
        negative_height.0 = -1;
        assert_eq!(block_from_row(negative_height), Err(StoreError::Corrupt));
    }
}
