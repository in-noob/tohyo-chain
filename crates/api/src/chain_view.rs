//! ブロックチェーンのビューア向け API の、純粋な部分（公開の判定・ページ送り・キャッシュの判定・DTO の組み立て）。
//!
//! 封印済みのブロックは内容が変わらないので、確定した応答には `Cache-Control: public, max-age=31536000, immutable` を付ける
//! （CDN でキャッシュできる）。ただし、**内容がこの先変わる応答には付けない**: `reveal_ballots=after_close` の締切前の詳細は
//! 票が伏せられていて、締切後に中身が変わるので、`immutable` にすると、票なしの版が CDN に 1 年残ってしまう。

use std::collections::{HashMap, HashSet};

use application::{ChainRead, StoreError};
use domain::election::Election;
use domain::{Ballot, Block, Hash32, ShardId, ballot_hash};
use serde::{Deserialize, Deserializer};
use shared_types::hex;
use shared_types::{BallotDto, BlockDto, BlockSummaryDto, HeaderDto, ReplacedBallotDto};

/// 再投票の票が置き換えた前の版（ブロックの高さと、その票）。キーは前の版の票のハッシュ（= 次の版の `supersedes`）。
pub type Replaced = HashMap<Hash32, (u64, Ballot)>;

/// 前の版を探すときに、1 回に読むブロックの数。
const REPLACED_SCAN_PAGE: usize = 100;

/// `block` の票が置き換えた前の版を、同じシャードのチェーンから探す（ビューアのリンク用。検証には使わない）。
///
/// 同じ slot の全版は同じシャードにあり、前の版は次の版と同じか、より低い高さに封印される（プールは到着順に封印するため。
/// ADR 0022）ので、`block` の高さから下へ、見つかるまで読む。
pub async fn find_replaced(
    chains: &dyn ChainRead,
    shard: ShardId,
    block: &Block,
) -> Result<Replaced, StoreError> {
    let mut wanted: HashSet<Hash32> = block
        .ballots
        .iter()
        .filter_map(|b| b.revote.and_then(|link| link.supersedes))
        .collect();
    let mut found = Replaced::new();
    let mut before = block.header.height.checked_add(1);
    while !wanted.is_empty() {
        let page = chains
            .blocks_before(shard, before, REPLACED_SCAN_PAGE)
            .await?;
        let Some(lowest) = page.last().map(|b| b.header.height) else {
            break;
        };
        for candidate in &page {
            for ballot in &candidate.ballots {
                let hash = ballot_hash(ballot);
                if wanted.remove(&hash) {
                    found.insert(hash, (candidate.header.height, ballot.clone()));
                }
            }
        }
        if lowest == 0 {
            break;
        }
        before = Some(lowest);
    }
    Ok(found)
}

/// 確定した応答（内容が二度と変わらない）。
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// 保存させない（締切前の詳細・エラー）。
pub const NO_STORE: &str = "no-store";
/// 使うたびに確認させる（先頭が変わる一覧）。
pub const REVALIDATE: &str = "no-cache";

/// ページの件数の既定と上限。
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

/// 票の中身を公開するタイミング（設定の `chain.reveal_ballots`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealPolicy {
    /// 常に公開する。
    Always,
    /// 締切（UNIX 秒）以後だけ公開する。締切前は、ヘッダーの情報だけを返す。
    AfterClose { closes_at_unix: i64 },
}

impl RevealPolicy {
    /// `now_unix` の時点で、票の中身を公開してよいか。締切ちょうどから公開する。
    pub fn is_revealed(self, now_unix: u64) -> bool {
        match self {
            Self::Always => true,
            // 符号つき 64 ビットに収まらない時刻でも、比較が狂わないよう、広い型で比べる。
            Self::AfterClose { closes_at_unix } => {
                i128::from(now_unix) >= i128::from(closes_at_unix)
            }
        }
    }
}

