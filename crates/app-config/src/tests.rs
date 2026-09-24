//! 層の優先順位・検証・エラーの出所・秘密情報の扱いのテスト。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

/// テスト用の一時ディレクトリ（終了時に消す）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "app-config-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("write file");
        path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sources(config: &TempDir, secrets: &TempDir, env: &[(&str, &str)]) -> Sources {
    Sources {
        config_dir: Some(config.path().to_path_buf()),
        secrets_dir: Some(secrets.path().to_path_buf()),
        env: env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    }
}

fn err_text(result: Result<Loaded, ConfigError>) -> String {
    result.expect_err("should be invalid").to_string()
}

#[test]
fn default_toml_alone_is_valid_and_has_the_documented_defaults() {
    let loaded = load_for_test(&[]).expect("defaults are valid");
    let c = &loaded.config;
    assert_eq!(c.app.env, Env::Dev);
    assert_eq!(c.app.mode, Mode::Memory);
    assert_eq!((c.api.port, c.web.port), (18080, 8080));
    assert_eq!(c.db.backend, DbBackend::Cassandra);
    assert_eq!(c.db.nodes, vec!["127.0.0.1:9042".to_string()]);
    assert_eq!(c.db.keyspace, "vote");
    assert_eq!(
        (
            c.seal.max_ballots,
            c.seal.interval_secs,
            c.seal.min_ballots_after_interval
        ),
        (100, 600, 10)
    );
    assert_eq!(c.sealer.lease_ttl_secs, 30);
    assert_eq!(c.sealer.id, None);
    assert_eq!(c.shard.count.get(), 1);
    assert_eq!(c.auth.mode, AuthMode::Stub);
    assert_eq!(c.session.ttl_secs, 3600);
    assert!(c.session.secret.is_none() && c.sealer.signing_seed.is_none());
    assert_eq!(c.chain.reveal_ballots, RevealBallots::Always);
    assert_eq!(c.labels.done_message, "投票を受け付けました");
    assert_eq!(
        c.election.election_dir(),
        PathBuf::from("seed/2026-general")
    );
    assert_eq!(c.labels.ballot_item, "投票用紙");
    assert_eq!(c.labels.progress, "{total}枚中{current}枚目");
    assert!(c.vote.allow_blank, "白票は既定で選べる");
    assert_eq!(c.labels.blank_option, "白票（どの候補者にも投票しない）");
    assert_eq!(
        c.labels.blank_confirm,
        "白票として投票します。よろしいですか？"
    );
    assert_eq!(c.labels.blank_name, "白票");
    loaded.ensure_supported().expect("defaults are supported");
}

#[test]
fn every_default_key_is_read_into_the_typed_config() {
    // default.toml の項目は、すべて型付きの設定か秘密情報として読まれる（書き忘れ・読み忘れの検出）。
    // 未知の項目としてエラーになるかを、各項目を同じ値で上書きして確かめる。
    let loaded = load_for_test(&[]).expect("valid");
    for key in loaded.entries.keys() {
        let value = loaded.get(key).expect("get");
        load_for_test(&[(key.as_str(), value.as_str())])
            .unwrap_or_else(|e| panic!("{key} を既定値で上書きしても有効なはず: {e}"));
    }
}

#[test]
fn layers_override_in_order_default_env_file_local_secrets_env() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write(
        "dev.toml",
        "[seal]\nmax_ballots = 10\ninterval_secs = 20\n[shard]\ncount = 2\n",
    );
    config.write("local.toml", "[seal]\nmax_ballots = 30\n");
    let loaded = load_from(&sources(
        &config,
        &secrets,
        &[("APP__SEAL__MAX_BALLOTS", "5")],
    ))
    .expect("valid");
    let c = &loaded.config;
    assert_eq!(c.seal.max_ballots, 5, "環境変数が最優先");
    assert_eq!(c.seal.interval_secs, 20, "dev.toml が既定を上書き");
    assert_eq!(c.shard.count.get(), 2);
    assert_eq!(c.api.port, 18080, "どこにも無ければ既定値");
    assert_eq!(
        loaded.origin("seal.max_ballots"),
        Some(&Origin::Env("APP__SEAL__MAX_BALLOTS".to_string()))
    );
    assert_eq!(
        loaded.origin("seal.interval_secs"),
        Some(&Origin::File(config.path().join("dev.toml")))
    );

    // 環境変数が無ければ、local.toml が dev.toml に勝つ。
    let loaded = load_from(&sources(&config, &secrets, &[])).expect("valid");
    assert_eq!(loaded.config.seal.max_ballots, 30);
    assert_eq!(
        loaded.origin("seal.max_ballots"),
        Some(&Origin::File(config.path().join("local.toml")))
    );
}

