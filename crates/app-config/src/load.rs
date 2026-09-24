//! 設定の層（既定 → 環境別ファイル → local.toml → secrets/ → 環境変数）を重ね、項目ごとに出所を記録する。
//!
//! 値の型は `config/default.toml` が決める（すべての項目がそこにある）。ファイルや環境変数の値が、その型に合わない、
//! または未知の項目（タイプミス）なら、出所つきのエラーにする。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::error::{ConfigError, Issue, Origin};
use crate::model::{self, AppConfig};

/// `config/default.toml`（バイナリに埋め込む。編集したら再ビルドする）。
pub const DEFAULT_TOML: &str = include_str!("../../../config/default.toml");

/// 秘密情報の項目。設定ファイルには書けない。`secrets/<. を _ にした名前>` か、環境変数 `APP__…` で渡す。
pub const SECRET_KEYS: [&str; 3] = ["session.secret", "sealer.signing_seed", "admin.token"];

/// 環境変数の接頭辞。`APP__SEAL__MAX_BALLOTS` → 項目 `seal.max_ballots`。
pub const ENV_PREFIX: &str = "APP__";

const ENVIRONMENTS: [&str; 3] = ["dev", "production", "test"];

/// 1 つの項目の値と、その出所。
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub value: Value,
    pub origin: Origin,
}

pub(crate) type Entries = BTreeMap<String, Entry>;

/// 設定の入力元。テストでは、ファイルなし・環境変数だけの構成を作れる。
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// 設定ファイルのディレクトリ。`None` なら、ファイルは読まない（既定値と環境変数だけ）。
    pub config_dir: Option<PathBuf>,
    /// 秘密情報のディレクトリ。`None` なら読まない。
    pub secrets_dir: Option<PathBuf>,
    /// 環境変数（`APP__` で始まるものだけを渡す）。
    pub env: Vec<(String, String)>,
}

impl Sources {
    /// 実プロセスの入力元。`APP_CONFIG_DIR`（既定 `config`）、`APP_SECRETS_DIR`（既定 `secrets`）、`APP__…`。
    pub fn from_process() -> Self {
        let dir = |name: &str, default: &str| {
            std::env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        let env = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, _)| name.starts_with(ENV_PREFIX))
            .collect();
        Self {
            config_dir: Some(dir("APP_CONFIG_DIR", "config")),
            secrets_dir: Some(dir("APP_SECRETS_DIR", "secrets")),
            env,
        }
    }

    /// テスト用: ファイルなし、`(項目, 値)` を環境変数として渡した入力元。
    pub fn overrides(pairs: &[(&str, &str)]) -> Self {
        Self {
            config_dir: None,
            secrets_dir: None,
            env: pairs
                .iter()
                .map(|(key, value)| (env_name(key), (*value).to_string()))
                .collect(),
        }
    }
}

/// 項目名（`seal.max_ballots`）に対応する環境変数名（`APP__SEAL__MAX_BALLOTS`）。
pub fn env_name(key: &str) -> String {
    format!("{ENV_PREFIX}{}", key.replace('.', "__").to_uppercase())
}

/// 読み込んだ設定。型付きの値と、項目ごとの生の値・出所を持つ。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: AppConfig,
    pub(crate) entries: Entries,
    /// 秘密情報のディレクトリ（`APP_SECRETS_DIR`、既定 `secrets`）。テスト用の読み込みでは `None`。
    pub secrets_dir: Option<PathBuf>,
}

/// 実プロセスの入力元から読み込む。
pub fn load() -> Result<Loaded, ConfigError> {
    load_from(&Sources::from_process())
}

