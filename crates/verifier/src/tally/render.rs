//! 集計のターミナル表示（表形式）。日本語の全角文字は表示幅 2 として、列をそろえる。

use std::fmt::Write;

use super::compute::{DistrictTally, GroupTotal, RevoteReport, Tally};
use super::gate::Phase;
use super::{Meta, Reconciliation};

/// 選挙区ごとの表を、すべて表示する選挙区数の上限（超えたら、先頭だけ表示して、残りは CSV / JSON を案内する）。
pub const MAX_DISTRICTS_SHOWN: usize = 30;

/// 文字の表示幅（全角・CJK は 2、それ以外は 1）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    match u32::from(c) {
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 => 2,
        _ => 1,
    }
}

fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(display_width(text)));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// 表を作る。`right[i]` が真の列は、右寄せ（数値）。各行は先頭に 2 文字の字下げをつける。
pub fn table(headers: &[&str], rows: &[Vec<String>], right: &[bool]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(display_width(cell));
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (cell, w))| pad(cell, *w, right.get(i).copied().unwrap_or(false)))
            .collect();
        format!("  {}\n", padded.join("  ").trim_end())
    };
    let mut out = line(headers.to_vec());
    let rule: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
    let _ = writeln!(out, "  {}", "-".repeat(rule));
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

/// 順位（同数は同順位。次の順位は飛ばす: 1, 2, 2, 4）。
pub fn rank(d: &DistrictTally, index: usize) -> usize {
    let votes = d.candidates[index].votes;
    1 + d.candidates.iter().filter(|c| c.votes > votes).count()
}

/// 選挙区 1 つの表。候補者（順位つき）の後に、白票（`blank_name`）・合計・投票済み者数を、候補者とは別の行で置く
/// （白票には順位も政党も付けない）。
fn district_table(d: &DistrictTally, blank_name: &str) -> String {
    let mut rows: Vec<Vec<String>> = d
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            vec![
                rank(d, i).to_string(),
                c.name.clone(),
                c.party.clone(),
                c.votes.to_string(),
            ]
        })
        .collect();
    let extra = |label: &str, n: u64| {
        vec![
            String::new(),
            label.to_string(),
            String::new(),
            n.to_string(),
        ]
    };
    rows.push(extra(blank_name, d.blank));
    rows.push(extra("合計", d.total));
    rows.push(extra("投票済み者数", d.participation));
    let scope = if d.prefectures.is_empty() {
        String::new()
    } else {
        format!("・{}", d.prefectures.join("・"))
    };
    let mut out = format!("■ {}（{}{scope}）\n", d.name, d.type_name);
    out.push_str(&table(
        &["順位", "候補者", "政党", "得票数"],
        &rows,
        &[true, false, false, true],
    ));
    out
}

fn group_table(title: &str, groups: &[GroupTotal], grand: &GroupTotal, blank_name: &str) -> String {
    let mut rows: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            let name = if g.wide {
                format!("{}（複数の都道府県）", g.name)
            } else {
                g.name.clone()
            };
            group_row(&name, g)
        })
        .collect();
    rows.push(group_row("全体", grand));
    let mut out = format!("■ {title}\n");
    out.push_str(&table(
        &[
            "名称",
            "選挙区数",
            "有効票",
            blank_name,
            "合計",
            "投票済み数",
        ],
        &rows,
        &[false, true, true, true, true, true],
    ));
    out
}

fn group_row(name: &str, g: &GroupTotal) -> Vec<String> {
    vec![
        name.to_string(),
        g.districts.to_string(),
        g.valid.to_string(),
        g.blank.to_string(),
        g.total.to_string(),
        g.participation.to_string(),
    ]
}

/// 集計の全体を、ターミナル用の文字列にする。`all_districts` が偽なら、選挙区ごとの表は先頭の
/// [`MAX_DISTRICTS_SHOWN`] 件まで。
pub fn render(
    tally: &Tally,
    meta: &Meta<'_>,
    recon: &Reconciliation,
    phase: Phase,
    all_districts: bool,
) -> String {
    let item = meta.ballot_item;
    let mut out = String::new();
    let kind = match phase {
        Phase::Final => "集計（締切後）",
        Phase::Interim => "中間集計（締切前または締切未設定。確定した結果ではありません）",
    };
    let _ = writeln!(
        out,
        "== {}（{}）: {kind} ==\n",
        tally.election_name, tally.election_id
    );

    let shown = if all_districts {
        tally.districts.len()
    } else {
        tally.districts.len().min(MAX_DISTRICTS_SHOWN)
    };
    let _ = writeln!(out, "【選挙区別】{} 選挙区\n", tally.districts.len());
    for d in &tally.districts[..shown] {
        out.push_str(&district_table(d, meta.blank_name));
        out.push('\n');
    }
    if shown < tally.districts.len() {
        let _ = writeln!(
            out,
            "（残り {} 選挙区は、表示を省略しました。CSV / JSON を見るか、--all で全件を表示してください）\n",
            tally.districts.len() - shown
        );
    }

    let grand = tally.grand_total();
    let note = format!(
        "投票済み数は、{item}の枚数（同じ有権者が複数の{item}に投票するので、人数ではありません）"
    );
    out.push_str(&group_table(
        &format!("都道府県別の合計（{note}）"),
        &tally.prefectures,
        &grand,
        meta.blank_name,
    ));
    out.push('\n');
    out.push_str(&group_table(
        &format!("選挙の種類別の合計（{note}）"),
        &tally.types,
        &grand,
        meta.blank_name,
    ));
    out.push('\n');
    out.push_str(&render_reconciliation(recon, item));
    out
}