#[test]
fn app_env_selects_the_environment_file() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write("dev.toml", "[shard]\ncount = 2\n");
    config.write("test.toml", "[shard]\ncount = 3\n");
    let dev = load_from(&sources(&config, &secrets, &[])).expect("valid");
    assert_eq!(dev.config.shard.count.get(), 2);
    let test = load_from(&sources(&config, &secrets, &[("APP__APP__ENV", "test")])).expect("valid");
    assert_eq!(test.config.shard.count.get(), 3);
    // local.toml でも選べる（環境変数が無いとき）。
    config.write("local.toml", "[app]\nenv = \"test\"\n");
    let local = load_from(&sources(&config, &secrets, &[])).expect("valid");
    assert_eq!(local.config.app.env, Env::Test);
    assert_eq!(local.config.shard.count.get(), 3);
}

#[test]
fn production_requires_its_file_and_db_mode() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    let env = [("APP__APP__ENV", "production")];
    let text = err_text(load_from(&sources(&config, &secrets, &env)));
    assert!(
        text.contains("production.toml") && text.contains("必要です"),
        "{text}"
    );

    config.write("production.toml", "[seal]\nmax_ballots = 50\n");
    let text = err_text(load_from(&sources(&config, &secrets, &env)));
    assert!(
        text.contains("app.mode") && text.contains("production では db"),
        "{text}"
    );

    config.write("production.toml", "[app]\nmode = \"db\"\n");
    let loaded = load_from(&sources(&config, &secrets, &env)).expect("valid");
    assert_eq!(loaded.config.app.env, Env::Production);
}

#[test]
fn environment_file_cannot_claim_another_environment() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write("dev.toml", "[app]\nenv = \"production\"\n");
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    assert!(
        text.contains("dev.toml") && text.contains("app.env"),
        "{text}"
    );
}

#[test]
fn invalid_value_reports_file_key_value_and_reason() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write("local.toml", "[seal]\nmax_ballots = 0\n");
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    let local = config.path().join("local.toml").display().to_string();
    assert!(text.contains(&local), "{text}");
    assert!(text.contains("seal.max_ballots = 0"), "{text}");
    assert!(text.contains("1 以上"), "{text}");
}

#[test]
fn invalid_environment_variable_reports_its_name() {
    let text = err_text(load_from(&Sources::overrides(&[(
        "seal.max_ballots",
        "many",
    )])));
    assert!(text.contains("環境変数 APP__SEAL__MAX_BALLOTS"), "{text}");
    assert!(
        text.contains("seal.max_ballots") && text.contains("整数"),
        "{text}"
    );
    let text = err_text(load_for_test(&[("app.mode", "disk")]));
    assert!(text.contains("memory / db"), "{text}");
}

#[test]
fn all_problems_are_reported_at_once() {
    let text = err_text(load_for_test(&[
        ("seal.max_ballots", "0"),
        ("shard.count", "0"),
        ("db.keyspace", "1bad"),
    ]));
    for key in ["seal.max_ballots", "shard.count", "db.keyspace"] {
        assert!(text.contains(key), "{key}: {text}");
    }
    assert!(text.contains("3 件"), "{text}");
}

#[test]
fn unknown_keys_are_rejected_in_files_and_environment() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write("local.toml", "[seal]\nmax_ballot = 5\n");
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    assert!(
        text.contains("seal.max_ballot") && text.contains("未知の項目"),
        "{text}"
    );
    let text = err_text(load_for_test(&[("seal.nope", "1")]));
    assert!(
        text.contains("APP__SEAL__NOPE") && text.contains("未知の項目"),
        "{text}"
    );
}

