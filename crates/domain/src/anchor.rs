//! アンカー: 全シャードの head（最新ブロック）をまとめて署名した、チェーン全体のスナップショット。
//!
//! 各シャードのチェーンは独立しているので、単独では「他のシャードがどうなっているか」「過去のブロックが
//! 巻き戻されていないか」を示せない。アンカーは全シャードの head を 1 つのハッシュに束ね、直前のアンカーの
//! ハッシュも含める（アンカー自身も鎖になる）。外部（ログ・公開 API）に出しておけば、後から
//! 「この時点でこの状態だった」を示せる。
//!
//! ハッシュ対象は固定長ビッグエンディアンで手組みする（serde / JSON は使わない。原則4）:
//! `SHA256("vote/anchor/v1" ‖ seq(8) ‖ minute(8) ‖ prev_anchor_hash(32) ‖ head_count(4)
//!          ‖ { shard(2) ‖ height(8) ‖ block_hash(32) } × head_count)`（head は shard の昇順）

use crate::encoding::sha256_parts;
use crate::signature::{Signer, Verifier};
use crate::types::{Hash32, SignatureBytes};

pub const ANCHOR_DOMAIN: &[u8] = b"vote/anchor/v1";

/// 最初のアンカーの `prev_anchor_hash`。
pub const GENESIS_ANCHOR_PREV: Hash32 = [0u8; 32];

/// あるシャードの head。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardHead {
    pub shard: u16,
    pub height: u64,
    pub block_hash: Hash32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// 1 から始まる通し番号。
    pub seq: u64,
    /// 作成時刻（UNIX 分。原則3）。
    pub anchor_minute: u64,
    pub prev_anchor_hash: Hash32,
    /// shard の昇順。
    pub heads: Vec<ShardHead>,
    pub anchor_hash: Hash32,
    /// `anchor_hash` への Ed25519 署名。
    pub signature: SignatureBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnchorError {
    #[error("head が空です")]
    NoHeads,
    #[error("head のシャードが昇順・一意ではありません")]
    HeadsNotSorted,
    #[error("head の件数が上限（u32）を超えています")]
    TooManyHeads,
    #[error("anchor_hash が内容から再計算した値と一致しません")]
    HashMismatch,
    #[error("署名が不正です")]
    SignatureInvalid,
    #[error("prev_anchor_hash が直前のアンカーの anchor_hash と一致しません")]
    PrevHashMismatch,
    #[error("seq が直前のアンカーの次ではありません")]
    SeqMismatch,
}

/// `heads` は shard の昇順・一意であること。
pub fn compute_anchor_hash(
    seq: u64,
    anchor_minute: u64,
    prev_anchor_hash: &Hash32,
    heads: &[ShardHead],
) -> Result<Hash32, AnchorError> {
    let count = u32::try_from(heads.len()).map_err(|_| AnchorError::TooManyHeads)?;
    let mut body = Vec::with_capacity(8 + 8 + 32 + 4 + heads.len() * 42);
    body.extend_from_slice(&seq.to_be_bytes());
    body.extend_from_slice(&anchor_minute.to_be_bytes());
    body.extend_from_slice(prev_anchor_hash);
    body.extend_from_slice(&count.to_be_bytes());
    for head in heads {
        body.extend_from_slice(&head.shard.to_be_bytes());
        body.extend_from_slice(&head.height.to_be_bytes());
        body.extend_from_slice(&head.block_hash);
    }
    Ok(sha256_parts(&[ANCHOR_DOMAIN, &body]))
}

fn heads_are_strictly_ascending(heads: &[ShardHead]) -> bool {
    heads.windows(2).all(|w| w[0].shard < w[1].shard)
}

