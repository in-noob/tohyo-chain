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
    /// 投票先の表示名: 候補者（政党つき。API が知らなければ候補者の ID）、または白票の呼び名。
    pub candidate: String,
    /// 白票か（画面は、候補者と見分けられるよう、別の書式で表示する）。
    pub blank: bool,
    /// 再投票の票なら、置き換えた前の票へのリンク（締切後だけ。API が票を返すときだけ付けるため）。
    pub replaces: Option<ReplacesLink>,
}

/// 「#<前の票> を置き換え（A→B）」のリンク（ADR 0022）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacesLink {
    pub label: String,
    /// 前の票の行へのリンク（同じシャードの、前の票があるブロックの詳細。`#ballot-<ballot_id>` の行）。
    pub path: String,
}

/// 票の行のアンカー（`id` 属性）。前の票へのリンクの飛び先。
pub fn ballot_anchor(ballot_id: &str) -> String {
    format!("ballot-{ballot_id}")
}

/// 前の票への置き換えのリンク。`B`（この票の投票先）は、政党を付けない表示名（白票は `blank_name`）。
fn replaces_link(shard: u16, b: &BallotDto, blank_name: &str) -> Option<ReplacesLink> {
    let prev = b.replaces.as_ref()?;
    let name = |blank: bool, name: &Option<String>, id: &str| {
        if blank {
            blank_name.to_string()
        } else {
            name.clone().unwrap_or_else(|| id.to_string())
        }
    };
    let from = name(prev.blank, &prev.candidate_name, &prev.candidate_id);
    let to = name(b.blank, &b.candidate_name, &b.candidate_id);
    Some(ReplacesLink {
        label: format!("#{} を置き換え（{from}→{to}）", short_hash(&prev.ballot_id)),
        path: format!(
            "{}#{}",
            block_path(shard, prev.height),
            ballot_anchor(&prev.ballot_id)
        ),
    })
}

fn district_label(b: &BallotDto) -> String {
    b.district_name
        .clone()
        .unwrap_or_else(|| b.contest_id.clone())
}

/// 票の投票先の表示名。白票は `blank_name`（設定 `labels.blank_name`）。
fn choice_label(b: &BallotDto, blank_name: &str) -> String {
    if b.blank {
        return blank_name.to_string();
    }
    match (&b.candidate_name, &b.party) {
        (Some(name), Some(party)) if !party.is_empty() => format!("{name}（{party}）"),
        (Some(name), _) => name.clone(),
        (None, _) => b.candidate_id.clone(),
    }
}

/// `shard` は、このブロックのシャード（前の票へのリンク先）。`blank_name` は、白票の呼び名（設定 `labels.blank_name`）。
pub fn ballot_rows(shard: u16, ballots: &[BallotDto], blank_name: &str) -> Vec<BallotRow> {
    ballots
        .iter()
        .enumerate()
        .map(|(i, b)| BallotRow {
            index: i + 1,
            ballot_id: b.ballot_id.clone(),
            district: district_label(b),
            candidate: choice_label(b, blank_name),
            blank: b.blank,
            replaces: replaces_link(shard, b, blank_name),
        })
        .collect()
}

/// ブロック内の票の、投票先別の件数の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceCount {
    pub district: String,
    /// 投票先の表示名（候補者、または白票の呼び名）。
    pub choice: String,
    pub count: usize,
    pub blank: bool,
}

