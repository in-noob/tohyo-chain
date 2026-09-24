//! 計測結果の集計。sealer のログ・負荷生成の出力・CPU のサンプルから、構成ごとの要約（summary.json）と
//! Markdown の表を作る。
//!
//! 封印遅延について: 票の受理時刻は「分」単位でしか保存しない（原則3）ので、票ごとの遅延は直接測れない。
//! 代わりに、負荷生成が記録した「受理した時刻（昇順）」と、sealer のログから作る「封印した時刻と件数（昇順）」を、
//! 全体を FIFO の 1 本の待ち行列とみなして突き合わせる: k 番目に受理された票は、封印の累積件数が k に達した
//! 封印の時刻に封印されたとみなし、その差を遅延とする。複数シャードの並行や、プール内の順序（分単位）を
//! 無視した近似で、遅延の全体像（分布）を見るためのもの。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde_json::{Value, json};

use crate::stats::percentiles;
use crate::timeparse::parse_rfc3339_ms;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealEvent {
    pub t_ms: i64,
    pub shard: u16,
    pub height: u64,
    pub count: usize,
    pub trigger: String,
    pub took_ms: Option<u64>,
}

/// sealer のログから、封印のイベント（`ブロックを封印しました shard=.. height=.. count=.. trigger=.. took_ms=..`）を取り出す。
pub fn parse_seal_events(log: &str) -> Vec<SealEvent> {
    log.lines()
        .filter(|line| line.contains("ブロックを封印しました"))
        .filter_map(|line| {
            let t_ms = parse_rfc3339_ms(line.split_whitespace().next()?)?;
            let field = |key: &str| -> Option<&str> {
                line.split_whitespace()
                    .find_map(|tok| tok.strip_prefix(key)?.strip_prefix('='))
            };
            Some(SealEvent {
                t_ms,
                shard: field("shard")?.parse().ok()?,
                height: field("height")?.parse().ok()?,
                count: field("count")?.parse().ok()?,
                trigger: field("trigger")?.to_string(),
                took_ms: field("took_ms").and_then(|v| v.parse().ok()),
            })
        })
        .collect()
}

/// 受理した時刻（昇順）と、封印した時刻・件数（昇順）から、k 番目の票の遅延（ミリ秒）を求める。
/// まだ封印されていない票は `None`。
pub fn fifo_delays_ms(arrivals_ms: &[i64], seals: &[(i64, usize)]) -> Vec<Option<i64>> {
    let mut delays = vec![None; arrivals_ms.len()];
    let mut next = 0usize;
    for &(sealed_at, count) in seals {
        let end = next.saturating_add(count).min(arrivals_ms.len());
        for k in next..end {
            // 受理時刻（負荷生成側の時計）と封印時刻（sealer 側の時計）の微小なずれで負にならないようにする。
            delays[k] = Some((sealed_at - arrivals_ms[k]).max(0));
        }
        next = end;
    }
    delays
}

/// `unix_ms,component,cpu_pct` の CSV から、区間 [from_ms, to_ms] の要素ごとの平均と最大（コア数 × 100%）。
pub fn cpu_summary(csv: &str, from_ms: i64, to_ms: i64) -> BTreeMap<String, (f64, f64)> {
    let mut acc: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in csv.lines() {
        let mut parts = line.split(',');
        let (Some(t), Some(name), Some(pct)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(t), Ok(pct)) = (t.trim().parse::<i64>(), pct.trim().parse::<f64>()) else {
            continue;
        };
        if (from_ms..=to_ms).contains(&t) {
            acc.entry(name.trim().to_string()).or_default().push(pct);
        }
    }
    acc.into_iter()
        .map(|(name, v)| {
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let max = v.iter().copied().fold(0.0, f64::max);
            (name, (mean, max))
        })
        .collect()
}

fn read_json(path: &Path) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("{} を読めません", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("{} が JSON ではありません", path.display()))
}

fn read_lines_i64(path: &Path) -> Vec<i64> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