/// アンカーを組み立てて署名する。`heads` は shard の昇順に並べ替える。
pub fn build_anchor(
    seq: u64,
    anchor_minute: u64,
    prev_anchor_hash: Hash32,
    mut heads: Vec<ShardHead>,
    signer: &dyn Signer,
) -> Result<Anchor, AnchorError> {
    if heads.is_empty() {
        return Err(AnchorError::NoHeads);
    }
    heads.sort_by_key(|h| h.shard);
    if !heads_are_strictly_ascending(&heads) {
        return Err(AnchorError::HeadsNotSorted);
    }
    let anchor_hash = compute_anchor_hash(seq, anchor_minute, &prev_anchor_hash, &heads)?;
    let signature = signer.sign(&anchor_hash);
    Ok(Anchor {
        seq,
        anchor_minute,
        prev_anchor_hash,
        heads,
        anchor_hash,
        signature,
    })
}

/// アンカー単体の検証（内容とハッシュの一致、署名）。
pub fn verify_anchor(anchor: &Anchor, verifier: &dyn Verifier) -> Result<(), AnchorError> {
    if anchor.heads.is_empty() {
        return Err(AnchorError::NoHeads);
    }
    if !heads_are_strictly_ascending(&anchor.heads) {
        return Err(AnchorError::HeadsNotSorted);
    }
    let recomputed = compute_anchor_hash(
        anchor.seq,
        anchor.anchor_minute,
        &anchor.prev_anchor_hash,
        &anchor.heads,
    )?;
    if anchor.anchor_hash != recomputed {
        return Err(AnchorError::HashMismatch);
    }
    verifier
        .verify(&recomputed, &anchor.signature)
        .map_err(|_| AnchorError::SignatureInvalid)
}

/// `next` が `prev` の直後のアンカーであること（番号とハッシュの連結）。
pub fn verify_anchor_link(prev: &Anchor, next: &Anchor) -> Result<(), AnchorError> {
    if prev.seq.checked_add(1) != Some(next.seq) {
        return Err(AnchorError::SeqMismatch);
    }
    if next.prev_anchor_hash != prev.anchor_hash {
        return Err(AnchorError::PrevHashMismatch);
    }
    Ok(())
}

/// 直前のアンカーから見た、現在の各シャードの head の変化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadsChange {
    /// どのシャードの先頭ブロックも変わっていない。アンカーを作る必要はない（データに更新がない）。
    Unchanged,
    /// いずれかのシャードの高さが進んだ（残りは同じ）。アンカーで新しい状態を固定する。
    Advanced,
    /// 直前のアンカーと矛盾している（高さが戻った、同じ高さでハッシュが違う、シャードの構成が違う）。
    /// チェーンの巻き戻しや分岐の疑いがあるので、新しいアンカーで上書きしてはならない。
    Inconsistent(&'static str),
}