pub fn load_from(sources: &Sources) -> Result<Loaded, ConfigError> {
    let mut issues = Vec::new();

    // 型の定義元（default.toml）。
    let default_items = parse_layer(DEFAULT_TOML, &Origin::Default, &mut issues);
    let mut entries = Entries::new();
    for (key, value) in default_items {
        entries.insert(
            key,
            Entry {
                value,
                origin: Origin::Default,
            },
        );
    }
    let kinds: BTreeMap<String, Kind> = entries
        .iter()
        .map(|(key, entry)| (key.clone(), Kind::of(&entry.value)))
        .collect();
    if !issues.is_empty() {
        return Err(ConfigError { issues });
    }

    // local.toml を先に読む（app.env を決めるのにも使う）。
    let local_path = sources
        .config_dir
        .as_ref()
        .map(|dir| dir.join("local.toml"));
    let local_items = local_path
        .as_deref()
        .and_then(|path| read_file_layer(path, &mut issues));

    // app.env: 環境変数 > local.toml > 既定値。
    let env_var = sources
        .env
        .iter()
        .find(|(name, _)| name == &env_name("app.env"));
    let env_name_value = match (env_var, &local_items) {
        (Some((_, value)), _) => value.trim().to_string(),
        (None, Some(items)) => items
            .iter()
            .find(|(key, _)| key == "app.env")
            .and_then(|(_, value)| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "dev".to_string()),
        (None, None) => "dev".to_string(),
    };
    if !ENVIRONMENTS.contains(&env_name_value.as_str()) {
        let origin = match env_var {
            Some((name, _)) => Origin::Env(name.clone()),
            None => local_path.clone().map_or(Origin::Default, Origin::File),
        };
        issues.push(Issue {
            origin,
            key: Some("app.env".to_string()),
            value: Some(format!("{env_name_value:?}")),
            reason: "dev / production / test のいずれかが必要です".to_string(),
        });
        return Err(ConfigError { issues });
    }

    // 環境別ファイル（config/<app.env>.toml）。production だけは必須。
    if let Some(dir) = &sources.config_dir {
        let path = dir.join(format!("{env_name_value}.toml"));
        match read_file_layer(&path, &mut issues) {
            Some(items) => apply(&mut entries, items, Origin::File(path), &kinds, &mut issues),
            None if env_name_value == "production" && !path.exists() => issues.push(Issue {
                origin: Origin::File(path),
                key: None,
                value: None,
                reason: "app.env=production には、このファイルが必要です（config/production.example.toml をコピーして作成）"
                    .to_string(),
            }),
            None => {}
        }
    }
    if let (Some(items), Some(path)) = (local_items, local_path) {
        apply(&mut entries, items, Origin::File(path), &kinds, &mut issues);
    }

    // secrets/ ディレクトリ（1 ファイル 1 値）。
    if let Some(dir) = &sources.secrets_dir {
        for key in SECRET_KEYS {
            let path = dir.join(key.replace('.', "_"));
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    entries.insert(
                        key.to_string(),
                        Entry {
                            value: Value::String(text.trim_end_matches(['\n', '\r']).to_string()),
                            origin: Origin::SecretFile(path),
                        },
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => issues.push(Issue {
                    origin: Origin::SecretFile(path),
                    key: Some(key.to_string()),
                    value: None,
                    reason: format!("読み込めません: {e}"),
                }),
            }
        }
    }

    // 環境変数（最優先）。
    apply_env(&mut entries, &sources.env, &kinds, &mut issues);

    // app.env は、選ばれた環境と食い違ってはいけない（環境別ファイルが別の環境を名乗るなど）。
    if let Some(entry) = entries.get("app.env")
        && entry.value.as_str() != Some(env_name_value.as_str())
    {
        issues.push(Issue {
            origin: entry.origin.clone(),
            key: Some("app.env".to_string()),
            value: Some(display_value(&entry.value)),
            reason: format!(
                "選ばれている環境は {env_name_value:?} です。環境別ファイルでは app.env を変えられません"
            ),
        });
    }

    if !issues.is_empty() {
        return Err(ConfigError { issues });
    }
    let config = model::extract(&entries)?;
    Ok(Loaded {
        config,
        entries,
        secrets_dir: sources.secrets_dir.clone(),
    })
}

/// 項目の型。default.toml の値から決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Bool,
    List,
}

impl Kind {
    fn of(value: &Value) -> Self {
        match value {
            Value::Integer(_) => Self::Int,
            Value::Boolean(_) => Self::Bool,
            Value::Array(_) => Self::List,
            _ => Self::Str,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Str => "文字列が必要です",
            Self::Int => "整数が必要です",
            Self::Bool => "true か false が必要です",
            Self::List => "文字列の配列が必要です",
        }
    }
}

/// 値を、エラーメッセージ用に表示する。
pub(crate) fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(display_value).collect();
            format!("[{}]", inner.join(", "))
        }
        other => other.to_string(),
    }
}

/// TOML テキストを、`セクション.項目` の平らな一覧にする。構文エラーは問題として記録する。
fn parse_layer(text: &str, origin: &Origin, issues: &mut Vec<Issue>) -> Vec<(String, Value)> {
    match text.parse::<toml::Table>() {
        Ok(table) => {
            let mut out = Vec::new();
            flatten("", &table, &mut out);
            out
        }
        Err(e) => {
            issues.push(Issue {
                origin: origin.clone(),
                key: None,
                value: None,
                reason: format!("TOML として読めません: {}", e.message()),
            });
            Vec::new()
        }
    }
}

fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, Value)>) {
    for (name, value) in table {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            Value::Table(inner) => flatten(&key, inner, out),
            other => out.push((key, other.clone())),
        }
    }
}

/// 設定ファイルを読む。無ければ `None`。読めない・構文エラーなら問題を記録して `None`。
fn read_file_layer(path: &Path, issues: &mut Vec<Issue>) -> Option<Vec<(String, Value)>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let before = issues.len();
            let items = parse_layer(&text, &Origin::File(path.to_path_buf()), issues);
            (issues.len() == before).then_some(items)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            issues.push(Issue {
                origin: Origin::File(path.to_path_buf()),
                key: None,
                value: None,
                reason: format!("読み込めません: {e}"),
            });
            None
        }
    }
}

/// ファイルの層を重ねる。秘密情報・未知の項目・型違いは問題にする。
fn apply(
    entries: &mut Entries,
    items: Vec<(String, Value)>,
    origin: Origin,
    kinds: &BTreeMap<String, Kind>,
    issues: &mut Vec<Issue>,
) {
    for (key, value) in items {
        let issue = |value: Option<String>, reason: String| Issue {
            origin: origin.clone(),
            key: Some(key.clone()),
            value,
            reason,
        };
        if SECRET_KEYS.contains(&key.as_str()) {
            // 値はメッセージに含めない。
            issues.push(issue(
                None,
                format!(
                    "秘密情報は設定ファイルに書けません。環境変数 {} か secrets/{} で渡してください",
                    env_name(&key),
                    key.replace('.', "_")
                ),
            ));
            continue;
        }
        let Some(&kind) = kinds.get(&key) else {
            issues.push(issue(
                Some(display_value(&value)),
                "未知の項目です（config/default.toml にありません。タイプミスでは？）".to_string(),
            ));
            continue;
        };
        // TOML の日時型は、文字列の項目（voting_opens_at など）として受け取る。
        let value = match (kind, value) {
            (Kind::Str, Value::Datetime(dt)) => Value::String(dt.to_string()),
            (_, value) => value,
        };
        let fits = match (&value, kind) {
            (Value::String(_), Kind::Str)
            | (Value::Integer(_), Kind::Int)
            | (Value::Boolean(_), Kind::Bool) => true,
            (Value::Array(items), Kind::List) => items.iter().all(Value::is_str),
            _ => false,
        };
        if !fits {
            issues.push(issue(
                Some(display_value(&value)),
                kind.describe().to_string(),
            ));
            continue;
        }
        entries.insert(
            key,
            Entry {
                value,
                origin: origin.clone(),
            },
        );
    }
}

/// 環境変数の層。値は文字列なので、項目の型に合わせて解釈する。
fn apply_env(
    entries: &mut Entries,
    env: &[(String, String)],
    kinds: &BTreeMap<String, Kind>,
    issues: &mut Vec<Issue>,
) {
    for (name, raw) in env {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let key = rest.to_lowercase().replace("__", ".");
        let origin = Origin::Env(name.clone());
        if SECRET_KEYS.contains(&key.as_str()) {
            entries.insert(
                key,
                Entry {
                    value: Value::String(raw.clone()),
                    origin,
                },
            );
            continue;
        }
        let Some(&kind) = kinds.get(&key) else {
            issues.push(Issue {
                origin,
                key: Some(key),
                value: Some(format!("{raw:?}")),
                reason: "未知の項目です（config/default.toml にありません。タイプミスでは？）"
                    .to_string(),
            });
            continue;
        };
        let parsed = match kind {
            Kind::Str => Some(Value::String(raw.clone())),
            Kind::Int => raw.trim().parse::<i64>().ok().map(Value::Integer),
            Kind::Bool => match raw.trim() {
                "true" => Some(Value::Boolean(true)),
                "false" => Some(Value::Boolean(false)),
                _ => None,
            },
            Kind::List => Some(Value::Array(
                raw.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            )),
        };
        match parsed {
            Some(value) => {
                entries.insert(key, Entry { value, origin });
            }
            None => issues.push(Issue {
                origin,
                key: Some(key),
                value: Some(format!("{raw:?}")),
                reason: kind.describe().to_string(),
            }),
        }
    }
}