#[test]
fn wrong_types_in_files_are_rejected() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write(
        "local.toml",
        "[api]\nport = \"80\"\n[db]\nnodes = \"a:1\"\n",
    );
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    assert!(text.contains("api.port") && text.contains("整数"), "{text}");
    assert!(text.contains("db.nodes") && text.contains("配列"), "{text}");
}

#[test]
fn toml_syntax_errors_name_the_file() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write("local.toml", "[seal\nmax_ballots = 5\n");
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    assert!(
        text.contains("local.toml") && text.contains("TOML"),
        "{text}"
    );
}

#[test]
fn secrets_in_config_files_are_rejected_without_echoing_the_value() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    config.write(
        "local.toml",
        "[session]\nsecret = \"file-secret-value-0123456789\"\n",
    );
    let text = err_text(load_from(&sources(&config, &secrets, &[])));
    assert!(
        text.contains("session.secret") && text.contains("APP__SESSION__SECRET"),
        "{text}"
    );
    assert!(!text.contains("file-secret-value"), "{text}");
}

#[test]
fn secrets_come_from_env_or_secrets_dir_and_env_wins() {
    let (config, secrets) = (TempDir::new(), TempDir::new());
    secrets.write("session_secret", "from-secrets-dir-0123456789\n");
    secrets.write("sealer_signing_seed", SEED);
    let loaded = load_from(&sources(&config, &secrets, &[])).expect("valid");
    assert_eq!(
        loaded
            .config
            .session
            .secret
            .as_ref()
            .map(|s| s.expose().as_str()),
        Some("from-secrets-dir-0123456789"),
        "末尾の改行は取り除く"
    );
    assert_eq!(
        loaded
            .config
            .sealer
            .signing_seed
            .as_ref()
            .map(|s| *s.expose()),
        Some([7u8; 32])
    );
    assert!(matches!(
        loaded.origin("session.secret"),
        Some(Origin::SecretFile(_))
    ));

    let loaded = load_from(&sources(
        &config,
        &secrets,
        &[("APP__SESSION__SECRET", "from-env-secret-0123456789")],
    ))
    .expect("valid");
    assert_eq!(
        loaded
            .config
            .session
            .secret
            .as_ref()
            .map(|s| s.expose().as_str()),
        Some("from-env-secret-0123456789")
    );
    assert!(matches!(
        loaded.origin("session.secret"),
        Some(Origin::Env(_))
    ));
}

#[test]
fn invalid_secrets_are_reported_without_the_value() {
    let text = err_text(load_for_test(&[("session.secret", "short-value")]));
    assert!(
        text.contains("session.secret") && text.contains("16 バイト以上"),
        "{text}"
    );
    assert!(!text.contains("short-value"), "{text}");
    let text = err_text(load_for_test(&[("sealer.signing_seed", "zz-not-hex")]));
    assert!(
        text.contains("sealer.signing_seed") && text.contains("64 桁"),
        "{text}"
    );
    assert!(!text.contains("zz-not-hex"), "{text}");
}

#[test]
fn show_and_get_never_reveal_secrets() {
    let loaded = load_for_test(&[
        ("session.secret", "super-secret-session-value"),
        ("sealer.signing_seed", SEED),
    ])
    .expect("valid");
    let shown = loaded.render();
    assert!(!shown.contains("super-secret-session-value"), "{shown}");
    assert!(!shown.contains(SEED), "{shown}");
    assert!(shown.contains("secret = \"***\""), "{shown}");
    assert!(shown.contains("signing_seed = \"***\""), "{shown}");
    assert!(loaded.get("session.secret").is_err());
    assert!(loaded.get("sealer.signing_seed").is_err());
    assert!(!format!("{:?}", loaded.config).contains("super-secret-session-value"));
    assert!(!format!("{:?}", loaded.config).contains("0707"));
    // 未設定の秘密情報も行を出す。
    let unset = load_for_test(&[]).expect("valid").render();
    assert!(unset.contains("secret = （未設定）"), "{unset}");
}

#[test]
fn render_shows_the_origin_of_each_value() {
    let loaded = load_for_test(&[("seal.max_ballots", "7")]).expect("valid");
    let shown = loaded.render();
    assert!(
        shown.contains("max_ballots = 7  # 環境変数 APP__SEAL__MAX_BALLOTS"),
        "{shown}"
    );
    assert!(
        shown.contains("port = 18080  # config/default.toml（既定値）"),
        "{shown}"
    );
}