/// 現在の `heads`（shard の昇順）を、直前のアンカー `prev` と比べる。
///
/// `prev` が無いとき（まだ 1 つもアンカーが無いとき）の基準は、ジェネシスだけの状態（全シャードの高さ 0）。
/// つまり、どのシャードにもブロックが追加されていなければ `Unchanged`（ジェネシスだけの状態は「更新なし」）。
pub fn compare_heads(prev: Option<&Anchor>, heads: &[ShardHead]) -> HeadsChange {
    let Some(prev) = prev else {
        return if heads.iter().any(|h| h.height > 0) {
            HeadsChange::Advanced
        } else {
            HeadsChange::Unchanged
        };
    };
    if prev.heads.len() != heads.len()
        || prev
            .heads
            .iter()
            .zip(heads)
            .any(|(before, now)| before.shard != now.shard)
    {
        return HeadsChange::Inconsistent("シャードの構成が直前のアンカーと違います");
    }
    let mut advanced = false;
    for (before, now) in prev.heads.iter().zip(heads) {
        match now.height.cmp(&before.height) {
            std::cmp::Ordering::Less => {
                return HeadsChange::Inconsistent("シャードの高さが直前のアンカーより戻っています");
            }
            std::cmp::Ordering::Equal if now.block_hash != before.block_hash => {
                return HeadsChange::Inconsistent(
                    "同じ高さのブロックのハッシュが直前のアンカーと違います",
                );
            }
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => advanced = true,
        }
    }
    if advanced {
        HeadsChange::Advanced
    } else {
        HeadsChange::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::Ed25519Signer;

    fn head(shard: u16, height: u64, fill: u8) -> ShardHead {
        ShardHead {
            shard,
            height,
            block_hash: [fill; 32],
        }
    }

    fn signer() -> Ed25519Signer {
        Ed25519Signer::from_seed(&[11u8; 32])
    }

    fn sample(signer: &Ed25519Signer) -> Anchor {
        build_anchor(
            1,
            30_000_000,
            GENESIS_ANCHOR_PREV,
            vec![head(2, 5, 0x22), head(0, 3, 0x00), head(1, 4, 0x11)],
            signer,
        )
        .expect("build")
    }

    #[test]
    fn builds_sorted_and_verifies() {
        let signer = signer();
        let anchor = sample(&signer);
        let shards: Vec<u16> = anchor.heads.iter().map(|h| h.shard).collect();
        assert_eq!(shards, vec![0, 1, 2]);
        assert_eq!(verify_anchor(&anchor, &signer.verifier()), Ok(()));
    }

    #[test]
    fn hash_encoding_is_fixed_big_endian_known_vector() {
        // 独立実装（Python hashlib）で計算した期待値:
        // sha256(b"vote/anchor/v1" + seq(8) + minute(8) + prev(32) + count(4) + [shard(2)+height(8)+hash(32)])
        let heads = [head(0, 0x0102, 0xaa), head(1, 0x0304, 0xbb)];
        let hash = compute_anchor_hash(0x0a0b, 0x0c0d, &[0x33; 32], &heads).expect("hash");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "e17ba066e14a815caecf1b61e76a6abe1073c5573d8d72b7a1ef644e3fc396a0"
        );
    }

    #[test]
    fn any_change_to_content_is_detected() {
        let signer = signer();
        let verifier = signer.verifier();
        let original = sample(&signer);

        let mut a = original.clone();
        a.heads[1].height += 1;
        assert_eq!(verify_anchor(&a, &verifier), Err(AnchorError::HashMismatch));

        let mut a = original.clone();
        a.heads[0].block_hash[0] ^= 1;
        assert_eq!(verify_anchor(&a, &verifier), Err(AnchorError::HashMismatch));

        let mut a = original.clone();
        a.anchor_minute += 1;
        assert_eq!(verify_anchor(&a, &verifier), Err(AnchorError::HashMismatch));

        let mut a = original.clone();
        a.prev_anchor_hash[0] ^= 1;
        assert_eq!(verify_anchor(&a, &verifier), Err(AnchorError::HashMismatch));

        // ハッシュごと作り直しても、署名鍵を持たない者には署名できない。
        let mut a = original.clone();
        a.heads[1].height += 1;
        a.anchor_hash = compute_anchor_hash(a.seq, a.anchor_minute, &a.prev_anchor_hash, &a.heads)
            .expect("hash");
        assert_eq!(
            verify_anchor(&a, &verifier),
            Err(AnchorError::SignatureInvalid)
        );

        let mut a = original;
        a.signature[3] ^= 1;
        assert_eq!(
            verify_anchor(&a, &verifier),
            Err(AnchorError::SignatureInvalid)
        );
    }

    #[test]
    fn rejects_other_signers_key() {
        let signer = signer();
        let other = Ed25519Signer::from_seed(&[12u8; 32]).verifier();
        assert_eq!(
            verify_anchor(&sample(&signer), &other),
            Err(AnchorError::SignatureInvalid)
        );
    }

    #[test]
    fn rejects_empty_and_duplicate_heads() {
        let signer = signer();
        assert_eq!(
            build_anchor(1, 1, GENESIS_ANCHOR_PREV, vec![], &signer),
            Err(AnchorError::NoHeads)
        );
        assert_eq!(
            build_anchor(
                1,
                1,
                GENESIS_ANCHOR_PREV,
                vec![head(1, 1, 1), head(1, 2, 2)],
                &signer
            ),
            Err(AnchorError::HeadsNotSorted)
        );
        // 検証側も、昇順でない head を拒否する。
        let mut a = sample(&signer);
        a.heads.swap(0, 1);
        assert_eq!(
            verify_anchor(&a, &signer.verifier()),
            Err(AnchorError::HeadsNotSorted)
        );
    }

    #[test]
    fn anchors_form_a_chain() {
        let signer = signer();
        let first = sample(&signer);
        let second = build_anchor(
            2,
            30_000_010,
            first.anchor_hash,
            vec![head(0, 4, 1), head(1, 5, 2), head(2, 6, 3)],
            &signer,
        )
        .expect("build");
        assert_eq!(verify_anchor_link(&first, &second), Ok(()));
        // 順序が逆・番号の飛び・ハッシュの不一致。
        assert_eq!(
            verify_anchor_link(&second, &first),
            Err(AnchorError::SeqMismatch)
        );
        let mut skipped = second.clone();
        skipped.seq = 3;
        assert_eq!(
            verify_anchor_link(&first, &skipped),
            Err(AnchorError::SeqMismatch)
        );
        let mut relinked = second;
        relinked.prev_anchor_hash = [9; 32];
        assert_eq!(
            verify_anchor_link(&first, &relinked),
            Err(AnchorError::PrevHashMismatch)
        );
    }

    fn anchor_with(heads: Vec<ShardHead>) -> Anchor {
        build_anchor(1, 100, GENESIS_ANCHOR_PREV, heads, &signer()).expect("valid anchor")
    }

    #[test]
    fn without_a_previous_anchor_genesis_only_is_unchanged() {
        assert_eq!(
            compare_heads(None, &[head(0, 0, 1), head(1, 0, 2)]),
            HeadsChange::Unchanged
        );
        assert_eq!(
            compare_heads(None, &[head(0, 0, 1), head(1, 1, 2)]),
            HeadsChange::Advanced
        );
    }

    #[test]
    fn identical_heads_are_unchanged_and_any_higher_shard_is_advanced() {
        let prev = anchor_with(vec![head(0, 3, 1), head(1, 5, 2)]);
        assert_eq!(
            compare_heads(Some(&prev), &[head(0, 3, 1), head(1, 5, 2)]),
            HeadsChange::Unchanged
        );
        assert_eq!(
            compare_heads(Some(&prev), &[head(0, 3, 1), head(1, 6, 9)]),
            HeadsChange::Advanced
        );
        assert_eq!(
            compare_heads(Some(&prev), &[head(0, 4, 9), head(1, 6, 9)]),
            HeadsChange::Advanced
        );
    }

    #[test]
    fn contradicting_heads_are_inconsistent_never_advanced() {
        let prev = anchor_with(vec![head(0, 3, 1), head(1, 5, 2)]);
        // 高さが戻った（巻き戻し）。他のシャードが進んでいても、矛盾を優先して報告する。
        assert!(matches!(
            compare_heads(Some(&prev), &[head(0, 2, 1), head(1, 9, 2)]),
            HeadsChange::Inconsistent(_)
        ));
        // 同じ高さでハッシュが違う（分岐）。
        assert!(matches!(
            compare_heads(Some(&prev), &[head(0, 3, 7), head(1, 5, 2)]),
            HeadsChange::Inconsistent(_)
        ));
        // シャードの構成が違う。
        assert!(matches!(
            compare_heads(Some(&prev), &[head(0, 3, 1)]),
            HeadsChange::Inconsistent(_)
        ));
        assert!(matches!(
            compare_heads(Some(&prev), &[head(0, 3, 1), head(2, 5, 2)]),
            HeadsChange::Inconsistent(_)
        ));
    }
}