fn i64_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `took_ms` を、封印時点の未封印件数（5 秒ごとのサンプルの直近値）の区間ごとに集計する。
/// 1 回の封印が未封印件数に比例して重くなるか（O(pending) か）の根拠。
pub fn took_by_pending(seals: &[SealEvent], pending: &[(i64, i64)]) -> Value {
    const EDGES: [i64; 5] = [0, 100, 1_000, 10_000, 100_000];
    let mut buckets: Vec<Vec<u64>> = vec![Vec::new(); EDGES.len()];
    for e in seals {
        let (Some(took), Some((_, n))) =
            (e.took_ms, pending.iter().rev().find(|(t, _)| *t <= e.t_ms))
        else {
            continue;
        };
        let i = EDGES.iter().rposition(|edge| n >= edge).unwrap_or(0);
        buckets[i].push(took);
    }
    let rows: Vec<Value> = buckets
        .into_iter()
        .enumerate()
        .map(|(i, mut v)| {
            let label = match EDGES.get(i + 1) {
                Some(next) => format!("{}〜{}", EDGES[i], next - 1),
                None => format!("{}〜", EDGES[i]),
            };
            let p = percentiles(&mut v);
            json!({
                "pending": label,
                "count": p.as_ref().map_or(0, |p| p.count),
                "p50": p.as_ref().map(|p| p.p50),
                "p99": p.as_ref().map(|p| p.p99),
            })
        })
        .collect();
    Value::Array(rows)
}

