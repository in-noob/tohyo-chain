//! ログイン画面・進捗画面に表示する、選挙の状態と投票の受付期間の文言（原則17・18）。
//!
//! 状態そのものの呼び名（投票開始前・投票受付中…）は、`labels`（利用者に見える呼び名）の対象ではない
//! （原則12 が対象にするのは site_title・done_message・login_heading・ballot_item・progress・
//! voting_*_message で、状態の表示名はここでは扱わない。進捗画面の「済み／今／これから」と同じ扱い）。

use shared_types::ElectionStatusResponse;
use shared_types::time::format_offset;

pub fn phase_label(phase: &str) -> &'static str {
    match phase {
        "scheduled" => "投票開始前",
        "open" => "投票受付中",
        "closing" => "投票受付終了処理中",
        "closed" => "投票終了",
        _ => "状態不明",
    }
}

/// 表示用の 1 行（例: 「投票受付中（受付期間: 2026-10-25T07:00:00+09:00 〜 2026-10-25T20:00:00+09:00、Asia/Tokyo）」）。
pub fn summary_line(status: &ElectionStatusResponse) -> String {
    let fmt = |secs: i64| format_offset(secs, status.display_timezone_offset_secs);
    let label = phase_label(&status.phase);
    match (status.opens_at, status.closes_at) {
        (Some(o), Some(c)) => format!(
            "{label}（受付期間: {} 〜 {}、{}）",
            fmt(o),
            fmt(c),
            status.display_timezone
        ),
        (Some(o), None) => format!("{label}（開始: {}、{}）", fmt(o), status.display_timezone),
        (None, Some(c)) => format!("{label}（締切: {}、{}）", fmt(c), status.display_timezone),
        (None, None) => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        phase: &str,
        opens_at: Option<i64>,
        closes_at: Option<i64>,
    ) -> ElectionStatusResponse {
        ElectionStatusResponse {
            phase: phase.to_string(),
            opens_at,
            closes_at,
            now: 0,
            display_timezone: "Asia/Tokyo".to_string(),
            display_timezone_offset_secs: 9 * 3600,
            rules: None,
            election_hash: "e1".repeat(32),
        }
    }

    #[test]
    fn labels_cover_every_phase() {
        assert_eq!(phase_label("scheduled"), "投票開始前");
        assert_eq!(phase_label("open"), "投票受付中");
        assert_eq!(phase_label("closing"), "投票受付終了処理中");
        assert_eq!(phase_label("closed"), "投票終了");
        assert_eq!(phase_label("bogus"), "状態不明");
    }

    #[test]
    fn summary_includes_the_period_when_present() {
        let s = status("open", Some(1_790_812_800), Some(1_790_856_000));
        let line = summary_line(&s);
        assert!(line.starts_with("投票受付中（受付期間: "));
        assert!(line.contains("Asia/Tokyo"));
        assert!(line.contains('〜'));
    }

    #[test]
    fn summary_without_a_period_is_just_the_phase() {
        let s = status("scheduled", None, None);
        assert_eq!(summary_line(&s), "投票開始前");
    }

    #[test]
    fn summary_with_only_one_bound() {
        let s = status("scheduled", Some(1_790_812_800), None);
        assert!(summary_line(&s).starts_with("投票開始前（開始: "));
        let s = status("open", None, Some(1_790_856_000));
        assert!(summary_line(&s).starts_with("投票受付中（締切: "));
    }
}
