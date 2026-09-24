//! `show` / `get` / `web-env` の出力。秘密情報は `***` に伏せる。

use toml::Value;

use crate::load::{Loaded, SECRET_KEYS, display_value};
use crate::secret::MASK;

/// 表示するセクションの順。
const SECTIONS: [&str; 15] = [
    "app",
    "api",
    "web",
    "db",
    "seal",
    "sealer",
    "shard",
    "auth",
    "session",
    "credentials",
    "election",
    "vote",
    "chain",
    "admin",
    "labels",
];

impl Loaded {
    /// 最終的に有効な設定を、項目ごとの出所つきで表示する。秘密情報は伏せる。
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# 有効な設定（優先順位: 既定 < config/<app.env>.toml < config/local.toml < secrets/ < 環境変数）\n",
        );
        // 秘密情報は、未設定でも行を出す（設定漏れに気づけるように）。
        let mut keys: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        for key in SECRET_KEYS {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        for section in SECTIONS {
            let prefix = format!("{section}.");
            let in_section: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|key| key.starts_with(&prefix))
                .collect();
            if in_section.is_empty() {
                continue;
            }
            out.push_str(&format!("\n[{section}]\n"));
            for key in in_section {
                let name = &key[prefix.len()..];
                let line = match self.entries.get(key) {
                    Some(entry) if SECRET_KEYS.contains(&key) => {
                        format!("{name} = \"{MASK}\"  # {}", entry.origin)
                    }
                    Some(entry) => {
                        format!(
                            "{name} = {}  # {}",
                            display_value(&entry.value),
                            entry.origin
                        )
                    }
                    None => format!("{name} = （未設定）  # 秘密情報。環境変数か secrets/ で渡す"),
                };
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    /// 1 つの項目の値（スクリプト用）。秘密情報は取得できない。配列はカンマ区切り。
    pub fn get(&self, key: &str) -> Result<String, String> {
        if SECRET_KEYS.contains(&key) {
            return Err(format!("{key} は秘密情報なので、取得できません"));
        }
        let entry = self
            .entries
            .get(key)
            .ok_or_else(|| format!("未知の項目です: {key}"))?;
        Ok(match &entry.value {
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","),
            other => other.to_string(),
        })
    }

    /// web のビルド時に渡す値を、`eval` できるシェルの `export` 行にする。
    /// web は `option_env!` で受け取る（web は app-config に依存しない）。
    pub fn web_env(&self) -> String {
        let labels = [
            ("APP_WEB_SITE_TITLE", &self.config.labels.site_title),
            ("APP_WEB_DONE_MESSAGE", &self.config.labels.done_message),
            ("APP_WEB_LOGIN_HEADING", &self.config.labels.login_heading),
            ("APP_WEB_BALLOT_ITEM", &self.config.labels.ballot_item),
            ("APP_WEB_PROGRESS", &self.config.labels.progress),
            ("APP_WEB_BLANK_OPTION", &self.config.labels.blank_option),
            ("APP_WEB_BLANK_CONFIRM", &self.config.labels.blank_confirm),
            ("APP_WEB_BLANK_NAME", &self.config.labels.blank_name),
        ];
        labels
            .iter()
            .map(|(name, value)| format!("export {name}={}\n", shell_quote(value)))
            .collect()
    }
}

/// シェルの単一引用符で包む（中の `'` は `'\''` にする）。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
