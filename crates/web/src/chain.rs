//! ブロックチェーンのビューア（`/chain` 以下）の表示ロジック。UI（Leptos / ブラウザ API）から切り離した**純粋関数**だけを置く
//! （通常の `cargo test` で検証できる）。ログイン不要の公開データだけを扱い、投票者を特定する情報は出てこない。

use shared_types::{AnchorDto, BallotDto, BlockDto, HeaderDto};

use crate::error::ApiFailure;

/// ビューアのパス。
pub const CHAIN_HOME: &str = "/chain";
pub const ANCHORS_PATH: &str = "/chain/anchors";

pub fn shard_path(shard: u16) -> String {
    format!("{CHAIN_HOME}/{shard}")
}

pub fn block_path(shard: u16, height: u64) -> String {
    format!("{CHAIN_HOME}/{shard}/blocks/{height}")
}

/// 前のブロックの詳細のパス。ジェネシス（高さ 0）には前がない。
pub fn prev_block_path(shard: u16, height: u64) -> Option<String> {
    height.checked_sub(1).map(|prev| block_path(shard, prev))
}

/// ジェネシス（高さ 0）か。
pub fn is_genesis(header: &HeaderDto) -> bool {
    header.height == 0
}

/// パスの `:shard` / `:height` を読む。数値でなければ `None`（画面は「見つかりません」にする）。
pub fn parse_shard(text: &str) -> Option<u16> {
    text.parse().ok()
}

pub fn parse_height(text: &str) -> Option<u64> {
    text.parse().ok()
}

/// 長いハッシュ（hex）を、先頭と末尾だけに縮める（全体は、`title` などで見られるようにする）。
pub fn short_hash(hex: &str) -> String {
    const KEEP: usize = 8;
    if hex.chars().count() <= KEEP * 2 + 1 {
        return hex.to_string();
    }
    let head: String = hex.chars().take(KEEP).collect();
    let tail: String = hex.chars().skip(hex.chars().count() - KEEP).collect();
    format!("{head}…{tail}")
}

/// 「このチェーンを検証するには」に表示するコマンド。`origin` は、この画面を配信しているオリジン
/// （api も同じオリジンから見える）。`public_key` があれば、API が返した鍵を信じずに固定するオプションを添える。
pub fn verify_command(origin: &str, public_key: Option<&str>) -> String {
    let base = format!(
        "cargo run -q -p verifier -- verify --api {}",
        origin.trim_end_matches('/')
    );
    match public_key {
        Some(key) => format!("{base} --public-key {key}"),
        None => base,
    }
}

/// 票の一覧の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BallotRow {
    /// 1 から始まる、ブロック内の並び（ballot_id のハッシュ順）。
    pub index: usize,
    pub ballot_id: String,
    /// 選挙区の表示名。API が知らなければ、投票用紙の ID。
    pub district: String,
    /// 候補者の表示名（政党つき）。API が知らなければ、候補者の ID。
    pub candidate: String,
}

pub fn ballot_rows(ballots: &[BallotDto]) -> Vec<BallotRow> {
    ballots
        .iter()
        .enumerate()
        .map(|(i, b)| BallotRow {
            index: i + 1,
            ballot_id: b.ballot_id.clone(),
            district: b
                .district_name
                .clone()
                .unwrap_or_else(|| b.contest_id.clone()),
            candidate: match (&b.candidate_name, &b.party) {
                (Some(name), Some(party)) if !party.is_empty() => format!("{name}（{party}）"),
                (Some(name), _) => name.clone(),
                (None, _) => b.candidate_id.clone(),
            },
        })
        .collect()
}

/// 票の一覧の代わりに出す案内（締切前など、API が票を返さないとき）。票を返しているときは `None`。
pub fn hidden_ballots_notice(block: &BlockDto) -> Option<String> {
    (!block.ballots_revealed).then(|| {
        format!(
            "票の中身（{} 票）は、投票の締切後に公開されます。ヘッダーの情報だけを表示しています。",
            block.header.ballot_count
        )
    })
}

/// アンカーが指している、各シャードの先頭ブロックへのリンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadLink {
    pub label: String,
    pub path: String,
    pub block_hash: String,
}

pub fn anchor_head_links(anchor: &AnchorDto) -> Vec<HeadLink> {
    anchor
        .heads
        .iter()
        .map(|h| HeadLink {
            label: format!("シャード {} の高さ {}", h.shard, h.height),
            path: block_path(h.shard, h.height),
            block_hash: h.block_hash.clone(),
        })
        .collect()
}

/// 取得に失敗したときの案内。
pub fn failure_message(failure: ApiFailure) -> &'static str {
    match failure {
        ApiFailure::NotFound => "見つかりません。",
        ApiFailure::Network => {
            "通信に失敗しました。ネットワークを確認して、もう一度お試しください。"
        }
        ApiFailure::Unavailable => {
            "サービスが利用できません。しばらくしてからもう一度お試しください。"
        }
        _ => "取得できませんでした。",
    }
}

#[cfg(test)]
mod tests {
    use shared_types::HeadRefDto;

    use super::*;