/// 1 つの構成（実行ディレクトリ）の要約を作る。
///
/// フェーズの順序は、ウォームアップ → 定常（`rate.json`）→ ドレイン（`drain.json`。任意）→ 飽和（`sat.json`。任意）。
pub fn summarize_run(dir: &Path) -> anyhow::Result<Value> {
    let meta = read_json(&dir.join("meta.json"))?;
    let warmup = read_json(&dir.join("warmup.json")).ok();
    let steady = read_json(&dir.join("rate.json")).context("rate.json がありません")?;
    let sat = read_json(&dir.join("sat.json")).ok();
    let drain = read_json(&dir.join("drain.json")).unwrap_or(Value::Null);
    let steady_end_ms = i64_of(&steady, "end_unix_ms");
    let sat_start_ms = sat
        .as_ref()
        .map_or(i64::MAX, |s| i64_of(s, "start_unix_ms"));

    // --- 受理した時刻（飽和の前のものだけ。飽和の票は未封印のまま終わるため遅延を測れない） ---
    let mut arrivals: Vec<(i64, &str)> = Vec::new();
    for name in ["warmup", "rate"] {
        for t in read_lines_i64(&dir.join(format!("{name}.accepted"))) {
            arrivals.push((t, name));
        }
    }
    arrivals.sort_by_key(|(t, _)| *t);
    let arrival_times: Vec<i64> = arrivals.iter().map(|(t, _)| *t).collect();

    // --- 封印のイベント ---
    let mut seals: Vec<SealEvent> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_sealer_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("sealer-") && n.ends_with(".log"));
        if is_sealer_log {
            seals.extend(parse_seal_events(&std::fs::read_to_string(&path)?));
        }
    }
    seals.sort_by_key(|e| e.t_ms);

    let count_by_trigger = |events: &[&SealEvent]| -> Value {
        let mut blocks: BTreeMap<&str, u64> = BTreeMap::new();
        let mut ballots: BTreeMap<&str, u64> = BTreeMap::new();
        for e in events {
            *blocks.entry(e.trigger.as_str()).or_default() += 1;
            *ballots.entry(e.trigger.as_str()).or_default() += e.count as u64;
        }
        let total_ballots: u64 = ballots.values().sum();
        json!({ "blocks": blocks, "ballots": ballots, "total_blocks": events.len(), "total_ballots": total_ballots })
    };
    let steady_seals: Vec<&SealEvent> = seals.iter().filter(|e| e.t_ms <= steady_end_ms).collect();
    let drain_seals: Vec<&SealEvent> = seals
        .iter()
        .filter(|e| e.t_ms > steady_end_ms && e.t_ms < sat_start_ms)
        .collect();
    let sat_seals: Vec<&SealEvent> = seals.iter().filter(|e| e.t_ms >= sat_start_ms).collect();
    let mut took: Vec<u64> = seals.iter().filter_map(|e| e.took_ms).collect();

    // --- 封印遅延（FIFO 近似。飽和の前に受理した票と、飽和の前の封印だけを使う） ---
    let seal_batches: Vec<(i64, usize)> = seals
        .iter()
        .filter(|e| e.t_ms < sat_start_ms)
        .map(|e| (e.t_ms, e.count))
        .collect();
    let delays = fifo_delays_ms(&arrival_times, &seal_batches);
    let delay_of = |name: Option<&str>| -> Value {
        let mut values: Vec<u64> = arrivals
            .iter()
            .zip(&delays)
            .filter(|((_, p), _)| name.is_none_or(|n| *p == n))
            .filter_map(|(_, d)| d.map(|v| v as u64))
            .collect();
        percentiles(&mut values).map_or(Value::Null, |p| p.to_json())
    };
    let unsealed = delays.iter().filter(|d| d.is_none()).count();

    // --- CPU・未封印の推移 ---
    let cpu_csv = std::fs::read_to_string(dir.join("cpu.csv")).unwrap_or_default();
    let window = |v: &Value| (i64_of(v, "start_unix_ms"), i64_of(v, "end_unix_ms"));
    let cpu_json = |from, to| -> Value {
        cpu_summary(&cpu_csv, from, to)
            .into_iter()
            .map(|(k, (mean, max))| (k, json!({ "mean": mean, "max": max })))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let pending: Vec<(i64, i64)> = std::fs::read_to_string(dir.join("pending.csv"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (t, n) = l.split_once(',')?;
            Some((t.trim().parse().ok()?, n.trim().parse().ok()?))
        })
        .collect();
    let pending_at = |t: i64| {
        pending
            .iter()
            .rev()
            .find(|(pt, _)| *pt <= t)
            .map(|(_, n)| *n)
    };
    let steady_pending_max = pending
        .iter()
        .filter(|(t, _)| *t <= steady_end_ms)
        .map(|(_, n)| *n)
        .max();

    // --- 飽和（任意）---
    let saturation = sat.as_ref().map(|sat| {
        let (from, to) = window(sat);
        let elapsed_s = sat["elapsed_s"].as_f64().unwrap_or(0.0);
        let sealed_ballots: u64 = sat_seals.iter().map(|e| e.count as u64).sum();
        let after = std::fs::read_to_string(dir.join("pending-after-sat.txt"))
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok());
        json!({
            "throughput_rps": sat["throughput_rps"], "accepted": sat["accepted"], "requests": sat["requests"],
            "status_counts": sat["status_counts"], "service_us": sat["service_us"], "concurrency": sat["concurrency"],
            "sealed_ballots": sealed_ballots,
            "sealed_per_s": if elapsed_s > 0.0 { json!(sealed_ballots as f64 / elapsed_s) } else { Value::Null },
            "pending_at_start": pending_at(from),
            "pending_at_end": after.or_else(|| pending_at(to)),
            "cpu_pct": cpu_json(from, to),
        })
    });
    let (steady_from, steady_to) = window(&steady);

    Ok(json!({
        "config": meta,
        "warmup": warmup.as_ref().map(|w| json!({"target_rate": w["target_rate"], "accepted": w["accepted"]})),
        "steady": {
            "target_rate": steady["target_rate"], "throughput_rps": steady["throughput_rps"],
            "accepted": steady["accepted"], "requests": steady["requests"], "status_counts": steady["status_counts"],
            "service_us": steady["service_us"], "from_schedule_us": steady["from_schedule_us"],
            "cpu_pct": cpu_json(steady_from, steady_to),
            "pending_max": steady_pending_max,
        },
        "saturation": saturation,
        "seals": {
            "total_blocks": seals.len(),
            "steady": count_by_trigger(&steady_seals),
            "drain": count_by_trigger(&drain_seals),
            "saturation": count_by_trigger(&sat_seals),
            "took_ms": percentiles(&mut took).map_or(Value::Null, |p| p.to_json()),
            "took_by_pending": took_by_pending(&seals, &pending),
        },
        "delay_ms": {
            "note": "受理の累積と封印の累積の FIFO 突き合わせによる近似（飽和の前に受理した票のみ）",
            "all": delay_of(None), "steady": delay_of(Some("rate")),
            "unsealed": unsealed,
        },
        "drain": drain,
    }))
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn ms(us: &Value) -> String {
    us.as_f64()
        .map_or("-".to_string(), |v| format!("{:.1}", v / 1000.0))
}

