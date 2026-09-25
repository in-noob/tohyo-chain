//! ログイン ID・パスワードの事前登録（app.mode=db のみ）。
//!
//!   credgen [--reissue] [--confirm-no-output]
//!
//! 有権者は、設定（election.seed_dir / election.election_id）の選挙データの名簿（voters.csv）から読む。
//! 登録先の DB は、設定（db.nodes / db.keyspace）。設定は docs/configuration.md を参照。
//!
//!   --reissue            登録済みの有権者にも、新しいログイン ID とパスワードを発行し直す
//!                        （既定は、登録済みの有権者をスキップする）。古い認証情報は削除される。
//!   --confirm-no-output  credentials.output_file_enabled=false のとき、平文のパスワードが二度と取り出せないことを
//!                        承知して、登録する（指定しない限り、何も処理しない）。
//!
//! 終了コード: 0 = 成功、1 = 失敗、2 = 使い方の誤り／確認が必要（何も処理していない）。

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, bail};
use application::{PasswordParams, SystemClock};
use credgen::{CsvFileSink, NoSink, Options, RecordSink, register};
use infra_scylla::{ScyllaConfig, ScyllaStore};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "使い方: credgen [--reissue] [--confirm-no-output]";

const NO_OUTPUT_WARNING: &str = "警告: credentials.output_file_enabled=false のため、平文のパスワードは出力されません。
DB に残るのはハッシュ（Argon2id）だけなので、**平文のパスワードは二度と取り出せません**（郵送できません）。
郵送するには、credentials.output_file_enabled=true（APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true）にして、
credentials.output_path に CSV を出力してください。出力せずに登録するなら、--confirm-no-output を指定してください。";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("エラー: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let mut reissue = false;
    let mut confirm_no_output = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--reissue" => reissue = true,
            "--confirm-no-output" => confirm_no_output = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            other => {
                eprintln!("不明な引数: {other}\n{USAGE}");
                return Ok(ExitCode::from(2));
            }
        }
    }

    let loaded = app_config::load()?;
    loaded.ensure_supported()?;
    let config = loaded.config;
    if config.app.mode != app_config::Mode::Db {
        bail!(
            "credgen は app.mode=db が必要です（認証情報は DB に登録します。memory モードは、stub 認証のままです）"
        );
    }

    // 出力しない設定のときは、確認が無ければ、何も処理しない（DB にも触れない）。
    let output_enabled = config.credentials.output_file_enabled;
    if !output_enabled {
        eprintln!("{NO_OUTPUT_WARNING}");
        if !confirm_no_output {
            eprintln!("\n--confirm-no-output が指定されていないので、何も処理していません。");
            return Ok(ExitCode::from(2));
        }
    }
    // 出力ファイルは、DB に書き込む前に作る（すでにあるなどで作れないなら、登録する前に失敗する）。
    let mut sink: Box<dyn RecordSink> = if output_enabled {
        Box::new(
            CsvFileSink::create(&config.credentials.output_path).with_context(|| {
                format!("{} を作れません", config.credentials.output_path.display())
            })?,
        )
    } else {
        Box::new(NoSink)
    };

    let data = seed::load(&config.election.seed_dir, &config.election.election_id)
        .context("選挙データの読み込みに失敗しました")?;
    let mut voters: Vec<_> = data
        .voters
        .iter()
        .map(|(voter, districts)| (voter.as_str().to_string(), districts.to_vec()))
        .collect();
    voters.sort();

    let store = Arc::new(
        ScyllaStore::connect(
            &ScyllaConfig {
                nodes: config.db.nodes.clone(),
                keyspace: config.db.keyspace.clone(),
                shard_count: config.shard.count,
            },
            Arc::new(SystemClock),
        )
        .await
        .context("DB への接続に失敗しました（docs/schema.cql を投入済みか確認してください）")?,
    );
    let options = Options {
        reissue,
        login_id_length: usize::try_from(config.credentials.login_id_length)?,
        password_length: usize::try_from(config.credentials.password_length)?,
        params: PasswordParams {
            memory_kib: config.auth.argon2.memory_kib,
            iterations: config.auth.argon2.iterations,
            parallelism: config.auth.argon2.parallelism,
        },
    };

    let total = voters.len();
    let summary = register(store, &data.election, voters, &options, sink.as_mut())
        .await
        .context("登録に失敗しました（途中まで登録されている可能性があります。CSV にない有権者は --reissue で発行し直せます）")?;
    println!(
        "登録: 新規 {}・再発行 {}・スキップ（登録済み）{}（名簿 {total} 人。キースペース {}）",
        summary.created, summary.reissued, summary.skipped, config.db.keyspace
    );
    if output_enabled {
        println!(
            "出力: {}（{} 行。権限 0600。**平文のパスワードを含むので、郵送の準備が済んだら安全に削除してください**）",
            config.credentials.output_path.display(),
            summary.created + summary.reissued
        );
    } else {
        println!("出力: なし（平文のパスワードは二度と取り出せません）");
    }
    Ok(ExitCode::SUCCESS)
}
