//! 型付きの設定（`AppConfig`）と、値の検証。問題はすべて集めて、出所つきで報告する。

use std::num::NonZeroU16;
use std::path::PathBuf;

use toml::Value;

use crate::error::{ConfigError, Issue, Origin};
use crate::load::{Entries, display_value};
use crate::secret::Secret;
use crate::time::parse_rfc3339;

/// キースペース名の最大長（Cassandra / ScyllaDB の上限）。
pub const MAX_KEYSPACE_LEN: usize = 48;
/// リースの TTL の下限（更新は TTL の 1/3 ごと。1 秒未満にならないように）。
pub const MIN_LEASE_TTL_SECS: u64 = 3;
/// セッション署名鍵の最小の長さ（バイト）。
pub const MIN_SESSION_SECRET_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    Dev,
    Production,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// api プロセスのメモリ（sealer は api 内蔵）。
    Memory,
    /// DB（sealer は別プロセス）。
    Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    Cassandra,
    Scylla,
}

impl DbBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cassandra => "cassandra",
            Self::Scylla => "scylla",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Stub,
    Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealBallots {
    Always,
    AfterClose,
}

/// 検証済みの日時。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp {
    pub text: String,
    pub unix_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub app: App,
    pub api: Api,
    pub web: Web,
    pub db: Db,
    pub seal: Seal,
    pub sealer: Sealer,
    pub shard: Shard,
    pub auth: Auth,
    pub session: Session,
    pub credentials: Credentials,
    pub election: Election,
    pub vote: Vote,
    pub chain: Chain,
    pub admin: Admin,
    pub labels: Labels,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub env: Env,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Api {
    pub port: u16,
    /// リクエストのタイムアウト秒（原則17の締切の手続きの待ち時間の計算にも使う: `state_cache_secs` に加える）。
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Web {
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Db {
    pub backend: DbBackend,
    /// 接続先（`host:port`）。1 件以上。
    pub nodes: Vec<String>,
    pub keyspace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seal {
    pub max_ballots: u64,
    pub interval_secs: u64,
    pub min_ballots_after_interval: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealer {
    /// `None` なら、起動ごとにランダムに決める。
    pub id: Option<String>,
    pub lease_ttl_secs: u64,
    /// 署名鍵の種（秘密情報）。sealer では必須。
    pub signing_seed: Option<Secret<[u8; 32]>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    pub count: NonZeroU16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Auth {
    pub mode: AuthMode,
    pub argon2: Argon2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argon2 {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub ttl_secs: u64,
    /// セッション署名鍵（秘密情報）。api では必須。
    pub secret: Option<Secret<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub output_file_enabled: bool,
    pub output_path: PathBuf,
    pub password_length: u32,
    /// 発行するログイン ID の長さ。
    pub login_id_length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Election {
    pub seed_dir: PathBuf,
    /// 読み込む選挙の ID（`seed_dir` の下のディレクトリ名）。
    pub election_id: String,
    pub voting_opens_at: Option<Timestamp>,
    pub voting_closes_at: Option<Timestamp>,
    /// 画面に表示するタイムゾーン（`Asia/Tokyo` | `UTC`）。
    pub display_timezone: DisplayTimezone,
    /// 選挙状態の短期キャッシュの秒数。締切の手続きの待ち時間（`state_cache_secs + api.request_timeout_secs`）に使う。
    pub state_cache_secs: u64,
}

/// 投票のルール（原則19: 選挙状態が open に移った時点の値を固定し、それ以降は設定を変えても使わない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    /// 白票（どの候補者にも投票しない）を選べるか。
    pub allow_blank: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayTimezone {
    pub name: &'static str,
    /// UTC からのオフセット秒。
    pub offset_secs: i64,
}

/// 対応するタイムゾーン（固定オフセット。IANA のタイムゾーン DB は依存に加えない）。
pub const DISPLAY_TIMEZONES: [(&str, i64); 2] = [("Asia/Tokyo", 9 * 3600), ("UTC", 0)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub reveal_ballots: RevealBallots,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admin {
    /// 管理用リスナーの待ち受け先（`host:port`）。公開用のポート（`api.port`）とは別。
    pub bind: String,
    /// 管理用エンドポイントのトークン（秘密情報）。`app.mode=db` かつ管理操作を使うときに必須。
    pub token: Option<Secret<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub site_title: String,
    pub done_message: String,
    pub login_heading: String,
    /// 投票用紙 1 枚の呼び名。
    pub ballot_item: String,
    /// 進捗の表示の型。`{total}` と `{current}` を含む。
    pub progress: String,
    /// 開始前に投票しようとしたときのメッセージ。
    pub voting_not_started_message: String,
    /// 締切の手続き中（closing）に投票しようとしたときのメッセージ。
    pub voting_closing_message: String,
    /// 終了後に投票しようとしたときのメッセージ。
    pub voting_closed_message: String,
    /// 候補者一覧の最後に置く、白票の選択肢の表示名。
    pub blank_option: String,
    /// 確認画面で、白票を選んだときに表示する文言。
    pub blank_confirm: String,
    /// 集計結果・ビューア・API のエラーでの、白票の呼び名。
    pub blank_name: String,
}

impl Election {
    /// 選挙データのディレクトリ: `<seed_dir>/<election_id>`。
    pub fn election_dir(&self) -> PathBuf {
        self.seed_dir.join(&self.election_id)
    }
}

/// 選挙の ID の最大長（`domain::ids::ELECTION_ID_MAX_LEN` と同じ）。
pub const ELECTION_ID_MAX_LEN: usize = 32;

/// 項目を読みながら、問題を集める。問題があっても、代わりの値を返して読み続ける（全件を一度に報告するため）。
struct Reader<'a> {
    entries: &'a Entries,
    issues: Vec<Issue>,
}

impl<'a> Reader<'a> {
    fn value(&mut self, key: &str) -> Option<&'a Value> {
        match self.entries.get(key) {
            Some(entry) => Some(&entry.value),
            None => {
                self.issues.push(Issue {
                    origin: Origin::Default,
                    key: Some(key.to_string()),
                    value: None,
                    reason: "config/default.toml にこの項目がありません（内部エラー）".to_string(),
                });
                None
            }
        }
    }

    /// 項目の値が不正なことを、その値の出所つきで記録する。
    fn bad(&mut self, key: &str, reason: impl Into<String>) {
        let (origin, value) = match self.entries.get(key) {
            Some(entry) => (entry.origin.clone(), Some(display_value(&entry.value))),
            None => (Origin::Default, None),
        };
        self.issues.push(Issue {
            origin,
            key: Some(key.to_string()),
            value,
            reason: reason.into(),
        });
    }

    fn text(&mut self, key: &str) -> String {
        self.value(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// 空でない文字列（最大 `max_chars` 文字）。
    fn label(&mut self, key: &str, max_chars: usize) -> String {
        let text = self.text(key);
        if text.trim().is_empty() {
            self.bad(key, "空にできません");
        } else if text.chars().count() > max_chars {
            self.bad(key, format!("{max_chars} 文字以内にしてください"));
        }
        text
    }

    fn uint(&mut self, key: &str, min: u64, max: u64) -> u64 {
        let n = self.value(key).and_then(Value::as_integer).unwrap_or(0);
        match u64::try_from(n) {
            Ok(n) if (min..=max).contains(&n) => n,
            _ => {
                self.bad(key, format!("{min} 以上 {max} 以下が必要です"));
                min
            }
        }
    }

    fn boolean(&mut self, key: &str) -> bool {
        self.value(key).and_then(Value::as_bool).unwrap_or_default()
    }

    fn choice<T: Copy>(&mut self, key: &str, options: &[(&str, T)]) -> T {
        let text = self.text(key);
        if let Some((_, found)) = options.iter().find(|(name, _)| *name == text) {
            return *found;
        }
        let names: Vec<&str> = options.iter().map(|(name, _)| *name).collect();
        self.bad(key, format!("{} のいずれかが必要です", names.join(" / ")));
        options[0].1
    }

    fn list(&mut self, key: &str) -> Vec<String> {
        self.value(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 秘密情報。値はメッセージに含めない。
    fn secret(&mut self, key: &str) -> Option<&'a str> {
        self.entries.get(key).and_then(|entry| entry.value.as_str())
    }

    fn secret_bad(&mut self, key: &str, reason: impl Into<String>) {
        let origin = self
            .entries
            .get(key)
            .map_or(Origin::Default, |entry| entry.origin.clone());
        self.issues.push(Issue {
            origin,
            key: Some(key.to_string()),
            value: None,
            reason: reason.into(),
        });
    }

    fn timestamp(&mut self, key: &str) -> Option<Timestamp> {
        let text = self.text(key);
        if text.is_empty() {
            return None;
        }
        match parse_rfc3339(&text) {
            Ok(unix_secs) => Some(Timestamp { text, unix_secs }),
            Err(reason) => {
                self.bad(key, reason);
                None
            }
        }
    }

    fn port(&mut self, key: &str) -> u16 {
        u16::try_from(self.uint(key, 1, u64::from(u16::MAX))).unwrap_or(1)
    }
}

/// キースペース名（CQL の識別子として完全修飾名に埋め込むので、厳しく検査する）。
pub fn check_keyspace(name: &str) -> Result<(), String> {
    let valid = name.len() <= MAX_KEYSPACE_LEN
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "先頭は英字、英数字と _ のみ、{MAX_KEYSPACE_LEN} 文字以内にしてください"
        ))
    }
}

/// 接続先（`host:port`）の形式。
fn check_node(node: &str) -> Result<(), String> {
    let Some((host, port)) = node.rsplit_once(':') else {
        return Err("host:port の形式が必要です".to_string());
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err("host が不正です".to_string());
    }
    match port.parse::<u16>() {
        Ok(p) if p >= 1 => Ok(()),
        _ => Err("port は 1〜65535 が必要です".to_string()),
    }
}

pub(crate) fn extract(entries: &Entries) -> Result<AppConfig, ConfigError> {
    let mut r = Reader {
        entries,
        issues: Vec::new(),
    };

    let env = r.choice(
        "app.env",
        &[
            ("dev", Env::Dev),
            ("production", Env::Production),
            ("test", Env::Test),
        ],
    );
    let mode = r.choice("app.mode", &[("memory", Mode::Memory), ("db", Mode::Db)]);
    let api_port = r.port("api.port");
    let web_port = r.port("web.port");
    if api_port == web_port {
        r.bad("web.port", "api.port と同じポートは使えません");
    }
    let request_timeout_secs = r.uint("api.request_timeout_secs", 1, 3600);

    let backend = r.choice(
        "db.backend",
        &[
            ("cassandra", DbBackend::Cassandra),
            ("scylla", DbBackend::Scylla),
        ],
    );
    let nodes = r.list("db.nodes");
    if nodes.is_empty() {
        r.bad("db.nodes", "接続先が 1 件以上必要です");
    }
    for node in &nodes {
        if let Err(reason) = check_node(node) {
            r.bad("db.nodes", format!("{node:?}: {reason}"));
        }
    }
    let keyspace = r.text("db.keyspace");
    if let Err(reason) = check_keyspace(&keyspace) {
        r.bad("db.keyspace", reason);
    }

    let max_ballots = r.uint("seal.max_ballots", 1, u64::from(u32::MAX));
    let interval_secs = r.uint("seal.interval_secs", 1, 31_536_000);
    let min_ballots_after_interval =
        r.uint("seal.min_ballots_after_interval", 1, u64::from(u32::MAX));

    let sealer_id = r.text("sealer.id");
    let lease_ttl_secs = r.uint("sealer.lease_ttl_secs", MIN_LEASE_TTL_SECS, 86_400);
    let signing_seed = match r.secret("sealer.signing_seed") {
        None => None,
        Some(hex) => match parse_seed(hex) {
            Some(seed) => Some(Secret::new(seed)),
            None => {
                r.secret_bad(
                    "sealer.signing_seed",
                    "64 桁の 16 進数（32 バイト）が必要です",
                );
                None
            }
        },
    };

    let shard_count = u16::try_from(r.uint("shard.count", 1, u64::from(u16::MAX))).unwrap_or(1);

    let auth_mode = r.choice(
        "auth.mode",
        &[("stub", AuthMode::Stub), ("db", AuthMode::Db)],
    );
    let memory_kib = r.uint("auth.argon2.memory_kib", 8, 4_194_304);
    let iterations = r.uint("auth.argon2.iterations", 1, 1000);
    let parallelism = r.uint("auth.argon2.parallelism", 1, 255);
    if memory_kib < 8 * parallelism {
        r.bad(
            "auth.argon2.memory_kib",
            "8 × auth.argon2.parallelism 以上が必要です",
        );
    }

    let ttl_secs = r.uint("session.ttl_secs", 1, 31_536_000);
    let session_secret = match r.secret("session.secret") {
        None => None,
        Some(secret) if secret.len() >= MIN_SESSION_SECRET_LEN => {
            Some(Secret::new(secret.to_string()))
        }
        Some(_) => {
            r.secret_bad(
                "session.secret",
                format!("{MIN_SESSION_SECRET_LEN} バイト以上が必要です"),
            );
            None
        }
    };

    let output_file_enabled = r.boolean("credentials.output_file_enabled");
    let output_path = r.text("credentials.output_path");
    if output_file_enabled && output_path.trim().is_empty() {
        r.bad(
            "credentials.output_path",
            "出力を有効にするときは空にできません",
        );
    }
    let password_length = r.uint("credentials.password_length", 8, 128);
    let login_id_length = r.uint("credentials.login_id_length", 8, 32);

    let seed_dir = r.text("election.seed_dir");
    if seed_dir.trim().is_empty() {
        r.bad("election.seed_dir", "空にできません");
    }
    let election_id = r.text("election.election_id");
    let id_ok = !election_id.is_empty()
        && election_id.len() <= ELECTION_ID_MAX_LEN
        && election_id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && election_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !id_ok {
        r.bad(
            "election.election_id",
            format!(
                "小文字の英数字・_・- のみ、先頭は英数字、{ELECTION_ID_MAX_LEN} 文字以内にしてください"
            ),
        );
    }
    let opens = r.timestamp("election.voting_opens_at");
    let closes = r.timestamp("election.voting_closes_at");
    if let (Some(opens), Some(closes)) = (&opens, &closes)
        && opens.unix_secs >= closes.unix_secs
    {
        r.bad(
            "election.voting_closes_at",
            "election.voting_opens_at より後の日時が必要です",
        );
    }
    let display_timezone = r.choice(
        "election.display_timezone",
        &DISPLAY_TIMEZONES
            .iter()
            .map(|&(name, offset_secs)| (name, DisplayTimezone { name, offset_secs }))
            .collect::<Vec<_>>(),
    );
    let state_cache_secs = r.uint("election.state_cache_secs", 0, 3600);

    let allow_blank = r.boolean("vote.allow_blank");

    let reveal = r.choice(
        "chain.reveal_ballots",
        &[
            ("always", RevealBallots::Always),
            ("after_close", RevealBallots::AfterClose),
        ],
    );
    if reveal == RevealBallots::AfterClose && closes.is_none() {
        r.bad(
            "chain.reveal_ballots",
            "after_close には election.voting_closes_at の指定が必要です",
        );
    }

    let labels = Labels {
        site_title: r.label("labels.site_title", 100),
        done_message: r.label("labels.done_message", 200),
        login_heading: r.label("labels.login_heading", 100),
        ballot_item: r.label("labels.ballot_item", 30),
        progress: r.label("labels.progress", 100),
        voting_not_started_message: r.label("labels.voting_not_started_message", 200),
        voting_closing_message: r.label("labels.voting_closing_message", 200),
        voting_closed_message: r.label("labels.voting_closed_message", 200),
        blank_option: r.label("labels.blank_option", 100),
        blank_confirm: r.label("labels.blank_confirm", 200),
        blank_name: r.label("labels.blank_name", 30),
    };
    if !labels.progress.contains("{total}") || !labels.progress.contains("{current}") {
        r.bad(
            "labels.progress",
            "{total} と {current} の両方を含めてください（例: {total}枚中{current}枚目）",
        );
    }

    let admin_bind = r.text("admin.bind");
    if check_node(&admin_bind).is_err() {
        r.bad("admin.bind", "host:port の形式が必要です");
    }
    let admin_token = match r.secret("admin.token") {
        None => None,
        Some(token) if token.len() >= MIN_SESSION_SECRET_LEN => {
            Some(Secret::new(token.to_string()))
        }
        Some(_) => {
            r.secret_bad(
                "admin.token",
                format!("{MIN_SESSION_SECRET_LEN} バイト以上が必要です"),
            );
            None
        }
    };

    // DB 認証（事前登録したログイン ID とパスワード）は、DB に登録された認証情報と名簿を使う。
    if auth_mode == AuthMode::Db && mode == Mode::Memory {
        r.bad(
            "auth.mode",
            "db には app.mode=db が必要です（認証情報と名簿は DB にあります。memory では stub だけが使えます）",
        );
    }

    // 本番では、メモリ上の保存（再起動で票が消える）を使えない。
    if env == Env::Production && mode == Mode::Memory {
        r.bad("app.mode", "app.env=production では db が必要です");
    }

    if !r.issues.is_empty() {
        return Err(ConfigError { issues: r.issues });
    }
    Ok(AppConfig {
        app: App { env, mode },
        api: Api {
            port: api_port,
            request_timeout_secs,
        },
        web: Web { port: web_port },
        db: Db {
            backend,
            nodes,
            keyspace,
        },
        seal: Seal {
            max_ballots,
            interval_secs,
            min_ballots_after_interval,
        },
        sealer: Sealer {
            id: (!sealer_id.trim().is_empty()).then_some(sealer_id),
            lease_ttl_secs,
            signing_seed,
        },
        shard: Shard {
            count: NonZeroU16::new(shard_count).unwrap_or(NonZeroU16::MIN),
        },
        auth: Auth {
            mode: auth_mode,
            argon2: Argon2 {
                memory_kib: u32::try_from(memory_kib).unwrap_or(u32::MAX),
                iterations: u32::try_from(iterations).unwrap_or(u32::MAX),
                parallelism: u32::try_from(parallelism).unwrap_or(u32::MAX),
            },
        },
        session: Session {
            ttl_secs,
            secret: session_secret,
        },
        credentials: Credentials {
            output_file_enabled,
            output_path: PathBuf::from(output_path),
            password_length: u32::try_from(password_length).unwrap_or(u32::MAX),
            login_id_length: u32::try_from(login_id_length).unwrap_or(u32::MAX),
        },
        election: Election {
            seed_dir: PathBuf::from(seed_dir),
            election_id,
            voting_opens_at: opens,
            voting_closes_at: closes,
            display_timezone,
            state_cache_secs,
        },
        vote: Vote { allow_blank },
        chain: Chain {
            reveal_ballots: reveal,
        },
        admin: Admin {
            bind: admin_bind,
            token: admin_token,
        },
        labels,
    })
}

/// 64 桁の 16 進数を 32 バイトにする。
fn parse_seed(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