#[test]
fn get_formats_values_for_scripts() {
    let loaded = load_for_test(&[("db.nodes", "a:1, b:2")]).expect("valid");
    assert_eq!(loaded.get("api.port").as_deref(), Ok("18080"));
    assert_eq!(loaded.get("db.nodes").as_deref(), Ok("a:1,b:2"));
    assert_eq!(
        loaded.get("credentials.output_file_enabled").as_deref(),
        Ok("false")
    );
    assert_eq!(loaded.get("election.seed_dir").as_deref(), Ok("seed"));
    assert!(loaded.get("no.such").is_err());
}

#[test]
fn field_validation_rules() {
    for (key, value, needle) in [
        ("api.port", "0", "1 以上"),
        ("api.port", "70000", "65535"),
        ("web.port", "18080", "同じポート"),
        ("db.backend", "mysql", "cassandra / scylla"),
        ("db.nodes", "", "1 件以上"),
        ("db.nodes", "nohost", "host:port"),
        ("db.nodes", "h:99999", "port"),
        ("db.keyspace", "a-b", "英字"),
        ("db.keyspace", &"k".repeat(49), "48"),
        ("seal.interval_secs", "0", "1 以上"),
        ("seal.min_ballots_after_interval", "0", "1 以上"),
        ("sealer.lease_ttl_secs", "2", "3 以上"),
        ("auth.mode", "ldap", "stub / db"),
        ("auth.argon2.memory_kib", "4", "8 以上"),
        ("auth.argon2.iterations", "0", "1 以上"),
        ("auth.argon2.parallelism", "0", "1 以上"),
        ("session.ttl_secs", "0", "1 以上"),
        ("credentials.password_length", "7", "8 以上"),
        ("credentials.password_length", "129", "128"),
        ("election.seed_dir", " ", "空"),
        ("election.voting_opens_at", "tomorrow", "RFC 3339"),
        ("chain.reveal_ballots", "never", "always / after_close"),
        ("labels.site_title", "", "空"),
        ("labels.ballot_item", "", "空"),
        ("labels.done_message", &"あ".repeat(201), "200"),
    ] {
        let text = err_text(load_for_test(&[(key, value)]));
        assert!(
            text.contains(key) && text.contains(needle),
            "{key}={value}: {text}"
        );
    }
    // 上限側は文字数（バイト数ではない）。
    load_for_test(&[("labels.done_message", &"あ".repeat(200))]).expect("200 chars ok");
    load_for_test(&[("db.keyspace", "vote_s6_1789912345_123456")]).expect("valid keyspace");
    load_for_test(&[("db.nodes", "db1:9042,db2:9042")]).expect("two nodes");
}

#[test]
fn argon2_memory_must_cover_parallelism() {
    let text = err_text(load_for_test(&[
        ("auth.argon2.memory_kib", "64"),
        ("auth.argon2.parallelism", "9"),
    ]));
    assert!(text.contains("8 × auth.argon2.parallelism"), "{text}");
}

#[test]
fn voting_window_is_validated_and_datetime_literals_are_accepted() {
    let text = err_text(load_for_test(&[
        ("election.voting_opens_at", "2026-10-02T00:00:00Z"),
        ("election.voting_closes_at", "2026-10-01T00:00:00Z"),
    ]));
    assert!(
        text.contains("election.voting_closes_at") && text.contains("より後"),
        "{text}"
    );

    let (config, secrets) = (TempDir::new(), TempDir::new());
    // TOML の日時リテラル（引用符なし）も受け付ける。
    config.write(
        "local.toml",
        "[election]\nvoting_opens_at = 2026-10-01T09:00:00+09:00\nvoting_closes_at = \"2026-10-02T09:00:00+09:00\"\n",
    );
    let loaded = load_from(&sources(&config, &secrets, &[])).expect("valid");
    let opens = loaded
        .config
        .election
        .voting_opens_at
        .as_ref()
        .expect("opens");
    let closes = loaded
        .config
        .election
        .voting_closes_at
        .as_ref()
        .expect("closes");
    assert_eq!(closes.unix_secs - opens.unix_secs, 86_400);
}