/// 再投票の件数と変更の内訳（A→B の件数表。締切後の集計だけで表示する）。白票は `blank_name`。
pub fn render_revotes(report: &RevoteReport, ballot_item: &str, blank_name: &str) -> String {
    let mut out = String::from("■ 再投票（締切後）\n");
    let _ = writeln!(
        out,
        "  再投票の件数: {} 件（投票した有権者 {} 人。集計は、それぞれの最後の票だけを数えています）",
        report.revotes, report.slots
    );
    if report.contests.is_empty() {
        return out;
    }
    let name = |id: &str, name: &Option<String>| match name {
        Some(name) => name.clone(),
        None if id == domain::BLANK_CANDIDATE_ID => blank_name.to_string(),
        None => id.to_string(),
    };
    let rows: Vec<Vec<String>> = report
        .contests
        .iter()
        .flat_map(|c| {
            c.changes.iter().map(move |change| {
                vec![
                    format!("{}（{}）", c.name, c.type_name),
                    format!(
                        "{} → {}",
                        name(&change.from_candidate_id, &change.from_name),
                        name(&change.to_candidate_id, &change.to_name)
                    ),
                    change.count.to_string(),
                ]
            })
        })
        .collect();
    out.push_str(&table(
        &[ballot_item, "変更の内訳（前 → 後）", "件数"],
        &rows,
        &[false, false, true],
    ));
    out
}

/// 突合の結果（検証と突合が済んでいる前提の要約）。
pub fn render_reconciliation(recon: &Reconciliation, ballot_item: &str) -> String {
    let mismatched = recon.contests.iter().filter(|r| !r.consistent).count();
    let unsealed: u64 = recon.contests.iter().map(|r| r.pending).sum();
    let mut out = String::from("■ 突合の結果\n");
    let _ = writeln!(
        out,
        "  チェーンの検証: OK（{} シャード, {} ブロック, {} 票）",
        recon.shards, recon.blocks, recon.ballots
    );
    let _ = writeln!(
        out,
        "  {ballot_item}別の突合（投票済み記録 = 封印済みの slot の数（再投票は 1 人 1 つ））: {} 枚中 {} 枚が一致、未封印 {unsealed} 件",
        recon.contests.len(),
        recon.contests.len() - mismatched
    );
    let _ = writeln!(out, "  ballot_id の重複: {} 件", recon.duplicate_ballots);
    let _ = writeln!(out, "  アンカー: {}", recon.anchor);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_characters_count_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("東京1区"), 7);
        assert_eq!(display_width("白票"), 4);
        assert_eq!(display_width("ｱ"), 1);
    }

    #[test]
    fn tables_align_columns_by_display_width() {
        let rows = vec![
            vec!["東京1区".to_string(), "12".to_string()],
            vec!["A".to_string(), "3".to_string()],
        ];
        let out = table(&["名称", "票"], &rows, &[false, true]);
        let lines: Vec<&str> = out.lines().collect();
        // 数値列は右寄せで、同じ表示幅の位置で終わる。
        assert_eq!(display_width(lines[2]), display_width(lines[3]));
        assert!(lines[2].ends_with("12") && lines[3].ends_with(" 3"));
    }

    #[test]
    fn ties_share_a_rank_and_skip_the_next() {
        let cand = |votes| super::super::compute::CandidateVotes {
            candidate_id: String::new(),
            name: String::new(),
            party: String::new(),
            votes,
        };
        let d = DistrictTally {
            contest_id: String::new(),
            district_id: String::new(),
            name: String::new(),
            election_type: String::new(),
            type_name: String::new(),
            prefectures: Vec::new(),
            candidates: vec![cand(5), cand(3), cand(3), cand(0)],
            blank: 0,
            valid: 11,
            total: 11,
            participation: 11,
        };
        let ranks: Vec<usize> = (0..4).map(|i| rank(&d, i)).collect();
        assert_eq!(ranks, [1, 2, 2, 4]);
    }

    #[test]
    fn blank_is_a_separate_row_named_by_the_label_without_a_rank() {
        let d = DistrictTally {
            contest_id: String::new(),
            district_id: String::new(),
            name: "東京1区".to_string(),
            election_type: String::new(),
            type_name: "小選挙区".to_string(),
            prefectures: Vec::new(),
            candidates: vec![super::super::compute::CandidateVotes {
                candidate_id: "c1".to_string(),
                name: "甲".to_string(),
                party: "党".to_string(),
                votes: 2,
            }],
            blank: 7,
            valid: 2,
            total: 9,
            participation: 9,
        };
        let out = district_table(&d, "白票（設定の呼び名）");
        let blank_line = out
            .lines()
            .find(|l| l.contains("白票（設定の呼び名）"))
            .expect("blank row");
        // 順位の列は空で、得票数の列に 7。候補者の行（順位 1）とは別の行。
        assert!(
            blank_line.trim_start().starts_with("白票（設定の呼び名）"),
            "{out}"
        );
        assert!(blank_line.trim_end().ends_with('7'), "{out}");
        let candidate_line = out.lines().find(|l| l.contains('甲')).expect("candidate");
        assert!(candidate_line.trim_start().starts_with('1'), "{out}");
        assert!(!candidate_line.contains("白票"), "{out}");
    }
}