fn num(v: &Value, digits: usize) -> String {
    v.as_f64()
        .map_or("-".to_string(), |x| format!("{x:.digits$}"))
}

fn secs(ms: &Value) -> String {
    ms.as_f64()
        .map_or("-".to_string(), |v| format!("{:.1}", v / 1000.0))
}

fn pct(part: u64, total: u64) -> String {
    if total == 0 {
        "-".to_string()
    } else {
        format!("{:.0}%", 100.0 * part as f64 / total as f64)
    }
}

/// 実行ディレクトリの一覧から、比較用の Markdown の表を作る。
pub fn render_markdown(root: &Path) -> anyhow::Result<String> {
    let mut runs: Vec<(String, Value)> = Vec::new();
    let mut stores: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if path.is_dir() && path.join("summary.json").exists() {
            runs.push((name, read_json(&path.join("summary.json"))?));
        } else if name.starts_with("store-") && name.ends_with(".json") {
            stores.push(read_json(&path)?);
        }
    }
    runs.sort_by_key(|(name, v)| {
        let c = &v["config"];
        (
            name.ends_with("-trickle"),
            i64_of(c, "shards"),
            i64_of(c, "apis"),
        )
    });
    stores.sort_by_key(|v| i64_of(v, "shard_count"));

    let mut out = String::new();
    let row = |out: &mut String, cells: Vec<String>| {
        out.push_str("| ");
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    };
    let bad_statuses = |counts: &Value| -> u64 {
        counts
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| *k != "201")
                    .filter_map(|(_, n)| n.as_u64())
                    .sum()
            })
            .unwrap_or(0)
    };
    // 構成の表示名: ドレインしない構成・トリクルには印を付ける。
    let label = |name: &str, c: &Value| -> (String, String, String) {
        let mut mark = String::new();
        if name.ends_with("-trickle") {
            mark.push_str(" (trickle)");
        } else if c["drain"] == json!(false) {
            mark.push_str(" (ドレインなし)");
        }
        (
            c["shards"].to_string(),
            c["sealers"].to_string(),
            format!("{}{}", c["apis"], mark),
        )
    };
    let normal: Vec<&(String, Value)> = runs
        .iter()
        .filter(|(n, _)| !n.ends_with("-trickle"))
        .collect();

    out.push_str("### 定常負荷（オープンループ、固定レート）\n\n");
    out.push_str("| shards | sealer | api | 目標レート (件/秒) | 実測 (件/秒) | p50 (ms) | p99 (ms) | 予定時刻基準 p50 (ms) | 予定時刻基準 p99 (ms) | 201 以外 | 未封印の最大 |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (name, v) in &normal {
        let (sh, se, ap) = label(name, &v["config"]);
        let s = &v["steady"];
        row(
            &mut out,
            vec![
                sh,
                se,
                ap,
                num(&s["target_rate"], 0),
                num(&s["throughput_rps"], 0),
                ms(&s["service_us"]["p50"]),
                ms(&s["service_us"]["p99"]),
                ms(&s["from_schedule_us"]["p50"]),
                ms(&s["from_schedule_us"]["p99"]),
                bad_statuses(&s["status_counts"]).to_string(),
                s["pending_max"]
                    .as_i64()
                    .map_or("-".to_string(), |n| n.to_string()),
            ],
        );
    }

    out.push_str("\n### 飽和測定（クローズドループ。最後に実施）\n\n");
    out.push_str("| shards | sealer | api | 同時接続 | 受理 (件/秒) | p50 (ms) | p99 (ms) | 201 以外 | sealer が封印した票 (件/秒) | 未封印の増加（開始 → 終了） |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for (name, v) in &normal {
        let s = &v["saturation"];
        if s.is_null() {
            continue;
        }
        let (sh, se, ap) = label(name, &v["config"]);
        let g = |x: &Value| x.as_i64().map_or("-".to_string(), |n| n.to_string());
        row(
            &mut out,
            vec![
                sh,
                se,
                ap,
                s["concurrency"].to_string(),
                num(&s["throughput_rps"], 0),
                ms(&s["service_us"]["p50"]),
                ms(&s["service_us"]["p99"]),
                bad_statuses(&s["status_counts"]).to_string(),
                num(&s["sealed_per_s"], 0),
                format!(
                    "{} → {}",
                    g(&s["pending_at_start"]),
                    g(&s["pending_at_end"])
                ),
            ],
        );
    }

    out.push_str("\n### 封印の trigger 別の回数（ブロック数）\n\n");
    out.push_str("| 構成 | 定常中 count | 定常中 time | 定常中 flush | ドレイン中 count | ドレイン中 time | ドレイン中 flush | 飽和中 count | 飽和中 time | 定常+ドレインの count 割合 | 定常+ドレインの time 割合 |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (name, v) in &runs {
        let c = &v["config"];
        let (sh, se, ap) = label(name, c);
        let s = &v["seals"];
        let get = |phase: &str, t: &str| s[phase]["blocks"][t].as_u64().unwrap_or(0);
        let total = get("steady", "count")
            + get("steady", "time")
            + get("steady", "flush")
            + get("drain", "count")
            + get("drain", "time")
            + get("drain", "flush");
        let all = |t: &str| get("steady", t) + get("drain", t);
        row(
            &mut out,
            vec![
                format!("shards={sh} sealer={se} api={ap}"),
                get("steady", "count").to_string(),
                get("steady", "time").to_string(),
                get("steady", "flush").to_string(),
                get("drain", "count").to_string(),
                get("drain", "time").to_string(),
                get("drain", "flush").to_string(),
                get("saturation", "count").to_string(),
                get("saturation", "time").to_string(),
                pct(all("count"), total),
                pct(all("time"), total),
            ],
        );
    }

    out.push_str(
        "\n### 封印遅延の分布（受理 → 封印、秒。FIFO 近似。ドレインまで行った構成のみ）\n\n",
    );
    out.push_str("| 構成 | 対象 | 件数 | p50 | p90 | p99 | max | 未封印 |\n|---|---|---|---|---|---|---|---|\n");
    for (name, v) in &runs {
        if v["drain"]["drained"] != json!(true) {
            continue;
        }
        let (sh, se, ap) = label(name, &v["config"]);
        let d = &v["delay_ms"];
        for (lbl, key) in [
            ("全体（ウォームアップ + 定常）", "all"),
            ("定常中に受理", "steady"),
        ] {
            row(
                &mut out,
                vec![
                    format!("shards={sh} sealer={se} api={ap}"),
                    lbl.to_string(),
                    d[key]["count"].to_string(),
                    secs(&d[key]["p50"]),
                    secs(&d[key]["p90"]),
                    secs(&d[key]["p99"]),
                    secs(&d[key]["max"]),
                    d["unsealed"].to_string(),
                ],
            );
        }
    }

    out.push_str(
        "\n### 封印 1 回に要した時間 took_ms（中央値 ms / 件数）と、その時点の未封印件数\n\n",
    );
    out.push_str("| 構成 | 0〜99 | 100〜999 | 1000〜9999 | 10000〜99999 | 100000〜 |\n|---|---|---|---|---|---|\n");
    for (name, v) in &runs {
        let (sh, se, ap) = label(name, &v["config"]);
        let mut cells = vec![format!("shards={sh} sealer={se} api={ap}")];
        for b in v["seals"]["took_by_pending"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            cells.push(match b["p50"].as_u64() {
                Some(p50) => format!("{p50} / {}", b["count"]),
                None => "-".to_string(),
            });
        }
        row(&mut out, cells);
    }

    out.push_str("\n### CPU 使用率（コア数 × 100%。区間内の 1 秒ごとのサンプルの平均 / 最大）\n\n");
    out.push_str("| 構成 | 区間 | api 合計 | sealer 合計 | DB | 負荷生成 | マシン全体 |\n|---|---|---|---|---|---|---|\n");
    for (name, v) in &runs {
        let (sh, se, ap) = label(name, &v["config"]);
        for (lbl, cpu) in [
            ("定常", &v["steady"]["cpu_pct"]),
            ("飽和", &v["saturation"]["cpu_pct"]),
        ] {
            if cpu.is_null() {
                continue;
            }
            let cell = |n: &str| {
                let x = &cpu[n];
                if x.is_null() {
                    "-".to_string()
                } else {
                    format!("{} / {}", num(&x["mean"], 0), num(&x["max"], 0))
                }
            };
            row(
                &mut out,
                vec![
                    format!("shards={sh} sealer={se} api={ap}"),
                    lbl.to_string(),
                    cell("api"),
                    cell("sealer"),
                    cell("db"),
                    cell("bench"),
                    cell("machine"),
                ],
            );
        }
    }

    if !stores.is_empty() {
        out.push_str("\n### DB 直接（api を介さない `ScyllaStore::cast`。LWT + 票の INSERT。sealer なし）\n\n");
        out.push_str("| shards | 同時実行 | スループット (件/秒) | p50 (ms) | p99 (ms) | エラー |\n|---|---|---|---|---|---|\n");
        for s in &stores {
            row(
                &mut out,
                vec![
                    s["shard_count"].to_string(),
                    s["concurrency"].to_string(),
                    num(&s["throughput_rps"], 0),
                    ms(&s["latency_us"]["p50"]),
                    ms(&s["latency_us"]["p99"]),
                    s["errors"].to_string(),
                ],
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
2026-09-20T05:00:01.250000Z  INFO sealer::coordinator: リースを取得しました shard=0 owner=sealer-a
2026-09-20T05:00:10.500000Z  INFO sealer::sealer: ブロックを封印しました shard=0 height=1 count=100 trigger=count took_ms=23
2026-09-20T05:10:10.750000Z  INFO sealer::sealer: ブロックを封印しました shard=3 height=7 count=42 trigger=time took_ms=9
2026-09-20T05:10:11.000000Z  INFO sealer::sealer: ブロックを封印しました shard=1 height=2 count=5 trigger=flush
2026-09-20T05:10:12.000000Z DEBUG sealer::sealer: 窓をリセットしました（未封印 0 件） shard=0
";

    #[test]
    fn parses_only_seal_lines_with_all_fields() {
        let events = parse_seal_events(LOG);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            SealEvent {
                t_ms: parse_rfc3339_ms("2026-09-20T05:00:10.500000Z").expect("valid"),
                shard: 0,
                height: 1,
                count: 100,
                trigger: "count".to_string(),
                took_ms: Some(23)
            }
        );
        assert_eq!(
            (
                events[1].trigger.as_str(),
                events[1].count,
                events[1].took_ms
            ),
            ("time", 42, Some(9))
        );
        // took_ms のない古い形式のログも読める。
        assert_eq!(
            (events[2].trigger.as_str(), events[2].took_ms),
            ("flush", None)
        );
        assert_eq!(events[1].t_ms - events[0].t_ms, 600_250);
    }

    #[test]
    fn malformed_lines_are_ignored() {
        assert!(
            parse_seal_events(
                "ブロックを封印しました shard=x height=1 count=1 trigger=count\nゴミ\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn fifo_delay_maps_the_kth_arrival_to_the_seal_that_reaches_k() {
        // 5 票が 1000, 2000, 3000, 4000, 5000 ms に受理され、3 票が 3500 ms に、2 票が 9000 ms に封印される。
        let delays = fifo_delays_ms(&[1000, 2000, 3000, 4000, 5000], &[(3500, 3), (9000, 2)]);
        assert_eq!(
            delays,
            vec![Some(2500), Some(1500), Some(500), Some(5000), Some(4000)]
        );
    }

    #[test]
    fn unsealed_arrivals_have_no_delay_and_clock_skew_is_clamped() {
        let delays = fifo_delays_ms(&[1000, 2000, 3000], &[(1990, 2)]);
        assert_eq!(delays, vec![Some(990), Some(0), None]);
        // 封印の件数が受理の件数より多くても、範囲外に出ない。
        assert_eq!(fifo_delays_ms(&[1000], &[(2000, 5)]), vec![Some(1000)]);
        assert!(fifo_delays_ms(&[], &[(1, 1)]).is_empty());
    }

    #[test]
    fn cpu_summary_averages_within_the_window() {
        let csv = "1000,api,100\n2000,api,300\n3000,api,500\n2000,db,50\n2000,db,150\nbad line\n";
        let s = cpu_summary(csv, 1500, 3000);
        assert_eq!(s["api"], (400.0, 500.0));
        assert_eq!(s["db"], (100.0, 150.0));
        assert!(cpu_summary(csv, 9000, 9999).is_empty());
    }

    #[test]
    fn took_by_pending_buckets_by_the_nearest_earlier_sample() {
        let ev = |t: i64, took: u64| SealEvent {
            t_ms: t,
            shard: 0,
            height: 1,
            count: 100,
            trigger: "count".into(),
            took_ms: Some(took),
        };
        let pending = [(1000, 50), (2000, 5_000), (3000, 500_000)];
        let seals = [
            ev(1500, 10),
            ev(2500, 200),
            ev(2600, 220),
            ev(9000, 3000),
            ev(500, 99),
        ];
        let rows = took_by_pending(&seals, &pending);
        let rows = rows.as_array().expect("array");
        let p50 = |i: usize| rows[i]["p50"].as_u64();
        assert_eq!((rows[0]["count"].as_u64(), p50(0)), (Some(1), Some(10)));
        assert_eq!(rows[1]["count"], 0); // 100〜999: なし
        assert_eq!((rows[2]["count"].as_u64(), p50(2)), (Some(2), Some(200)));
        assert_eq!((rows[4]["count"].as_u64(), p50(4)), (Some(1), Some(3000)));
        // 最初のサンプルより前の封印は、対応する未封印件数がないので数えない。
        assert_eq!(
            rows.iter()
                .map(|r| r["count"].as_u64().unwrap_or(0))
                .sum::<u64>(),
            4
        );
    }

    #[test]
    fn summarize_and_render_a_synthetic_run() {
        let dir = std::env::temp_dir().join(format!("bench-report-test-{}", rand::random::<u32>()));
        let run = dir.join("s1-k1-a1");
        std::fs::create_dir_all(&run).expect("mkdir");
        let write = |name: &str, text: &str| std::fs::write(run.join(name), text).expect("write");
        write(
            "meta.json",
            r#"{"shards":1,"sealers":1,"apis":1,"drain":true,"max_ballots":100,"interval_secs":600}"#,
        );
        let phase = |label: &str, start: i64, end: i64, rate: Value| {
            json!({"label": label, "start_unix_ms": start, "end_unix_ms": end, "throughput_rps": 50.0,
                   "accepted": 3, "requests": 3, "status_counts": {"201": 3}, "concurrency": 4, "elapsed_s": 4.0,
                   "target_rate": rate, "service_us": {"p50": 4000, "p99": 9000},
                   "from_schedule_us": {"p50": 5000, "p99": 12000}}).to_string()
        };
        // 時間軸: ウォームアップ 0〜1 秒 → 定常 1〜5 秒 → ドレイン（〜609 秒）→ 飽和 610〜614 秒。
        let t0 = parse_rfc3339_ms("2026-09-20T05:00:00Z").expect("valid");
        write("warmup.json", &phase("warmup", t0, t0 + 1000, json!(10.0)));
        write(
            "rate.json",
            &phase("rate", t0 + 1000, t0 + 5000, json!(25.0)),
        );
        write(
            "sat.json",
            &phase("sat", t0 + 610_000, t0 + 614_000, Value::Null),
        );
        write("warmup.accepted", &format!("{}\n", t0 + 500));
        write(
            "rate.accepted",
            &format!("{}\n{}\n{}\n", t0 + 2000, t0 + 3000, t0 + 4000),
        );
        write("sat.accepted", &format!("{}\n", t0 + 611_000));
        write(
            "sealer-1.log",
            "2026-09-20T05:00:04.000000Z  INFO sealer::sealer: ブロックを封印しました shard=0 height=1 count=3 trigger=count took_ms=10\n\
             2026-09-20T05:10:09.000000Z  INFO sealer::sealer: ブロックを封印しました shard=0 height=2 count=1 trigger=time took_ms=8\n\
             2026-09-20T05:10:12.000000Z  INFO sealer::sealer: ブロックを封印しました shard=0 height=3 count=100 trigger=count took_ms=500\n",
        );
        write(
            "cpu.csv",
            &format!(
                "{},api,80\n{},sealer,20\n{},bench,300\n",
                t0 + 2000,
                t0 + 2000,
                t0 + 2000
            ),
        );
        write(
            "pending.csv",
            &format!(
                "{},0\n{},3\n{},1\n{},50\n",
                t0,
                t0 + 4900,
                t0 + 8900,
                t0 + 611_000
            ),
        );
        write("pending-after-sat.txt", "250\n");
        write("drain.json", r#"{"drained": true}"#);

        let summary = summarize_run(&run).expect("summary");
        // 封印は、定常中（〜5 秒）が 1 回、ドレイン中が 1 回、飽和中が 1 回。
        assert_eq!(summary["seals"]["steady"]["blocks"]["count"], 1);
        assert_eq!(summary["seals"]["drain"]["blocks"]["time"], 1);
        assert_eq!(summary["seals"]["saturation"]["blocks"]["count"], 1);
        assert_eq!(summary["seals"]["total_blocks"], 3);
        // 遅延は飽和の前に受理した 4 票だけで計算する（受理 0.5, 2, 3, 4 秒 → 最初の 3 票が 4 秒に、最後の 1 票が
        // 9 分 9 秒に封印: 遅延 3.5, 2, 1 秒と 605 秒）。飽和中の票と封印は含まれない。
        assert_eq!(summary["delay_ms"]["all"]["count"], 4);
        assert_eq!(summary["delay_ms"]["all"]["p50"], 2_000);
        assert_eq!(summary["delay_ms"]["all"]["max"], 605_000);
        assert_eq!(summary["delay_ms"]["steady"]["p50"], 2_000);
        assert_eq!(summary["delay_ms"]["unsealed"], 0);
        // 飽和: 封印された票 100（4 秒 → 25/秒）。未封印は、開始直前のサンプル（1 件）→ 終了直後の記録（250 件）。
        assert_eq!(summary["saturation"]["sealed_ballots"], 100);
        assert_eq!(summary["saturation"]["sealed_per_s"], 25.0);
        assert_eq!(summary["saturation"]["pending_at_start"], 1);
        assert_eq!(summary["saturation"]["pending_at_end"], 250);
        assert_eq!(summary["steady"]["pending_max"], 3);
        assert_eq!(summary["steady"]["cpu_pct"]["api"]["mean"], 80.0);

        std::fs::write(run.join("summary.json"), summary.to_string()).expect("write");
        let md = render_markdown(&dir).expect("markdown");
        for section in [
            "定常負荷",
            "飽和測定",
            "封印の trigger 別",
            "封印遅延の分布",
            "took_ms",
            "CPU 使用率",
        ] {
            assert!(md.contains(section), "{section}: {md}");
        }
        assert!(md.contains("| 1 | 1 | 1 |"), "{md}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