/// ブロック内の票を、選挙区（投票用紙）ごとに、投票先別に数える。選挙区は投票用紙の ID 順、その中は候補者
/// （票の多い順、同数は表示名の順）の後に、**白票を候補者とは別の行**で置く（白票が無い選挙区には、白票の行を出さない）。
pub fn choice_counts(ballots: &[BallotDto], blank_name: &str) -> Vec<ChoiceCount> {
    use std::collections::BTreeMap;
    // 投票用紙の ID → (選挙区の表示名, 候補者の表示名 → 件数, 白票の件数)。
    let mut by_contest: BTreeMap<&str, (String, BTreeMap<String, usize>, usize)> = BTreeMap::new();
    for b in ballots {
        let entry = by_contest
            .entry(b.contest_id.as_str())
            .or_insert_with(|| (district_label(b), BTreeMap::new(), 0));
        if b.blank {
            entry.2 += 1;
        } else {
            *entry.1.entry(choice_label(b, blank_name)).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for (district, candidates, blank) in by_contest.into_values() {
        let mut rows: Vec<(String, usize)> = candidates.into_iter().collect();
        // 安定ソート: 同数なら、表示名の順（BTreeMap の順）のまま。
        rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        out.extend(rows.into_iter().map(|(choice, count)| ChoiceCount {
            district: district.clone(),
            choice,
            count,
            blank: false,
        }));
        if blank > 0 {
            out.push(ChoiceCount {
                district,
                choice: blank_name.to_string(),
                count: blank,
                blank: true,
            });
        }
    }
    out
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
            blank: false,
            district_name: with_names.then(|| "東京1区".to_string()),
            candidate_name: with_names.then(|| "甲".to_string()),
            party: with_names.then(|| "党".to_string()),
            slot: None,
            seq: None,
            supersedes: None,
            replaces: None,
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

    fn blank_ballot() -> BallotDto {
        BallotDto {
            candidate_id: "blank".to_string(),
            blank: true,
            candidate_name: None,
            party: None,
            ..ballot(true)
        }
    }

    #[test]
    fn ballot_rows_use_display_names_and_fall_back_to_ids() {
        let rows = ballot_rows(0, &[ballot(true), ballot(false)], "白票");
        assert_eq!(rows[0].index, 1);
        assert_eq!(rows[0].district, "東京1区");
        assert_eq!(rows[0].candidate, "甲（党）");
        assert_eq!(rows[1].index, 2);
        assert_eq!(rows[1].district, "2026-general/smd.13.01");
        assert_eq!(rows[1].candidate, "smd.13.01.c1");
        // 政党が空なら、氏名だけ。
        let mut b = ballot(true);
        b.party = Some(String::new());
        assert_eq!(ballot_rows(0, &[b], "白票")[0].candidate, "甲");
        assert!(ballot_rows(0, &[], "白票").is_empty());
        assert!(!rows[0].blank);
    }

    #[test]
    fn a_blank_ballot_is_shown_with_the_blank_name_not_as_a_candidate() {
        let rows = ballot_rows(0, &[blank_ballot()], "白票");
        assert_eq!(rows[0].candidate, "白票");
        assert!(rows[0].blank);
        assert_eq!(rows[0].district, "東京1区");
    }

    #[test]
    fn choice_counts_put_blank_on_its_own_row_after_the_candidates() {
        let mut other = ballot(true);
        other.candidate_id = "smd.13.01.c2".to_string();
        other.candidate_name = Some("乙".to_string());
        let ballots = [
            blank_ballot(),
            ballot(true),
            other.clone(),
            other,
            blank_ballot(),
            blank_ballot(),
        ];
        let rows: Vec<(String, usize, bool)> = choice_counts(&ballots, "白票")
            .into_iter()
            .map(|r| (r.choice, r.count, r.blank))
            .collect();
        // 白票が最多でも、候補者の後の、別の行。
        assert_eq!(
            rows,
            [
                ("乙（党）".to_string(), 2, false),
                ("甲（党）".to_string(), 1, false),
                ("白票".to_string(), 3, true),
            ]
        );
        // 白票が無ければ、白票の行は出さない。
        let rows = choice_counts(&[ballot(true)], "白票");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].blank);
        assert!(choice_counts(&[], "白票").is_empty());
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

    #[test]
    fn a_revote_links_to_the_ballot_it_replaced_with_a_to_b() {
        let mut b = blank_ballot();
        b.seq = Some(2);
        b.replaces = Some(shared_types::ReplacedBallotDto {
            ballot_id: "0123456789abcdef".repeat(2),
            height: 4,
            candidate_id: "smd.13.01.c1".to_string(),
            blank: false,
            candidate_name: Some("甲".to_string()),
        });
        let rows = ballot_rows(2, &[b, ballot(true)], "白票");
        let link = rows[0].replaces.as_ref().expect("link");
        assert_eq!(link.label, "#01234567…89abcdef を置き換え（甲→白票）");
        assert_eq!(
            link.path,
            format!("/chain/2/blocks/4#ballot-{}", "0123456789abcdef".repeat(2))
        );
        // 置き換えていない票（初回の投票）には、リンクを付けない。
        assert_eq!(rows[1].replaces, None);
        assert_eq!(ballot_anchor("ab"), "ballot-ab");
    }
}
