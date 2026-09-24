//! HTTP の投票負荷。
//!
//! - `saturate`（クローズドループ）: 同時接続数を固定し、応答が返り次第、次の投票を送る。最大スループットを測る。
//! - `rate`（オープンループ）: 一定のレート（req/s）で送る。**送信の予定時刻**からの経過を遅延として測るので、
//!   サーバが遅れて送信が遅れても、その待ち時間が過小評価されない（coordinated omission の回避）。
//!
//! 各接続（スレッド）は 1 つの api に張りっぱなし（keep-alive）。複数の api には、スレッドごとに
//! ラウンドロビンで割り当てる。トークンは `session.secret`（設定）で `SessionSigner` により発行する
//! （ログイン API の負荷を計測に混ぜないため）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use application::SessionSigner;
use serde_json::{Value, json};

use crate::stats::percentiles;
use crate::workload::Workload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Saturate,
    Rate,
}

#[derive(Debug, Clone)]
pub struct LoadArgs {
    /// 投票計画（選挙データの有権者と、その有権者の投票用紙から作る）。
    pub plan: Arc<Workload>,
    pub targets: Vec<String>,
    pub secret: String,
    pub mode: Mode,
    pub concurrency: usize,
    pub duration: Duration,
    /// `Mode::Rate` のときの目標レート（req/s）。
    pub rate: f64,
    pub start_index: u64,
    pub label: String,
    pub out: PathBuf,
}

/// 1 リクエストの結果。
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// 実際に送信を始めた時刻（開始からのマイクロ秒）。
    #[allow(dead_code)]
    sent_us: u64,
    /// 送信から応答までのマイクロ秒。
    service_us: u64,
    /// 送信の予定時刻から応答までのマイクロ秒（`rate` のとき。`saturate` では service と同じ）。
    from_schedule_us: u64,
    /// HTTP ステータス。0 は通信エラー。
    status: u16,
    /// 応答を受け取った UNIX ミリ秒。
    done_unix_ms: u64,
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn duration_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// 1 票を送り、ステータス（通信エラーは 0）を返す。
fn send_vote(agent: &ureq::Agent, base: &str, token: &str, contest: &str, candidate: &str) -> u16 {
    // 投票用紙の ID は `{election_id}/{district_id}`（パスの 2 つのセグメントになる）。
    let url = format!("{base}/api/v1/contests/{contest}/vote");
    let body = json!({ "candidate_id": candidate }).to_string();
    match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.as_str())
    {
        Ok(mut response) => {
            let status = response.status().as_u16();
            // 接続を再利用するため、本文を読み切る。
            let _ = response.body_mut().read_to_vec();
            status
        }
        Err(_) => 0,
    }
}

struct Shared {
    args: LoadArgs,
    signer: SessionSigner,
    /// 次に使う投票の番号（`start_index` から）。
    next: AtomicU64,
    /// `rate` のとき、送信済みのスロット数。
    slot: AtomicU64,
    /// 投票計画が尽きた。
    exhausted: std::sync::atomic::AtomicBool,
    t0: Instant,
}

fn worker(shared: &Shared, thread_no: usize) -> Vec<Sample> {
    let args = &shared.args;
    let base = args.targets[thread_no % args.targets.len()]
        .trim_end_matches('/')
        .to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let stop_at = shared.t0 + args.duration;
    let total_slots = (args.rate * args.duration.as_secs_f64()) as u64;
    let mut samples = Vec::new();

    loop {
        // 送る票の割り当てと、送信の予定時刻。
        let (index, scheduled) = match args.mode {
            Mode::Saturate => {
                if Instant::now() >= stop_at {
                    break;
                }
                (shared.next.fetch_add(1, Ordering::Relaxed), None)
            }
            Mode::Rate => {
                let k = shared.slot.fetch_add(1, Ordering::Relaxed);
                if k >= total_slots {
                    break;
                }
                let due = shared.t0 + Duration::from_secs_f64(k as f64 / args.rate);
                (args.start_index + k, Some(due))
            }
        };
        if let Some(due) = scheduled {
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            }
        }

        // 投票計画が尽きた（有権者数が足りない）: これ以上は送らない。
        let Some(item) = args.plan.get(index) else {
            shared.exhausted.store(true, Ordering::Relaxed);
            break;
        };
        let token = shared.signer.issue(&item.voter, unix_ms_now() / 1000).token;

        let sent = Instant::now();
        let status = send_vote(
            &agent,
            &base,
            &token,
            item.contest.as_str(),
            item.candidate.as_str(),
        );
        let done = Instant::now();
        samples.push(Sample {
            sent_us: duration_us(sent - shared.t0),
            service_us: duration_us(done - sent),
            from_schedule_us: duration_us(done - scheduled.unwrap_or(sent)),
            status,
            done_unix_ms: unix_ms_now(),
        });
    }
    samples
}

