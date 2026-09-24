//! 設定の不正を、「どのファイル（環境変数）のどの項目がなぜ不正か」で表す。

use std::fmt;
use std::path::PathBuf;

/// 設定値の出所。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `config/default.toml`（バイナリに埋め込まれた既定値）。
    Default,
    /// 設定ファイル（`config/dev.toml`、`config/local.toml` など）。
    File(PathBuf),
    /// `secrets/` ディレクトリのファイル。
    SecretFile(PathBuf),
    /// 環境変数（名前）。
    Env(String),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("config/default.toml（既定値）"),
            Self::File(path) => write!(f, "{}", path.display()),
            Self::SecretFile(path) => write!(f, "{}（secrets）", path.display()),
            Self::Env(name) => write!(f, "環境変数 {name}"),
        }
    }
}

/// 1 件の問題。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub origin: Origin,
    /// 項目（`seal.max_ballots` など）。ファイル全体の問題（構文エラーなど）では `None`。
    pub key: Option<String>,
    /// 不正な値の表示。秘密情報では `None`（値をメッセージに含めない）。
    pub value: Option<String>,
    pub reason: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.key, &self.value) {
            (Some(key), Some(value)) => write!(
                f,
                "{}: {key} = {value} は不正です（{}）",
                self.origin, self.reason
            ),
            (Some(key), None) => write!(f, "{}: {key}: {}", self.origin, self.reason),
            (None, _) => write!(f, "{}: {}", self.origin, self.reason),
        }
    }
}

/// 1 件以上の問題（すべてをまとめて報告する）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub issues: Vec<Issue>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "設定が不正です（{} 件）:", self.issues.len())?;
        for issue in &self.issues {
            write!(f, "\n  - {issue}")?;
        }
        Ok(())
    }
}
