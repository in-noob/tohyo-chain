//! 画面の文言のうち、設定（`labels.*`）で変えられるもの。
//!
//! 値は**ビルド時**に環境変数で受け取る（web は他のワークスペースクレートに依存しない: 原則5）。
//! `eval "$(cargo run -q -p app-config -- web-env)"` してから `trunk build` / `trunk serve` を実行する
//! （`scripts/dev_up.sh` がやっている）。環境変数が無ければ、既定の文言になる（`config/default.toml` と同じ）。

use crate::flow;

/// ヘッダに表示するサイト名（`labels.site_title`）。
pub const DEFAULT_SITE_TITLE: &str = "投票システム（プロトタイプ）";
/// ログイン画面の見出し（`labels.login_heading`）。
pub const DEFAULT_LOGIN_HEADING: &str = "ログイン";
/// 投票用紙 1 枚の呼び名（`labels.ballot_item`）。
pub const DEFAULT_BALLOT_ITEM: &str = "投票用紙";
/// 進捗の表示の型（`labels.progress`）。`{total}` と `{current}` を置き換える。
pub const DEFAULT_PROGRESS: &str = "{total}枚中{current}枚目";
/// 候補者一覧の最後に置く白票の選択肢（`labels.blank_option`）。
pub const DEFAULT_BLANK_OPTION: &str = "白票（どの候補者にも投票しない）";
/// 確認画面で白票を選んだときの文言（`labels.blank_confirm`）。
pub const DEFAULT_BLANK_CONFIRM: &str = "白票として投票します。よろしいですか？";
/// 白票の呼び名（`labels.blank_name`）。ビューアの票の一覧で使う。
pub const DEFAULT_BLANK_NAME: &str = "白票";

pub fn site_title() -> &'static str {
    option_env!("APP_WEB_SITE_TITLE").unwrap_or(DEFAULT_SITE_TITLE)
}

/// 投票完了画面の文言（`labels.done_message`）。既定は [`flow::DONE_MESSAGE`]。
pub fn done_message() -> &'static str {
    option_env!("APP_WEB_DONE_MESSAGE").unwrap_or(flow::DONE_MESSAGE)
}

pub fn login_heading() -> &'static str {
    option_env!("APP_WEB_LOGIN_HEADING").unwrap_or(DEFAULT_LOGIN_HEADING)
}

/// 投票用紙 1 枚の呼び名。画面の文言・エラーの案内に使う。
pub fn ballot_item() -> &'static str {
    option_env!("APP_WEB_BALLOT_ITEM").unwrap_or(DEFAULT_BALLOT_ITEM)
}

pub fn progress_template() -> &'static str {
    option_env!("APP_WEB_PROGRESS").unwrap_or(DEFAULT_PROGRESS)
}

pub fn blank_option() -> &'static str {
    option_env!("APP_WEB_BLANK_OPTION").unwrap_or(DEFAULT_BLANK_OPTION)
}

pub fn blank_confirm() -> &'static str {
    option_env!("APP_WEB_BLANK_CONFIRM").unwrap_or(DEFAULT_BLANK_CONFIRM)
}

pub fn blank_name() -> &'static str {
    option_env!("APP_WEB_BLANK_NAME").unwrap_or(DEFAULT_BLANK_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_config_default_toml() {
        // ビルド時の環境変数が無ければ、既定の文言（config/default.toml の labels と同じ）。
        if option_env!("APP_WEB_SITE_TITLE").is_none() {
            assert_eq!(site_title(), "投票システム（プロトタイプ）");
        }
        if option_env!("APP_WEB_DONE_MESSAGE").is_none() {
            assert_eq!(done_message(), "投票を受け付けました");
        }
        if option_env!("APP_WEB_LOGIN_HEADING").is_none() {
            assert_eq!(login_heading(), "ログイン");
        }
        if option_env!("APP_WEB_BALLOT_ITEM").is_none() {
            assert_eq!(ballot_item(), "投票用紙");
        }
        if option_env!("APP_WEB_PROGRESS").is_none() {
            assert_eq!(progress_template(), "{total}枚中{current}枚目");
        }
        if option_env!("APP_WEB_BLANK_OPTION").is_none() {
            assert_eq!(blank_option(), "白票（どの候補者にも投票しない）");
        }
        if option_env!("APP_WEB_BLANK_CONFIRM").is_none() {
            assert_eq!(blank_confirm(), "白票として投票します。よろしいですか？");
        }
        if option_env!("APP_WEB_BLANK_NAME").is_none() {
            assert_eq!(blank_name(), "白票");
        }
    }
}