pub fn run(args: LoadArgs) -> anyhow::Result<()> {
    if args.targets.is_empty() {
        bail!("--targets が空です");
    }
    if args.concurrency == 0 {
        bail!("--concurrency は 1 以上が必要です");
    }
    if args.mode == Mode::Rate && !(args.rate.is_finite() && args.rate > 0.0) {
        bail!("--mode rate では --rate（req/s）が必要です");
    }
    let signer = SessionSigner::new(args.secret.as_bytes(), 86_400)
        .context("session.secret が不正です（16 バイト以上が必要）")?;

    let start_unix_ms = unix_ms_now();
    let shared = Arc::new(Shared {
        next: AtomicU64::new(args.start_index),
        slot: AtomicU64::new(0),
        exhausted: std::sync::atomic::AtomicBool::new(false),
        t0: Instant::now(),
        signer,
        args: args.clone(),
    });
    let collected: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..args.concurrency)
        .map(|n| {
            let (shared, collected) = (shared.clone(), collected.clone());
            thread::spawn(move || {
                let samples = worker(&shared, n);
                collected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(samples);
            })
        })
        .collect();
    for h in handles {
        h.join()
            .map_err(|_| anyhow::anyhow!("ワーカースレッドが panic しました"))?;
    }
    let elapsed = shared.t0.elapsed();
    let end_unix_ms = unix_ms_now();
    let samples = std::mem::take(&mut *collected.lock().unwrap_or_else(|e| e.into_inner()));

    let ok: Vec<&Sample> = samples.iter().filter(|s| s.status == 201).collect();
    let mut by_status = std::collections::BTreeMap::<u16, u64>::new();
    for s in &samples {
        *by_status.entry(s.status).or_default() += 1;
    }
    let mut service: Vec<u64> = ok.iter().map(|s| s.service_us).collect();
    let mut from_schedule: Vec<u64> = ok.iter().map(|s| s.from_schedule_us).collect();
    let next_index = match args.mode {
        Mode::Saturate => shared.next.load(Ordering::Relaxed),
        Mode::Rate => args.start_index + (args.rate * args.duration.as_secs_f64()) as u64,
    };

    let result = json!({
        "label": args.label,
        "mode": match args.mode { Mode::Saturate => "saturate", Mode::Rate => "rate" },
        "targets": args.targets,
        "concurrency": args.concurrency,
        "duration_s": args.duration.as_secs_f64(),
        "elapsed_s": elapsed.as_secs_f64(),
        "target_rate": if args.mode == Mode::Rate { json!(args.rate) } else { Value::Null },
        "start_unix_ms": start_unix_ms,
        "end_unix_ms": end_unix_ms,
        "start_index": args.start_index,
        "next_index": next_index,
        "plan_len": args.plan.len(),
        "plan_exhausted": shared.exhausted.load(Ordering::Relaxed),
        "requests": samples.len(),
        "accepted": ok.len(),
        "status_counts": by_status.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "throughput_rps": ok.len() as f64 / elapsed.as_secs_f64(),
        "service_us": percentiles(&mut service).map(|p| p.to_json()),
        "from_schedule_us": percentiles(&mut from_schedule).map(|p| p.to_json()),
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&result)?)
        .with_context(|| format!("{} に書き込めません", args.out.display()))?;

    // 受理した時刻（UNIX ミリ秒）。封印遅延の計算（受理の累積と封印の累積の突き合わせ）に使う。
    let mut accepted: Vec<u64> = ok.iter().map(|s| s.done_unix_ms).collect();
    accepted.sort_unstable();
    let text: String = accepted.iter().map(|t| format!("{t}\n")).collect();
    std::fs::write(args.out.with_extension("accepted"), text)?;

    println!(
        "{}: {} 件受理 / {} 件送信 / {:.1} 件/秒 / p50={:.1}ms p99={:.1}ms（{}）",
        args.label,
        ok.len(),
        samples.len(),
        ok.len() as f64 / elapsed.as_secs_f64(),
        percentiles(&mut service).map_or(0.0, |p| p.p50 as f64 / 1000.0),
        percentiles(&mut service).map_or(0.0, |p| p.p99 as f64 / 1000.0),
        by_status
            .iter()
            .map(|(s, n)| format!("{s}:{n}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(())
}