#[test]
fn reveal_after_close_requires_a_closing_time() {
    let text = err_text(load_for_test(&[("chain.reveal_ballots", "after_close")]));
    assert!(text.contains("voting_closes_at"), "{text}");
}

#[test]
fn voting_opens_at_is_implemented_and_supported() {
    // 原則17・18（ADR 0019）: 投票の開始時刻の制御は実装済みなので、指定してもエラーにならない。
    let pairs = vec![("election.voting_opens_at", "2026-10-01T09:00:00+09:00")];
    let loaded = load_for_test(&pairs).expect("format is valid");
    loaded.ensure_supported().expect("opens_at is supported");
}

#[test]
fn reveal_after_close_is_supported_and_needs_a_close_time() {
    // 締切後にだけ票の中身を公開する（ブロックチェーンのビューア）。締切が無い after_close は、設定の検証で弾く。
    let loaded = load_for_test(&[
        ("chain.reveal_ballots", "after_close"),
        ("election.voting_closes_at", "2026-10-01T09:00:00+09:00"),
    ])
    .expect("valid");
    loaded.ensure_supported().expect("after_close is supported");
    assert_eq!(
        loaded.config.chain.reveal_ballots,
        RevealBallots::AfterClose
    );
    let text = err_text(load_for_test(&[("chain.reveal_ballots", "after_close")]));
    assert!(
        text.contains("chain.reveal_ballots") && text.contains("voting_closes_at"),
        "{text}"
    );
}

#[test]
fn voting_closes_at_is_supported_for_tally() {
    // 締切は、verifier tally が「締切後の集計か」を判定するのに使う（api / sealer は使わないが、受け付ける）。
    let loaded = load_for_test(&[("election.voting_closes_at", "2026-10-01T09:00:00+09:00")])
        .expect("valid");
    loaded.ensure_supported().expect("closes_at is supported");
    let closes = loaded.config.election.voting_closes_at.expect("set");
    assert_eq!(closes.text, "2026-10-01T09:00:00+09:00");
    assert_eq!(closes.unix_secs, 1_790_812_800);
}

#[test]
fn db_auth_requires_db_mode_and_credentials_settings_are_validated() {
    // DB 認証は、DB に登録した認証情報を使うので、memory モードとは組み合わせられない。
    let text = err_text(load_for_test(&[("auth.mode", "db")]));
    assert!(
        text.contains("auth.mode") && text.contains("app.mode=db"),
        "{text}"
    );
    let loaded = load_for_test(&[("auth.mode", "db"), ("app.mode", "db")]).expect("valid");
    loaded.ensure_supported().expect("db auth is implemented");
    assert_eq!(loaded.config.auth.mode, AuthMode::Db);
    // 出力する設定も、実装済み。
    load_for_test(&[("credentials.output_file_enabled", "true")])
        .expect("valid")
        .ensure_supported()
        .expect("output is implemented");
    for (key, value, needle) in [
        ("credentials.login_id_length", "7", "8 以上"),
        ("credentials.login_id_length", "33", "32"),
        ("credentials.password_length", "7", "8 以上"),
    ] {
        let text = err_text(load_for_test(&[(key, value)]));
        assert!(text.contains(key) && text.contains(needle), "{key}: {text}");
    }
    assert_eq!(
        load_for_test(&[])
            .expect("valid")
            .config
            .credentials
            .login_id_length,
        10
    );
}

