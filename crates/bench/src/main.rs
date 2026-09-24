//! 性能計測ツール。
//!
//!   bench load  --targets URL,URL --mode saturate|rate --concurrency N --duration SECS
//!               [--rate RPS] [--start-index N] --label NAME --out FILE.json
//!   bench store --concurrency N --duration SECS --out FILE.json
//!
//! 投票する有権者と投票用紙は、選挙データ（seed。設定 election.seed_dir / election.election_id）の名簿から作る
//! （api は、名簿にある有権者の、属する選挙区の投票用紙にしか投票させないため）。有権者数が足りず、投票計画が
//! 尽きたら、そこで止める（結果の plan_exhausted）。seedgen の --voters を増やして作り直す。
//! 接続先（db.nodes）・シャード数（shard.count）・セッション署名鍵（session.secret）は設定（app-config）から読む
//! （config/*.toml、secrets/、環境変数 APP__…）。`--nodes` / `--shard-count` で、`store` だけ上書きできる。
//!   bench summarize --dir RUN_DIR          （RUN_DIR/summary.json を作る）
//!   bench report --root RESULTS_DIR         （構成の比較表を Markdown で標準出力へ）
//!
//! 手順の全体は scripts/bench.sh と docs/benchmark.md を参照。

mod load;
mod report;
mod stats;
mod store_bench;
mod timeparse;
mod workload;

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};

/// `--key value` の組を読む。
fn parse_flags(args: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut flags = HashMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let key = flag
            .strip_prefix("--")
            .with_context(|| format!("不明な引数: {flag}"))?;
        let value = it
            .next()
            .with_context(|| format!("--{key} には値が必要です"))?;
        flags.insert(key.to_string(), value.clone());
    }
    Ok(flags)
}

/// 設定（config/*.toml、secrets/、環境変数）を読む。
fn load_config() -> anyhow::Result<app_config::AppConfig> {
    Ok(app_config::load()?.config)
}

/// 選挙データ（seed）の有権者と投票用紙から、投票計画を作る。
fn load_plan() -> anyhow::Result<Arc<workload::Workload>> {
    let config = load_config()?;
    let data = seed::load(&config.election.seed_dir, &config.election.election_id)
        .context("選挙データを読み込めません")?;
    let plan = workload::Workload::from_seed(&data);
    if plan.is_empty() {
        bail!("投票計画が空です（名簿に有権者がいません）");
    }
    eprintln!(
        "投票計画: 有権者 {} 人・投票 {} 件（{}）",
        data.voters.len(),
        plan.len(),
        config.election.election_dir().display()
    );
    Ok(Arc::new(plan))
}

/// 負荷をかける対象の api と同じ、セッション署名鍵（`session.secret`。秘密情報）。
fn session_secret() -> anyhow::Result<String> {
    load_config()?
        .session
        .secret
        .map(|secret| secret.expose().clone())
        .context(
            "session.secret が未設定です（環境変数 APP__SESSION__SECRET か secrets/session_secret。対象の api と同じ値）",
        )
}

fn required<'a>(flags: &'a HashMap<String, String>, key: &str) -> anyhow::Result<&'a str> {
    flags
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("--{key} が必要です"))
}

fn parsed<T: std::str::FromStr>(
    flags: &HashMap<String, String>,
    key: &str,
    default: T,
) -> anyhow::Result<T> {
    match flags.get(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .map_err(|_| anyhow::anyhow!("--{key} が不正です: {raw:?}")),
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        bail!("使い方: bench load|store|summarize|report ...（先頭のコメントを参照）");
    };
    let flags = parse_flags(rest)?;

    match command.as_str() {
        "load" => {
            let mode = match required(&flags, "mode")? {
                "saturate" => load::Mode::Saturate,
                "rate" => load::Mode::Rate,
                other => bail!("--mode は saturate / rate のいずれか: {other:?}"),
            };
            load::run(load::LoadArgs {
                plan: load_plan()?,
                targets: required(&flags, "targets")?
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
                secret: session_secret()?,
                mode,
                concurrency: parsed(&flags, "concurrency", 128)?,
                duration: Duration::from_secs_f64(parsed(&flags, "duration", 60.0)?),
                rate: parsed(&flags, "rate", 0.0)?,
                start_index: parsed(&flags, "start-index", 0)?,
                label: flags
                    .get("label")
                    .cloned()
                    .unwrap_or_else(|| "load".to_string()),
                out: PathBuf::from(required(&flags, "out")?),
            })
        }
        "store" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("tokio を初期化できません")?;
            runtime.block_on(store_bench::run(store_bench::StoreArgs {
                plan: load_plan()?,
                nodes: match flags.get("nodes") {
                    Some(nodes) => nodes
                        .split(',')
                        .map(|n| n.trim().to_string())
                        .filter(|n| !n.is_empty())
                        .collect(),
                    None => load_config()?.db.nodes,
                },
                shard_count: match flags.get("shard-count") {
                    Some(raw) => NonZeroU16::new(
                        raw.parse()
                            .map_err(|_| anyhow::anyhow!("--shard-count が不正です: {raw:?}"))?,
                    )
                    .context("--shard-count は 1 以上が必要です")?,
                    None => load_config()?.shard.count,
                },
                concurrency: parsed(&flags, "concurrency", 128)?,
                duration: Duration::from_secs_f64(parsed(&flags, "duration", 30.0)?),
                out: PathBuf::from(required(&flags, "out")?),
            }))
        }
        "summarize" => {
            let dir = PathBuf::from(required(&flags, "dir")?);
            let summary = report::summarize_run(&dir)?;
            std::fs::write(
                dir.join("summary.json"),
                serde_json::to_string_pretty(&summary)?,
            )?;
            println!("{} を作成しました", dir.join("summary.json").display());
            Ok(())
        }
        "report" => {
            print!(
                "{}",
                report::render_markdown(&PathBuf::from(required(&flags, "root")?))?
            );
            Ok(())
        }
        other => bail!("不明なサブコマンド: {other}"),
    }
}
