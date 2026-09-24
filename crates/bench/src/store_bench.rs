//! DB 直接の計測: HTTP（api）を介さず、`ScyllaStore::cast`（participation の LWT + 票のプールへの INSERT）を
//! 同時実行数を固定して連続で呼ぶ。api のオーバーヘッドと DB 側（LWT）の切り分けに使う。
//!
//! 専用のキースペース（`docs/schema.cql` を `infra_scylla::schema` で描画して適用）を作って計測し、終わったら削除する。
//! 実行中は sealer を動かさない（プールに票が溜まる）。

use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use application::{CastError, SystemClock, VoteStore};
use domain::{Ballot, BallotId, shard_for};
use infra_scylla::{ScyllaConfig, ScyllaStore};
use scylla::client::session_builder::SessionBuilder;
use serde_json::json;

use crate::stats::percentiles;
use crate::workload::Workload;

#[derive(Debug, Clone)]
pub struct StoreArgs {
    /// 投票計画（選挙データの有権者と、その有権者の投票用紙から作る）。
    pub plan: Arc<Workload>,
    pub nodes: Vec<String>,
    pub shard_count: NonZeroU16,
    pub concurrency: usize,
    pub duration: Duration,
    pub out: PathBuf,
}

#[derive(Debug, Default)]
struct WorkerResult {
    latencies_us: Vec<u64>,
    accepted: u64,
    duplicates: u64,
    errors: u64,
}

pub async fn run(args: StoreArgs) -> anyhow::Result<()> {
    let admin = SessionBuilder::new()
        .known_nodes(&args.nodes)
        .build()
        .await
        .context("DB に接続できません")?;
    // 接頭辞（環境変数 BENCH_KEYSPACE_PREFIX。既定 `bench`）で始まる専用のキースペース。共用の `vote` には触れない。
    let prefix = std::env::var("BENCH_KEYSPACE_PREFIX").unwrap_or_else(|_| "bench".to_string());
    let keyspace = format!(
        "{prefix}_store_{:012x}",
        rand::random::<u64>() & 0xffff_ffff_ffff
    );
    for statement in infra_scylla::schema::statements(&keyspace) {
        admin
            .query_unpaged(statement.as_str(), &[])
            .await
            .with_context(|| format!("スキーマの適用に失敗しました: {statement}"))?;
    }

    let outcome = measure(&args, &keyspace).await;
    // 計測用のキースペースは、成否にかかわらず削除する。
    let _ = admin
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), &[])
        .await;
    outcome
}

async fn measure(args: &StoreArgs, keyspace: &str) -> anyhow::Result<()> {
    let store = Arc::new(
        ScyllaStore::connect(
            &ScyllaConfig {
                nodes: args.nodes.clone(),
                keyspace: keyspace.to_string(),
                shard_count: args.shard_count,
            },
            Arc::new(SystemClock),
        )
        .await
        .context("ScyllaStore に接続できません")?,
    );
    let next = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let stop_at = t0 + args.duration;

    let tasks: Vec<_> = (0..args.concurrency)
        .map(|_| {
            let (store, next, plan) = (store.clone(), next.clone(), args.plan.clone());
            let shards = args.shard_count;
            tokio::spawn(async move {
                let mut result = WorkerResult::default();
                while Instant::now() < stop_at {
                    // 投票計画が尽きたら、止める（有権者数を増やして seedgen で作り直す）。
                    let Some(item) = plan.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let ballot_id = BallotId::from_random_bytes(rand::random());
                    let ballot = Ballot {
                        ballot_id,
                        contest_id: item.contest.clone(),
                        candidate_id: item.candidate.clone(),
                        revote: None,
                    };
                    let shard = shard_for(&ballot_id, shards);
                    let started = Instant::now();
                    let outcome = store.cast(&item.voter, shard, ballot).await;
                    result
                        .latencies_us
                        .push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
                    match outcome {
                        Ok(()) => result.accepted += 1,
                        Err(CastError::AlreadyVoted) => result.duplicates += 1,
                        Err(_) => result.errors += 1,
                    }
                }
                result
            })
        })
        .collect();

    let mut latencies = Vec::new();
    let (mut accepted, mut duplicates, mut errors) = (0u64, 0u64, 0u64);
    for task in tasks {
        let r = task.await.context("計測タスクが失敗しました")?;
        latencies.extend(r.latencies_us);
        accepted += r.accepted;
        duplicates += r.duplicates;
        errors += r.errors;
    }
    let elapsed = t0.elapsed();
    let stats = percentiles(&mut latencies);

    let result = json!({
        "kind": "store",
        "shard_count": args.shard_count.get(),
        "concurrency": args.concurrency,
        "duration_s": args.duration.as_secs_f64(),
        "elapsed_s": elapsed.as_secs_f64(),
        "accepted": accepted,
        "duplicates": duplicates,
        "errors": errors,
        "throughput_rps": accepted as f64 / elapsed.as_secs_f64(),
        "latency_us": stats.as_ref().map(|p| p.to_json()),
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&result)?)
        .with_context(|| format!("{} に書き込めません", args.out.display()))?;
    println!(
        "store shards={}: {} 件 / {:.1} 件/秒 / p50={:.1}ms p99={:.1}ms（重複 {duplicates}・エラー {errors}）",
        args.shard_count,
        accepted,
        accepted as f64 / elapsed.as_secs_f64(),
        stats.as_ref().map_or(0.0, |p| p.p50 as f64 / 1000.0),
        stats.as_ref().map_or(0.0, |p| p.p99 as f64 / 1000.0),
    );
    Ok(())
}
