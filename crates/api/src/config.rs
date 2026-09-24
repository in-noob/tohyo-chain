//! api の設定。読み込みと検証は `app-config`（config/*.toml、secrets/、環境変数 `APP__…`）が行い、ここではその結果を
//! api が使う形に変換する。

use std::fmt;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};
use app_config::{AppConfig, AuthMode, DisplayTimezone, Mode, RevealBallots, Secret};
use application::PasswordParams;
use domain::seal_policy::SealPolicy;
use domain::{ElectionRules, Period};

use crate::chain_view::RevealPolicy;

/// 保存先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Storage {
    /// api プロセスのメモリ（再起動で消える。開発・テスト用）。
    Memory,
    /// ScyllaDB / Cassandra（永続）。スキーマ（`docs/schema.cql`）を投入済みであること。
    Scylla {
        /// 接続先ノード（`host:port`）。
        nodes: Vec<String>,
        keyspace: String,
    },
}

pub struct Config {
    /// `app.mode`（`memory` または `db`）。
    /// `memory` のとき sealer を api プロセス内のタスクとして起動する。`db` のときは起動しない
    /// （独立した sealer プロセスが封印する）。`db` のとき `db.nodes` と `db.keyspace` を使う。
    pub storage: Storage,
    /// `api.port`
    pub port: u16,
    /// `shard.count`。sealer と同じ値にする（食い違うと DB が接続を拒否する）。
    pub shard_count: NonZeroU16,
    /// `election.seed_dir`（選挙データのディレクトリ）と `election.election_id`（読み込む選挙。
    /// `<seed_dir>/<election_id>/` の下の election.toml・districts.csv・candidates/・voters.csv）。
    pub seed_dir: PathBuf,
    pub election_id: String,
    /// エラーメッセージに使う呼び名（`labels.ballot_item`）。
    pub labels: crate::state::ApiLabels,
    /// `session.secret`（秘密情報。必須、16 バイト以上）。全インスタンスで同じ値にする。
    pub session_secret: String,
    /// `session.ttl_secs`
    pub session_ttl_secs: u64,
    /// `auth.mode`（`stub` = 入力 ID をそのまま採用 / `db` = 事前登録したログイン ID とパスワード）。
    pub auth_mode: AuthMode,
    /// `auth.argon2.*`。存在しない ID のダミーの照合に使う（実在する ID の照合と、計算量を同じにする）。
    pub password_params: PasswordParams,
    /// `seal.max_ballots` / `seal.interval_secs` / `seal.min_ballots_after_interval`
    pub seal_policy: SealPolicy,
    /// `sealer.signing_seed`（秘密情報）。`app.mode=memory` のプロセス内 sealer だけが使う（任意。
    /// 未設定なら起動ごとにランダム生成）。`db` では api は署名鍵を持たない（公開鍵は DB から読む）。
    pub sealer_signing_seed: Option<[u8; 32]>,
    /// `chain.reveal_ballots` と `election.voting_closes_at`。ブロックの詳細で、票の中身を返してよいかを決める。
    pub reveal: RevealPolicy,
    /// リクエストのタイムアウト（`api.request_timeout_secs`）。
    pub request_timeout: Duration,
    /// 投票の受付期間（原則18）。`app.mode=memory` では、ここから選挙状態を初期化する
    /// （`app.mode=db` では sealer が初期化するので、api は DB の値を読み直す）。
    pub period: Period,
    /// 締切の手続きの待ち時間（`election.state_cache_secs + api.request_timeout_secs`）。
    pub election_grace: Duration,
    /// 選挙状態の短期キャッシュの秒数。
    pub state_cache_secs: u64,
    /// 画面に表示するタイムゾーン。
    pub display_timezone: DisplayTimezone,
    /// 管理用リスナーの待ち受け先（`host:port`）。
    pub admin_bind: String,
    /// 管理用エンドポイントのトークン（秘密情報）。未設定なら管理用リスナーは起動しない。
    pub admin_token: Option<Secret<String>>,
    /// 選挙のルール（`vote.allow_blank` / `vote.allow_revote` / `vote.max_revotes`）。この api が open に遷移させる
    /// （memory モードの内蔵スケジューラ・`open --now`）ときに固定する値で、固定した後は、選挙状態に保存した値を使う（原則19）。
    pub rules: ElectionRules,
    /// 再投票の鍵のファイル（`<secrets>/revote_key`。ADR 0022）。`None` なら鍵なし（テスト用の読み込み）。
    pub revote_key_path: Option<PathBuf>,
}