/// `limit` を、1 以上 [`MAX_LIMIT`] 以下にそろえる（未指定なら [`DEFAULT_LIMIT`]）。
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// ブロックの詳細の `Cache-Control`。票を返す（確定した）応答だけが `immutable`。
pub fn block_detail_cache(revealed: bool) -> &'static str {
    if revealed { IMMUTABLE } else { NO_STORE }
}

/// ブロックの一覧の `Cache-Control`。`before_height` が先頭の高さ + 1 以下なら、返すブロックはすべて封印済みで、
/// 同じ URL の応答は変わらない（`immutable`）。`before_height` が無い、または先頭より先を指すページは、チェーンが伸びると
/// 中身が変わるので、確認させる。
pub fn blocks_page_cache(before_height: Option<u64>, head_height: Option<u64>) -> &'static str {
    match (before_height, head_height) {
        (Some(before), Some(head)) if head.checked_add(1).is_some_and(|top| before <= top) => {
            IMMUTABLE
        }
        _ => REVALIDATE,
    }
}

/// 次のページの `before_height`: いま返した最も低い高さ（それより古いブロックがあるとき）。
pub fn next_before_height(blocks: &[BlockSummaryDto]) -> Option<u64> {
    blocks
        .last()
        .map(|b| b.header.height)
        .filter(|&height| height > 0)
}

/// クエリの空文字（`?before_height=&limit=`）を「指定なし」として読む。
pub fn empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(text) => text.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

fn header_dto(block: &Block) -> HeaderDto {
    let h = &block.header;
    HeaderDto {
        version: h.version,
        height: h.height,
        prev_hash: hex::encode(&h.prev_hash),
        merkle_root: hex::encode(&h.merkle_root),
        ballot_count: h.ballot_count,
        sealed_at_minute: h.sealed_at_minute,
    }
}

/// 一覧用の要約（票は含まない）。
pub fn block_summary(block: &Block) -> BlockSummaryDto {
    BlockSummaryDto {
        header: header_dto(block),
        block_hash: hex::encode(&block.block_hash),
        signature: hex::encode(&block.signature),
    }
}