#[test]
fn web_env_exports_quoted_labels() {
    let loaded = load_for_test(&[("labels.site_title", "It's a \"test\" 選挙")]).expect("valid");
    let out = loaded.web_env();
    assert!(
        out.contains("export APP_WEB_SITE_TITLE='It'\\''s a \"test\" 選挙'\n"),
        "{out}"
    );
    assert!(
        out.contains("export APP_WEB_DONE_MESSAGE='投票を受け付けました'\n"),
        "{out}"
    );
    assert!(
        out.contains("export APP_WEB_LOGIN_HEADING='ログイン'\n"),
        "{out}"
    );
    assert!(
        out.contains("export APP_WEB_BALLOT_ITEM='投票用紙'\n"),
        "{out}"
    );
    assert!(
        out.contains("export APP_WEB_PROGRESS='{total}枚中{current}枚目'\n"),
        "{out}"
    );
    for (name, value) in [
        ("APP_WEB_BLANK_OPTION", "白票（どの候補者にも投票しない）"),
        (
            "APP_WEB_BLANK_CONFIRM",
            "白票として投票します。よろしいですか？",
        ),
        ("APP_WEB_BLANK_NAME", "白票"),
    ] {
        assert!(out.contains(&format!("export {name}='{value}'\n")), "{out}");
    }
    // vote.allow_blank は、ビルド時ではなく実行時に API から受け取る（open の時点で固定するため）。
    assert!(!out.contains("ALLOW_BLANK"), "{out}");
}

#[test]
fn allow_blank_can_be_turned_off_and_must_be_a_boolean() {
    let loaded = load_for_test(&[("vote.allow_blank", "false")]).expect("valid");
    assert!(!loaded.config.vote.allow_blank);
    let text = err_text(load_for_test(&[("vote.allow_blank", "maybe")]));
    assert!(text.contains("vote.allow_blank"), "{text}");
    for key in [
        "labels.blank_option",
        "labels.blank_confirm",
        "labels.blank_name",
    ] {
        let text = err_text(load_for_test(&[(key, " ")]));
        assert!(text.contains(key) && text.contains("空"), "{text}");
    }
}

#[test]
fn progress_label_must_have_both_placeholders_and_election_id_is_validated() {
    let text = err_text(load_for_test(&[("labels.progress", "{total}枚")]));
    assert!(
        text.contains("labels.progress") && text.contains("{current}"),
        "{text}"
    );
    let text = err_text(load_for_test(&[("labels.progress", "{current}枚目")]));
    assert!(
        text.contains("labels.progress") && text.contains("{total}"),
        "{text}"
    );
    load_for_test(&[("labels.progress", "全{total}枚のうち{current}枚目")]).expect("valid");
    for bad in ["", "2026 general", "Général", "-x", &"a".repeat(33)] {
        let text = err_text(load_for_test(&[("election.election_id", bad)]));
        assert!(text.contains("election.election_id"), "{bad:?}: {text}");
    }
    load_for_test(&[("election.election_id", "2027-local_1")]).expect("valid");
}

#[test]
fn env_name_maps_keys_to_variables() {
    assert_eq!(env_name("seal.max_ballots"), "APP__SEAL__MAX_BALLOTS");
    assert_eq!(
        env_name("auth.argon2.memory_kib"),
        "APP__AUTH__ARGON2__MEMORY_KIB"
    );
    assert_eq!(env_name("session.secret"), "APP__SESSION__SECRET");
}

#[test]
fn shipped_config_files_are_valid() {
    // リポジトリの config/dev.toml と production.example.toml が、読み込めること。
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let secrets = TempDir::new();
    let dev = Sources {
        config_dir: Some(root.clone()),
        secrets_dir: Some(secrets.path().to_path_buf()),
        env: Vec::new(),
    };
    // local.toml が開発者の手元にあっても影響しないよう、ファイルの層は一時ディレクトリにコピーして確かめる。
    let config = TempDir::new();
    config.write(
        "dev.toml",
        &std::fs::read_to_string(root.join("dev.toml")).expect("read"),
    );
    let dev = Sources {
        config_dir: Some(config.path().to_path_buf()),
        ..dev
    };
    let loaded = load_from(&dev).expect("dev.toml is valid");
    assert_eq!(
        (
            loaded.config.seal.interval_secs,
            loaded.config.seal.min_ballots_after_interval
        ),
        (10, 10)
    );

    config.write(
        "production.toml",
        &std::fs::read_to_string(root.join("production.example.toml")).expect("read"),
    );
    let prod = Sources {
        env: vec![("APP__APP__ENV".to_string(), "production".to_string())],
        ..dev
    };
    let loaded = load_from(&prod).expect("production.example.toml is valid");
    assert_eq!(loaded.config.app.mode, Mode::Db);
    assert_eq!(loaded.config.shard.count.get(), 8);
}