    fn header(height: u64, count: u32) -> HeaderDto {
        HeaderDto {
            version: 2,
            height,
            prev_hash: "00".repeat(32),
            merkle_root: "11".repeat(32),
            ballot_count: count,
            sealed_at_minute: 0,
        }
    }

    fn ballot(with_names: bool) -> BallotDto {
        BallotDto {
            ballot_id: "aa".repeat(16),
            contest_id: "2026-general/smd.13.01".to_string(),
            candidate_id: "smd.13.01.c1".to_string(),
            district_name: with_names.then(|| "東京1区".to_string()),
            candidate_name: with_names.then(|| "甲".to_string()),
            party: with_names.then(|| "党".to_string()),
        }
    }

    #[test]
    fn paths_follow_the_viewer_routes() {
        assert_eq!(CHAIN_HOME, "/chain");
        assert_eq!(shard_path(3), "/chain/3");
        assert_eq!(block_path(3, 12), "/chain/3/blocks/12");
        assert_eq!(ANCHORS_PATH, "/chain/anchors");
    }

    #[test]
    fn the_previous_block_link_stops_at_the_genesis() {
        assert_eq!(prev_block_path(0, 5), Some("/chain/0/blocks/4".to_string()));
        assert_eq!(prev_block_path(0, 1), Some("/chain/0/blocks/0".to_string()));
        assert_eq!(prev_block_path(0, 0), None);
        assert!(is_genesis(&header(0, 0)));
        assert!(!is_genesis(&header(1, 0)));
    }

    #[test]
    fn path_parameters_must_be_numbers() {
        assert_eq!(parse_shard("0"), Some(0));
        assert_eq!(parse_shard("65535"), Some(65535));
        assert_eq!(parse_shard("65536"), None);
        assert_eq!(parse_shard("-1"), None);
        assert_eq!(parse_shard("abc"), None);
        assert_eq!(parse_height("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_height(""), None);
    }

    #[test]
    fn long_hashes_are_shortened_but_short_ones_are_kept() {
        let full = "0123456789abcdef".repeat(4);
        assert_eq!(short_hash(&full), "01234567…89abcdef");
        assert_eq!(short_hash("abcd"), "abcd");
        let boundary = "x".repeat(17);
        assert_eq!(short_hash(&boundary), boundary);
        assert_ne!(short_hash(&"x".repeat(18)), "x".repeat(18));
    }

    #[test]
    fn the_verify_command_points_at_this_origin_and_can_pin_the_key() {
        assert_eq!(
            verify_command("https://vote.example", None),
            "cargo run -q -p verifier -- verify --api https://vote.example"
        );
        assert_eq!(
            verify_command("http://localhost:8080/", Some("ab12")),
            "cargo run -q -p verifier -- verify --api http://localhost:8080 --public-key ab12"
        );
    }

    #[test]
    fn ballot_rows_use_display_names_and_fall_back_to_ids() {
        let rows = ballot_rows(&[ballot(true), ballot(false)]);
        assert_eq!(rows[0].index, 1);
        assert_eq!(rows[0].district, "東京1区");
        assert_eq!(rows[0].candidate, "甲（党）");
        assert_eq!(rows[1].index, 2);
        assert_eq!(rows[1].district, "2026-general/smd.13.01");
        assert_eq!(rows[1].candidate, "smd.13.01.c1");
        // 政党が空なら、氏名だけ。
        let mut b = ballot(true);
        b.party = Some(String::new());
        assert_eq!(ballot_rows(&[b])[0].candidate, "甲");
        assert!(ballot_rows(&[]).is_empty());
    }

    #[test]
    fn a_notice_replaces_the_ballot_list_only_while_they_are_hidden() {
        let mut block = BlockDto {
            header: header(3, 42),
            ballots: Vec::new(),
            block_hash: String::new(),
            signature: String::new(),
            ballots_revealed: false,
            signer_public_key: None,
        };
        let notice = hidden_ballots_notice(&block).expect("hidden");
        assert!(
            notice.contains("42 票") && notice.contains("締切後"),
            "{notice}"
        );
        block.ballots_revealed = true;
        assert_eq!(hidden_ballots_notice(&block), None);
    }

    #[test]
    fn anchors_link_to_the_head_block_of_each_shard() {
        let anchor = AnchorDto {
            seq: 2,
            anchor_minute: 0,
            prev_anchor_hash: String::new(),
            heads: vec![
                HeadRefDto {
                    shard: 0,
                    height: 7,
                    block_hash: "aa".to_string(),
                },
                HeadRefDto {
                    shard: 1,
                    height: 0,
                    block_hash: "bb".to_string(),
                },
            ],
            anchor_hash: String::new(),
            signature: String::new(),
        };
        let links = anchor_head_links(&anchor);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].path, "/chain/0/blocks/7");
        assert_eq!(links[0].label, "シャード 0 の高さ 7");
        assert_eq!(links[1].path, "/chain/1/blocks/0");
    }

    #[test]
    fn failures_have_readable_messages() {
        assert!(failure_message(ApiFailure::NotFound).contains("見つかりません"));
        assert!(failure_message(ApiFailure::Network).contains("通信"));
        assert!(failure_message(ApiFailure::Unavailable).contains("利用できません"));
        assert!(!failure_message(ApiFailure::Unexpected(418)).is_empty());
    }
}