/// 詳細。`revealed` が偽なら、票（`ballot_id`・`contest_id`・`candidate_id`・再投票の slot / seq / supersedes）を
/// 返さない。表示名（選挙区・候補者・政党）は、選挙データにあれば付ける。`replaced` は、再投票の票が置き換えた
/// 前の版（[`find_replaced`]）。
pub fn block_detail(
    block: &Block,
    revealed: bool,
    election: &Election,
    signer_public_key: Option<&[u8; 32]>,
    replaced: &Replaced,
) -> BlockDto {
    let ballots = if revealed {
        block
            .ballots
            .iter()
            .map(|b| {
                let contest = election.contest(&b.contest_id);
                let name_of = |id: &domain::CandidateId| {
                    id.candidate().and_then(|code| {
                        contest.and_then(|c| c.candidates.iter().find(|c| &c.id == code))
                    })
                };
                // 白票は候補者ではないので、候補者の表示名を付けない（`blank` で示す）。
                let candidate = name_of(&b.candidate_id);
                let link = b.revote;
                let replaces = link
                    .and_then(|l| l.supersedes)
                    .and_then(|prev| replaced.get(&prev))
                    .map(|(height, prev)| ReplacedBallotDto {
                        ballot_id: hex::encode(&prev.ballot_id.0),
                        height: *height,
                        candidate_id: prev.candidate_id.to_string(),
                        blank: prev.candidate_id.is_blank(),
                        candidate_name: name_of(&prev.candidate_id).map(|c| c.name.clone()),
                    });
                BallotDto {
                    ballot_id: hex::encode(&b.ballot_id.0),
                    contest_id: b.contest_id.to_string(),
                    candidate_id: b.candidate_id.to_string(),
                    blank: b.candidate_id.is_blank(),
                    district_name: contest.map(|c| c.district.name.clone()),
                    candidate_name: candidate.map(|c| c.name.clone()),
                    party: candidate.map(|c| c.party.clone()),
                    slot: link.map(|l| hex::encode(&l.slot.0)),
                    seq: link.map(|l| l.seq),
                    supersedes: link.and_then(|l| l.supersedes).map(|h| hex::encode(&h)),
                    replaces,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    BlockDto {
        header: header_dto(block),
        ballots,
        block_hash: hex::encode(&block.block_hash),
        signature: hex::encode(&block.signature),
        ballots_revealed: revealed,
        signer_public_key: signer_public_key.map(|k| hex::encode(k)),
    }
}

#[cfg(test)]
mod tests {
    use domain::election::{Candidate, District, ElectionType, VotingMethod};
    use domain::{
        Ballot, BallotId, CandidateCode, CandidateId, ContestId, DistrictId, Ed25519Signer,
        ElectionId, ElectionTypeCode, genesis, seal_block,
    };

    use super::*;

    #[test]
    fn always_reveals_and_after_close_reveals_from_the_close_on() {
        assert!(RevealPolicy::Always.is_revealed(0));
        let policy = RevealPolicy::AfterClose {
            closes_at_unix: 1_000,
        };
        assert!(!policy.is_revealed(0));
        assert!(!policy.is_revealed(999));
        assert!(policy.is_revealed(1_000));
        assert!(policy.is_revealed(u64::MAX));
    }

    #[test]
    fn limit_is_clamped_to_one_through_the_maximum() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(7)), 7);
        assert_eq!(clamp_limit(Some(100)), 100);
        assert_eq!(clamp_limit(Some(101)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(usize::MAX)), MAX_LIMIT);
    }

    #[test]
    fn only_finished_responses_are_immutable() {
        // 票を返す詳細は確定。伏せた詳細は、締切後に変わるので、保存させない。
        assert_eq!(block_detail_cache(true), IMMUTABLE);
        assert_eq!(block_detail_cache(false), NO_STORE);
        assert_eq!(IMMUTABLE, "public, max-age=31536000, immutable");
        // 一覧: before_height が先頭 + 1 以下のときだけ確定（先頭が 5 なら、before_height=6 まで）。
        assert_eq!(blocks_page_cache(Some(6), Some(5)), IMMUTABLE);
        assert_eq!(blocks_page_cache(Some(1), Some(5)), IMMUTABLE);
        assert_eq!(blocks_page_cache(Some(0), Some(5)), IMMUTABLE);
        assert_eq!(blocks_page_cache(Some(7), Some(5)), REVALIDATE);
        assert_eq!(blocks_page_cache(None, Some(5)), REVALIDATE);
        assert_eq!(blocks_page_cache(Some(3), None), REVALIDATE);
        assert_eq!(
            blocks_page_cache(Some(u64::MAX), Some(u64::MAX)),
            REVALIDATE
        );
    }

    #[derive(Debug, Deserialize)]
    struct Q {
        #[serde(default, deserialize_with = "empty_as_none")]
        before_height: Option<u64>,
        #[serde(default, deserialize_with = "empty_as_none")]
        limit: Option<usize>,
    }

    fn query(text: &str) -> Result<Q, axum::extract::rejection::QueryRejection> {
        let uri: axum::http::Uri = format!("/x?{text}").parse().expect("uri");
        axum::extract::Query::<Q>::try_from_uri(&uri).map(|q| q.0)
    }

    #[test]
    fn empty_query_values_mean_not_specified() {
        let q = query("before_height=&limit=").expect("parse");
        assert_eq!((q.before_height, q.limit), (None, None));
        let q = query("").expect("parse");
        assert_eq!((q.before_height, q.limit), (None, None));
        let q = query("before_height=12&limit=5").expect("parse");
        assert_eq!((q.before_height, q.limit), (Some(12), Some(5)));
        assert!(query("limit=abc").is_err());
        assert!(query("before_height=-1").is_err());
    }

    fn election() -> Election {
        let ty = ElectionTypeCode::new("smd").expect("type");
        let district = District {
            id: DistrictId::new("smd.13.01").expect("district"),
            election_type: ty.clone(),
            name: "東京1区".to_string(),
            prefectures: vec!["13".to_string()],
            order: 1,
        };
        let candidates = vec![Candidate {
            id: CandidateCode::parse("smd.13.01.c1").expect("candidate"),
            name: "甲".to_string(),
            party: "党".to_string(),
            profile: String::new(),
        }];
        Election::new(
            ElectionId::new("e1").expect("election"),
            "選挙".to_string(),
            vec![ElectionType {
                code: ty,
                name: "小選挙区".to_string(),
                order: 1,
                method: VotingMethod::SingleChoice,
            }],
            vec![district],
            candidates,
        )
        .expect("election")
    }

    fn block_with(contest: &str, candidate: &str) -> Block {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let genesis = genesis(&signer, 10);
        let ballot = Ballot {
            ballot_id: BallotId::from_random_bytes([9; 16]),
            contest_id: ContestId::parse(contest).expect("contest"),
            candidate_id: CandidateId::parse(candidate).expect("candidate"),
            revote: None,
        };
        seal_block(&genesis, vec![ballot], 11, &signer).expect("seal")
    }

    #[test]
    fn a_hidden_detail_has_no_ballots_but_keeps_the_header_and_count() {
        let block = block_with("e1/smd.13.01", "smd.13.01.c1");
        let hidden = block_detail(&block, false, &election(), None, &Replaced::new());
        assert!(!hidden.ballots_revealed);
        assert!(hidden.ballots.is_empty());
        assert_eq!(hidden.header.ballot_count, 1);
        assert_eq!(hidden.block_hash, hex::encode(&block.block_hash));
        // JSON にも、票の中身（ID）は現れない。
        let json = serde_json::to_string(&hidden).expect("json");
        assert!(
            !json.contains("candidate_id") && !json.contains("smd.13.01.c1"),
            "{json}"
        );
        assert!(!json.contains("ballot_id"), "{json}");
    }

    #[test]
    fn a_revealed_detail_lists_ballots_with_display_names() {
        let block = block_with("e1/smd.13.01", "smd.13.01.c1");
        let key = [4u8; 32];
        let shown = block_detail(&block, true, &election(), Some(&key), &Replaced::new());
        assert!(shown.ballots_revealed);
        let b = &shown.ballots[0];
        assert_eq!(b.candidate_id, "smd.13.01.c1");
        assert_eq!(
            (
                b.district_name.as_deref(),
                b.candidate_name.as_deref(),
                b.party.as_deref()
            ),
            (Some("東京1区"), Some("甲"), Some("党"))
        );
        assert_eq!(shown.signer_public_key, Some(hex::encode(&key)));
        assert!(!b.blank);
        // 候補者の票の JSON には、blank を出さない（false は省く）。
        assert!(!serde_json::to_string(b).expect("json").contains("blank"));
    }

    #[test]
    fn the_reserved_blank_value_is_the_same_for_web_and_domain() {
        // web は domain に依存しないので、shared_types に同じ値を置いている（原則5）。食い違うと白票が通らない。
        assert_eq!(shared_types::BLANK_CANDIDATE_ID, domain::BLANK_CANDIDATE_ID);
        assert_eq!(
            CandidateId::Blank.as_str(),
            shared_types::BLANK_CANDIDATE_ID
        );
    }

    #[test]
    fn a_blank_ballot_is_marked_blank_and_has_no_candidate_names() {
        let block = block_with("e1/smd.13.01", "blank");
        let shown = block_detail(&block, true, &election(), None, &Replaced::new());
        let b = &shown.ballots[0];
        assert!(b.blank);
        assert_eq!(b.candidate_id, "blank");
        assert_eq!(b.district_name.as_deref(), Some("東京1区"));
        assert_eq!((&b.candidate_name, &b.party), (&None, &None));
        // 伏せた詳細には、白票かどうかも現れない。
        let hidden = block_detail(&block, false, &election(), None, &Replaced::new());
        let json = serde_json::to_string(&hidden).expect("json");
        assert!(!json.contains("blank"), "{json}");
    }

    #[test]
    fn unknown_contests_and_candidates_have_no_display_names() {
        let block = block_with("e1/smd.99.99", "smd.99.99.c1");
        let shown = block_detail(&block, true, &election(), None, &Replaced::new());
        let b = &shown.ballots[0];
        assert_eq!(
            (&b.district_name, &b.candidate_name, &b.party),
            (&None, &None, &None)
        );
        assert_eq!(shown.signer_public_key, None);
    }

    /// メモリ上のブロック列（添字 = 高さ）だけを返す、読み取り専用のチェーン。
    struct FixedChain(Vec<Block>);

    #[async_trait::async_trait]
    impl ChainRead for FixedChain {
        async fn head(&self, _: ShardId) -> Result<Option<Block>, StoreError> {
            Ok(self.0.last().cloned())
        }
        async fn block(&self, _: ShardId, height: u64) -> Result<Option<Block>, StoreError> {
            Ok(usize::try_from(height)
                .ok()
                .and_then(|h| self.0.get(h))
                .cloned())
        }
        async fn latest_anchor(&self) -> Result<Option<domain::Anchor>, StoreError> {
            Ok(None)
        }
        async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn a_revote_links_to_the_ballot_it_replaced_only_when_revealed() {
        let signer = Ed25519Signer::from_seed(&[1u8; 32]);
        let slot = domain::Slot([3; 32]);
        let first = Ballot {
            ballot_id: BallotId::from_random_bytes([1; 16]),
            contest_id: ContestId::parse("e1/smd.13.01").expect("contest"),
            candidate_id: CandidateId::parse("smd.13.01.c1").expect("candidate"),
            revote: Some(domain::RevoteLink {
                slot,
                seq: 1,
                supersedes: None,
            }),
        };
        let second = Ballot {
            ballot_id: BallotId::from_random_bytes([2; 16]),
            candidate_id: CandidateId::Blank,
            revote: Some(domain::RevoteLink {
                slot,
                seq: 2,
                supersedes: Some(ballot_hash(&first)),
            }),
            ..first.clone()
        };
        let g = genesis(&signer, 10);
        let b1 = seal_block(&g, vec![first.clone()], 11, &signer).expect("seal");
        let b2 = seal_block(&b1, vec![second], 12, &signer).expect("seal");
        let chain = FixedChain(vec![g, b1, b2.clone()]);
        let replaced = find_replaced(&chain, ShardId(0), &b2).await.expect("scan");
        assert_eq!(replaced.len(), 1);

        let shown = block_detail(&b2, true, &election(), None, &replaced);
        let b = &shown.ballots[0];
        assert_eq!(
            (b.seq, b.slot.as_deref()),
            (Some(2), Some(hex::encode(&[3; 32]).as_str()))
        );
        assert_eq!(b.supersedes, Some(hex::encode(&ballot_hash(&first))));
        let replaces = b.replaces.as_ref().expect("replaces");
        assert_eq!(replaces.ballot_id, hex::encode(&first.ballot_id.0));
        assert_eq!(replaces.height, 1);
        assert_eq!(replaces.candidate_name.as_deref(), Some("甲"));
        assert!(b.blank && !replaces.blank);

        // 締切前（伏せた詳細）には、candidate も supersedes も slot も現れない。
        let hidden = block_detail(&b2, false, &election(), None, &replaced);
        let json = serde_json::to_string(&hidden).expect("json");
        for word in ["supersedes", "slot", "seq", "replaces", "candidate"] {
            assert!(!json.contains(word), "{word}: {json}");
        }
    }

    #[test]
    fn the_next_page_starts_below_the_lowest_returned_height() {
        let summary = |height| {
            let mut s = block_summary(&block_with("e1/smd.13.01", "smd.13.01.c1"));
            s.header.height = height;
            s
        };
        assert_eq!(next_before_height(&[summary(9), summary(8)]), Some(8));
        // 最も古いブロック（高さ 0）まで返したら、次はない。
        assert_eq!(next_before_height(&[summary(1), summary(0)]), None);
        assert_eq!(next_before_height(&[]), None);
    }
}