// 秘密情報がログに出ないよう、Debug では伏せる。
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("storage", &self.storage)
            .field("port", &self.port)
            .field("shard_count", &self.shard_count)
            .field("seed_dir", &self.seed_dir)
            .field("election_id", &self.election_id)
            .field("labels", &self.labels)
            .field("session_secret", &"<redacted>")
            .field("session_ttl_secs", &self.session_ttl_secs)
            .field("auth_mode", &self.auth_mode)
            .field("seal_policy", &self.seal_policy)
            .field("reveal", &self.reveal)
            .field(
                "sealer_signing_seed",
                &self.sealer_signing_seed.as_ref().map(|_| "<redacted>"),
            )
            .field("request_timeout", &self.request_timeout)
            .field("period", &self.period)
            .field("election_grace", &self.election_grace)
            .field("display_timezone", &self.display_timezone)
            .field("admin_bind", &self.admin_bind)
            .field("rules", &self.rules)
            .field("revote_key_path", &self.revote_key_path)
            .field(
                "admin_token",
                &self.admin_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Config {
    /// 設定（config/*.toml、secrets/、環境変数）を読み込み、この版で実装済みの機能だけを使っていることまで確認する。
    pub fn load() -> anyhow::Result<Self> {
        let loaded = app_config::load()?;
        loaded.ensure_supported()?;
        let mut config = Self::from_app(&loaded.config)?;
        config.revote_key_path = loaded.revote_key_path();
        Ok(config)
    }

    /// 読み込み済みの設定から、api の設定を作る。
    pub fn from_app(app: &AppConfig) -> anyhow::Result<Self> {
        let session_secret = app
            .session
            .secret
            .as_ref()
            .map(|secret| secret.expose().clone())
            .ok_or_else(|| {
                anyhow!(
                    "session.secret が未設定です（環境変数 APP__SESSION__SECRET か secrets/session_secret で、\
                     全インスタンスで共通の 16 バイト以上の値を渡してください）"
                )
            })?;
        let storage = match app.app.mode {
            Mode::Memory => Storage::Memory,
            Mode::Db => Storage::Scylla {
                nodes: app.db.nodes.clone(),
                keyspace: app.db.keyspace.clone(),
            },
        };
        Ok(Self {
            storage,
            port: app.api.port,
            shard_count: app.shard.count,
            seed_dir: app.election.seed_dir.clone(),
            election_id: app.election.election_id.clone(),
            labels: crate::state::ApiLabels {
                ballot_item: app.labels.ballot_item.clone(),
                voting_not_started_message: app.labels.voting_not_started_message.clone(),
                voting_closing_message: app.labels.voting_closing_message.clone(),
                voting_closed_message: app.labels.voting_closed_message.clone(),
                blank_name: app.labels.blank_name.clone(),
                revote_limit_reached: app.labels.revote_limit_reached.clone(),
            },
            session_secret,
            session_ttl_secs: app.session.ttl_secs,
            auth_mode: app.auth.mode,
            password_params: PasswordParams {
                memory_kib: app.auth.argon2.memory_kib,
                iterations: app.auth.argon2.iterations,
                parallelism: app.auth.argon2.parallelism,
            },
            seal_policy: SealPolicy::new(
                usize::try_from(app.seal.max_ballots).context("seal.max_ballots が大きすぎます")?,
                app.seal.interval_secs,
                usize::try_from(app.seal.min_ballots_after_interval)
                    .context("seal.min_ballots_after_interval が大きすぎます")?,
            )
            .context(
                "seal.max_ballots / seal.interval_secs / seal.min_ballots_after_interval が不正です",
            )?,
            sealer_signing_seed: app.sealer.signing_seed.as_ref().map(|seed| *seed.expose()),
            reveal: match app.chain.reveal_ballots {
                RevealBallots::Always => RevealPolicy::Always,
                RevealBallots::AfterClose => RevealPolicy::AfterClose {
                    closes_at_unix: app
                        .election
                        .voting_closes_at
                        .as_ref()
                        .map(|t| t.unix_secs)
                        .ok_or_else(|| {
                            anyhow!("chain.reveal_ballots=after_close には election.voting_closes_at が必要です")
                        })?,
                },
            },
            request_timeout: Duration::from_secs(app.api.request_timeout_secs),
            period: Period {
                opens_at: app.election.voting_opens_at.as_ref().map(|t| t.unix_secs),
                closes_at: app.election.voting_closes_at.as_ref().map(|t| t.unix_secs),
            },
            election_grace: Duration::from_secs(
                app.election.state_cache_secs + app.api.request_timeout_secs,
            ),
            state_cache_secs: app.election.state_cache_secs,
            display_timezone: app.election.display_timezone,
            admin_bind: app.admin.bind.clone(),
            admin_token: app.admin.token.clone(),
            rules: ElectionRules {
                allow_blank: app.vote.allow_blank,
                allow_revote: app.vote.allow_revote,
                max_revotes: app.vote.max_revotes,
            },
            revote_key_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: (&str, &str) = ("session.secret", "0123456789abcdef");

    fn config(pairs: &[(&str, &str)]) -> anyhow::Result<Config> {
        let loaded = app_config::load_for_test(pairs)?;
        loaded.ensure_supported()?;
        Config::from_app(&loaded.config)
    }

    #[test]
    fn defaults_apply_when_only_secret_is_set() {
        let c = config(&[SECRET]).expect("valid");
        assert_eq!(c.port, 18080);
        assert_eq!(c.shard_count.get(), 1);
        assert_eq!(c.seed_dir, PathBuf::from("seed"));
        assert_eq!(c.election_id, "2026-general");
        assert_eq!(c.labels.ballot_item, "投票用紙");
        assert_eq!(c.session_ttl_secs, 3600);
        assert_eq!(c.storage, Storage::Memory);
        assert_eq!(c.seal_policy, SealPolicy::default());
        assert_eq!(c.sealer_signing_seed, None);
        assert_eq!(c.reveal, RevealPolicy::Always);
        assert_eq!(
            c.rules,
            ElectionRules {
                allow_blank: true,
                ..ElectionRules::default()
            }
        );
        assert_eq!(c.labels.blank_name, "白票");
    }

    #[test]
    fn revote_rules_are_read_from_the_config() {
        let c = config(&[
            SECRET,
            ("vote.allow_revote", "true"),
            ("vote.max_revotes", "2"),
        ])
        .expect("valid");
        assert_eq!(
            c.rules,
            ElectionRules {
                allow_blank: true,
                allow_revote: true,
                max_revotes: 2,
            }
        );
        // テスト用の読み込みには secrets/ が無いので、鍵のファイルも無い。
        assert_eq!(c.revote_key_path, None);
    }

    #[test]
    fn allow_blank_is_read_from_the_config() {
        let c = config(&[SECRET, ("vote.allow_blank", "false")]).expect("valid");
        assert_eq!(
            c.rules,
            ElectionRules {
                allow_blank: false,
                ..ElectionRules::default()
            }
        );
    }

    #[test]
    fn after_close_takes_the_close_time_from_the_config() {
        let c = config(&[
            SECRET,
            ("chain.reveal_ballots", "after_close"),
            ("election.voting_closes_at", "2026-10-01T00:00:00Z"),
        ])
        .expect("valid");
        assert_eq!(
            c.reveal,
            RevealPolicy::AfterClose {
                closes_at_unix: 1_790_812_800
            }
        );
        // 締切が無い after_close は、設定の検証で弾かれる。
        assert!(config(&[SECRET, ("chain.reveal_ballots", "after_close")]).is_err());
    }

    #[test]
    fn values_are_read_from_the_config() {
        let c = config(&[
            SECRET,
            ("api.port", "9000"),
            ("shard.count", "2"),
            ("election.seed_dir", "/tmp/e"),
            ("session.ttl_secs", "60"),
        ])
        .expect("valid");
        assert_eq!(
            (c.port, c.shard_count.get(), c.session_ttl_secs),
            (9000, 2, 60)
        );
        assert_eq!(c.seed_dir, PathBuf::from("/tmp/e"));
    }

    #[test]
    fn seal_settings_are_read_and_validated() {
        let c = config(&[
            SECRET,
            ("seal.max_ballots", "7"),
            ("seal.interval_secs", "10"),
            ("app.mode", "memory"),
            (
                "sealer.signing_seed",
                "0101010101010101010101010101010101010101010101010101010101010101",
            ),
        ])
        .expect("valid");
        assert_eq!(c.seal_policy, SealPolicy::new(7, 10, 10).expect("valid"));
        assert_eq!(c.sealer_signing_seed, Some([1u8; 32]));
        // 不正な値は app-config の検証で、出所つきで弾かれる。
        assert!(config(&[SECRET, ("seal.max_ballots", "0")]).is_err());
        assert!(config(&[SECRET, ("seal.interval_secs", "x")]).is_err());
        assert!(config(&[SECRET, ("sealer.signing_seed", "abcd")]).is_err());
    }

    #[test]
    fn db_mode_reads_the_connection_settings() {
        let c = config(&[
            SECRET,
            ("app.mode", "db"),
            ("db.nodes", "db1:9042,db2:9042"),
            ("db.keyspace", "vote_test"),
        ])
        .expect("valid");
        assert_eq!(
            c.storage,
            Storage::Scylla {
                nodes: vec!["db1:9042".to_string(), "db2:9042".to_string()],
                keyspace: "vote_test".to_string(),
            }
        );
        assert!(config(&[SECRET, ("app.mode", "disk")]).is_err());
        assert!(config(&[SECRET, ("app.mode", "db"), ("db.keyspace", "a-b")]).is_err());
    }

    #[test]
    fn missing_secret_is_an_error_that_says_how_to_set_it() {
        let e = config(&[]).expect_err("secret is required");
        let text = e.to_string();
        assert!(
            text.contains("APP__SESSION__SECRET") && text.contains("secrets/session_secret"),
            "{text}"
        );
        assert!(config(&[("session.secret", "short")]).is_err());
    }

    #[test]
    fn voting_opens_at_is_implemented_and_read_into_period() {
        // 原則17・18（ADR 0019）: 投票の開始時刻の制御は実装済み。
        let c = config(&[
            SECRET,
            ("election.voting_opens_at", "2026-10-01T09:00:00+09:00"),
            ("election.voting_closes_at", "2026-10-02T09:00:00+09:00"),
        ])
        .expect("opens_at is supported");
        assert!(c.period.opens_at.is_some());
        assert!(c.period.closes_at.is_some());
    }

    #[test]
    fn db_auth_needs_db_mode_and_reads_the_argon2_parameters() {
        let e = config(&[SECRET, ("auth.mode", "db")]).expect_err("needs db mode");
        assert!(e.to_string().contains("app.mode=db"), "{e}");
        let c = config(&[
            SECRET,
            ("app.mode", "db"),
            ("auth.mode", "db"),
            ("auth.argon2.memory_kib", "64"),
            ("auth.argon2.iterations", "3"),
            ("auth.argon2.parallelism", "2"),
        ])
        .expect("valid");
        assert_eq!(c.auth_mode, AuthMode::Db);
        assert_eq!(
            c.password_params,
            PasswordParams {
                memory_kib: 64,
                iterations: 3,
                parallelism: 2
            }
        );
        assert_eq!(config(&[SECRET]).expect("valid").auth_mode, AuthMode::Stub);
    }

    #[test]
    fn debug_hides_secrets() {
        let c = config(&[
            SECRET,
            (
                "sealer.signing_seed",
                "0202020202020202020202020202020202020202020202020202020202020202",
            ),
        ])
        .expect("valid");
        let debug = format!("{c:?}");
        assert!(!debug.contains("0123456789abcdef"), "{debug}");
        assert!(!debug.contains("0202"), "{debug}");
    }
}
