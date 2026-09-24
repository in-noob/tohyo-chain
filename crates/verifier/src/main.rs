//! ハッシュチェーンの検証ツール。
//!
//! 使い方:
//!   verifier demo
//!   verifier verify [--api URL] [--public-key HEX]   （--api の既定は http://localhost:<api.port>。設定から読む）
//!   verifier tally [--api URL] [--public-key HEX] [--allow-interim] [--out DIR] [--all]
//!
//! 終了コード: 0=成功 / 1=実行時エラー（通信障害など）/ 2=使い方の誤り（`--allow-interim` を app.env=dev 以外で
//! 指定した場合を含む）/ 3=検証失敗（不整合を検出。tally は集計しない）/
//! 4=まだ実行できない（tally: 未封印の票が残っている・選挙状態が closed ではない。verify / tally: 票が非公開の締切前）

mod demo;
mod tally;
mod verify;

use std::process::ExitCode;

use anyhow::Context;
use shared_types::hex;
use verify::{
    AnchorCheck, AuditReport, ChainSource, HttpSource, RevoteSummary, ShardReport, ShardVerdict,
    audit, check_revotes, verify_all,
};

const USAGE: &str = "使い方:\n  verifier demo\n  verifier verify [--api URL] [--public-key HEX]\n  verifier tally [--api URL] [--public-key HEX] [--allow-interim] [--out DIR] [--all]\n  （--api の既定は http://localhost:<api.port>。api.port は config/*.toml か APP__API__PORT で決まる。\n   tally の --out の既定は out/tally。--allow-interim は選挙状態が closed になる前の中間集計を許す\n   （app.env=dev のときだけ）。--all は選挙区ごとの表を省略せず全件表示）";

/// 突合の行を、すべて表示する投票用紙の枚数の上限（これを超えると、不一致のものだけを表示する）。
const MAX_ROWS_SHOWN: usize = 20;

const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_INVALID: u8 = 3;
const EXIT_NOT_READY: u8 = 4;

/// `tally` の出力先の既定。
const DEFAULT_TALLY_OUT: &str = "out/tally";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Demo,
    Verify {
        /// `None` なら、設定の `api.port` から決める。
        api: Option<String>,
        public_key: Option<String>,
    },
    Tally(TallyArgs),
}

#[derive(Debug, PartialEq, Eq, Default)]
struct TallyArgs {
    api: Option<String>,
    public_key: Option<String>,
    allow_interim: bool,
    out: Option<String>,
    all_districts: bool,
}

fn parse_tally_args(rest: &[String]) -> Result<TallyArgs, String> {
    let mut args = TallyArgs::default();
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} には値が必要です"))
        };
        match flag.as_str() {
            "--api" => args.api = Some(value()?),
            "--public-key" => args.public_key = Some(value()?),
            "--out" => args.out = Some(value()?),
            "--allow-interim" => args.allow_interim = true,
            "--all" => args.all_districts = true,
            other => return Err(format!("不明なオプション: {other}")),
        }
    }
    Ok(args)
}

/// コマンドライン引数（プログラム名を除く）を解釈する。
fn parse_args(args: &[String]) -> Result<Command, String> {
    match args.split_first() {
        Some((cmd, [])) if cmd == "demo" => Ok(Command::Demo),
        Some((cmd, rest)) if cmd == "verify" => {
            let mut api = None;
            let mut public_key = None;
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("{flag} には値が必要です"))?;
                match flag.as_str() {
                    "--api" => api = Some(value.clone()),
                    "--public-key" => public_key = Some(value.clone()),
                    other => return Err(format!("不明なオプション: {other}")),
                }
            }
            Ok(Command::Verify { api, public_key })
        }
        Some((cmd, rest)) if cmd == "tally" => parse_tally_args(rest).map(Command::Tally),
        _ => Err("不明なサブコマンドです".to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(reason) => {
            eprintln!("{reason}\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("エラー: {e:#}");
            ExitCode::from(exit_code_for_error(&e))
        }
    }
}

/// 実行時エラーの終了コード。票が伏せられている（締切前）ときは、通信障害などと区別して、「まだ実行できない」（4）。
fn exit_code_for_error(error: &anyhow::Error) -> u8 {
    if error.downcast_ref::<verify::BallotsHidden>().is_some() {
        EXIT_NOT_READY
    } else {
        EXIT_RUNTIME_ERROR
    }
}

fn run(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Demo => demo::run().map(|()| ExitCode::SUCCESS),
        Command::Verify { api, public_key } => {
            let api = match api {
                Some(api) => api,
                None => default_api()?,
            };
            verify_command(&api, public_key.as_deref())
        }
        Command::Tally(args) => tally_command(args),
    }
}

/// 設定（`api.port`）から、api の URL を決める。
fn default_api() -> anyhow::Result<String> {
    let loaded = app_config::load()?;
    Ok(format!("http://localhost:{}", loaded.config.api.port))
}

fn verify_command(api: &str, public_key: Option<&str>) -> anyhow::Result<ExitCode> {
    // 表示に使う呼び名（設定の labels.ballot_item）。
    let ballot_item = app_config::load()?.config.labels.ballot_item;
    let source = HttpSource::new(api);
    let verified = verify_and_print(api, &source, public_key, &ballot_item)?;
    Ok(if verified.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_INVALID)
    })
}

/// チェーンの検証と突合の結果。
struct Verified {
    verdicts: Vec<ShardVerdict>,
    /// 全シャードのチェーンが正しいときだけ `Some`（突合は、その後にだけ行う）。
    audit: Option<AuditReport>,
    /// 再投票のつながりの検証に成功したときだけ `Some`（ADR 0022）。
    revotes: Option<RevoteSummary>,
    /// 検証と突合のすべてが成功したか。
    ok: bool,
}

impl Verified {
    /// 検証に成功したシャード。
    fn reports(&self) -> Vec<&ShardReport> {
        self.verdicts
            .iter()
            .filter_map(|v| match v {
                ShardVerdict::Valid(r) => Some(r),
                ShardVerdict::Invalid { .. } => None,
            })
            .collect()
    }
}

/// 全シャードの検証と突合を行い、結果を表示する（`verify` と `tally` の共通の前提確認）。
fn verify_and_print(
    api: &str,
    source: &dyn ChainSource,
    public_key: Option<&str>,
    ballot_item: &str,
) -> anyhow::Result<Verified> {
    let pinned = public_key
        .map(|k| {
            hex::decode_array::<32>(k).map_err(|e| anyhow::anyhow!("--public-key が不正です: {e}"))
        })
        .transpose()?;

    let verdicts = verify_all(source, pinned.as_ref())
        .with_context(|| format!("{api} からチェーンを検証できませんでした"))?;

    let mut invalid = 0;
    let (mut blocks, mut ballots) = (0, 0);
    let mut reports: Vec<&ShardReport> = Vec::new();
    for verdict in &verdicts {
        match verdict {
            ShardVerdict::Valid(r) => {
                println!(
                    "shard={} blocks={} ballots={} OK",
                    r.shard, r.blocks, r.ballots
                );
                blocks += r.blocks;
                ballots += r.ballots;
                reports.push(r);
            }
            ShardVerdict::Invalid { shard, failure } => {
                println!("shard={shard} NG: {failure}");
                invalid += 1;
            }
        }
    }
    if pinned.is_none() {
        println!("注意: 公開鍵は API が提供した値を使っています（--public-key で固定できます）");
    }

    // 全シャードのチェーンが正しいときだけ、再投票のつながり・突合・重複・アンカーを確認する。
    let mut audit_ok = true;
    let mut audit_report = None;
    let mut revote_summary = None;
    let mut contests_checked = 0;
    if invalid == 0 {
        let rules = source
            .rules()
            .with_context(|| format!("{api} から選挙のルールを取得できませんでした"))?;
        match check_revotes(&reports, rules) {
            Ok(summary) if rules.allow_revote => {
                // 件数だけを表示する（再投票の件数は、締切後にしか検証できない: 票が公開されるのは締切後）。
                println!(
                    "再投票のつながり: slot {} 個・再投票 {} 件（上限 {} 回）OK",
                    summary.slots,
                    summary.revotes(),
                    rules.max_revotes
                );
                revote_summary = Some(summary);
            }
            Ok(summary) => {
                println!("再投票のつながり: 再投票を認めない選挙で、slot を持つ票はありません OK");
                revote_summary = Some(summary);
            }
            Err(e) => {
                println!("再投票のつながり: NG（{e}）");
                audit_ok = false;
            }
        }
        let report = audit(source, &reports)
            .with_context(|| format!("{api} から突合に必要な情報を取得できませんでした"))?;
        println!("突合（{ballot_item}別: participation と、チェーン内の票数 + 封印待ち）:");
        // 47 都道府県規模では、投票用紙が数千枚になる。件数が多いときは、不一致のものだけを表示する。
        let show_all = report.contests.len() <= MAX_ROWS_SHOWN;
        if !show_all {
            println!(
                "  （{} 枚の{ballot_item}のうち、不一致のものだけを表示します）",
                report.contests.len()
            );
        }
        for row in &report.contests {
            if !show_all && row.is_consistent() {
                continue;
            }
            let verdict = if row.is_consistent() {
                "OK"
            } else {
                "NG（participation が、封印済みの slot の数 + 封印待ちの最初の票の数と一致しません）"
            };
            let note = if row.pending > 0 {
                "（封印待ちあり）"
            } else {
                ""
            };
            let revotes = if row.sealed_revotes > 0 {
                format!(" revotes={}", row.sealed_revotes)
            } else {
                String::new()
            };
            println!(
                "  contest={} participation={} sealed={}{revotes} pending={} {verdict}{note}",
                row.contest_id, row.participation, row.sealed, row.pending
            );
        }
        contests_checked = report.contests.len();
        if report.duplicate_ballots == 0 {
            println!("ballot_id の重複: なし OK");
        } else {
            println!(
                "ballot_id の重複: {} 件 NG（同じ票が複数回封印されています）",
                report.duplicate_ballots
            );
        }
        match &report.anchor {
            AnchorCheck::Valid { seq, shards } => {
                println!("アンカー: seq={seq} shards={shards} OK")
            }
            AnchorCheck::Missing => {
                println!("アンカー: まだ作られていません（ブロックの追加が無い間は作られません）")
            }
            AnchorCheck::Invalid(reason) => println!("アンカー: NG（{reason}）"),
        }
        audit_ok &= report.is_ok();
        audit_report = Some(report);
    }

    let ok = invalid == 0 && audit_ok;
    if ok {
        println!(
            "検証 OK: {} シャード, {blocks} ブロック, {ballots} 票, {contests_checked} 枚の{ballot_item}を突合",
            verdicts.len()
        );
    } else {
        let reason = if invalid > 0 {
            format!("{invalid} シャードで不整合を検出しました")
        } else {
            "突合・重複・アンカーの確認で不整合を検出しました".to_string()
        };
        println!("検証 NG: {reason}");
    }
    Ok(Verified {
        verdicts,
        audit: audit_report,
        revotes: revote_summary,
        ok,
    })
}

/// 現在の時刻（UNIX 秒）。時計が 1970 年より前を指すことは想定しない（その場合は 0）。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// `tally` の実行条件（設定・コマンドラインと、api から取得した選挙状態から決まる）。
struct TallyRun<'a> {
    api: &'a str,
    public_key: Option<&'a str>,
    ballot_item: &'a str,
    /// 設定の `labels.blank_name`（白票の表示名）。
    blank_name: &'a str,
    /// api から取得した、今の選挙状態（原則17）。
    phase: domain::ElectionPhase,
    /// 表示用（api から取得した締切。未設定なら `None`）。
    closes_at_display: Option<String>,
    now_unix: i64,
    allow_interim: bool,
    all_districts: bool,
    out_base: &'a std::path::Path,
}

/// 集計の結果。
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// 集計して、出力した（出力先のディレクトリ）。
    Done(std::path::PathBuf),
    /// 検証・突合・選挙データとの照合に失敗した（集計していない）。
    Invalid,
    /// 集計の前提を満たさない（未封印の票・締切前。集計していない）。
    NotReady(tally::gate::Refusal),
}

/// 検証 → 突合 → 未封印・締切の確認 → 集計 → 表示と CSV / JSON の出力。
/// どれかの確認が通らなければ、集計せず、出力ファイルも作らない。
fn tally_flow(
    source: &dyn ChainSource,
    election: &domain::election::Election,
    run: &TallyRun<'_>,
) -> anyhow::Result<Flow> {
    let ballot_item = run.ballot_item;

    // 1. チェーン全体の検証と、投票済み記録との突合。失敗したら集計しない。
    let verified = verify_and_print(run.api, source, run.public_key, ballot_item)
        .with_context(|| format!("{} からチェーンを検証できませんでした", run.api))?;
    let (true, Some(audit), Some(revotes)) = (
        verified.ok,
        verified.audit.as_ref(),
        verified.revotes.as_ref(),
    ) else {
        println!(
            "集計を中止しました: 検証または突合に失敗しました（改ざんされた可能性のあるデータは集計しません）"
        );
        return Ok(Flow::Invalid);
    };
    let reports = verified.reports();

    // 2. 未封印の票が残っていないこと。3. 選挙状態が closed であること（--allow-interim なら、中間集計として続行）。
    let (pending, pending_contests) = tally::gate::pending_of(&audit.contests);
    let gate_phase =
        match tally::gate::check(pending, pending_contests, run.phase, run.allow_interim) {
            Ok(gate_phase) => gate_phase,
            Err(refusal) => {
                println!("\n{}", refusal.message(ballot_item));
                return Ok(Flow::NotReady(refusal));
            }
        };

    // 4. 集計。チェーンと選挙データの食い違いは、検証失敗と同じ扱い。
    let recon = tally::Reconciliation::new(&reports, audit);
    let tallied = match tally::compute::compute(election, &reports, &audit.contests, revotes) {
        Ok(tallied) => tallied,
        Err(e) => {
            println!("\n集計を中止しました: {e}");
            return Ok(Flow::Invalid);
        }
    };
    let meta = tally::Meta {
        ballot_item,
        blank_name: run.blank_name,
        generated_at_unix: run.now_unix,
        phase: gate_phase,
        voting_closes_at: run.closes_at_display.as_deref(),
    };
    println!();
    print!(
        "{}",
        tally::render::render(&tallied, &meta, &recon, gate_phase, run.all_districts)
    );
    // 再投票の件数と変更の内訳（A→B）は、締切後の集計だけで出力する（中間集計で、途中の心変わりの傾向を出さない）。
    let revote_report = (gate_phase == tally::gate::Phase::Final)
        .then(|| tally::compute::revote_report(election, revotes));
    match &revote_report {
        Some(report) => print!(
            "\n{}",
            tally::render::render_revotes(report, ballot_item, run.blank_name)
        ),
        None if revotes.revotes() > 0 || revotes.slots > 0 => {
            println!("\n（再投票の件数・変更の内訳は、締切後の集計だけで出力します）");
        }
        None => {}
    }

    // 5. CSV / JSON。
    let dir = tally::export::write_all(
        run.out_base,
        &tallied,
        &recon,
        &meta,
        revote_report.as_ref(),
    )
    .context("集計結果のファイル出力に失敗しました")?;
    let revotes_csv = if revote_report.is_some() {
        " revotes.csv,"
    } else {
        ""
    };
    println!(
        "出力: {}（districts.csv, candidates.csv, prefectures.csv, types.csv, reconciliation.csv,{revotes_csv} tally.json）",
        dir.display()
    );
    Ok(Flow::Done(dir))
}

fn tally_command(args: TallyArgs) -> anyhow::Result<ExitCode> {
    let loaded = app_config::load()?;
    let config = &loaded.config;
    // --allow-interim（中間集計の漏洩を防ぐ）は、開発用の設定でだけ使える（原則17）。
    if args.allow_interim && config.app.env != app_config::Env::Dev {
        println!(
            "--allow-interim は app.env=dev のときだけ使えます（今の app.env は dev ではありません）。"
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    }
    let election_dir = config.election.election_dir();
    let election = seed::load_election(&election_dir)
        .with_context(|| format!("選挙データ {} を読み込めません", election_dir.display()))?;
    let api = args
        .api
        .clone()
        .unwrap_or_else(|| format!("http://localhost:{}", config.api.port));
    let out_base = std::path::PathBuf::from(args.out.as_deref().unwrap_or(DEFAULT_TALLY_OUT));
    let source = HttpSource::new(&api);
    // 選挙状態（原則17）。時刻ではなく、api が持つ状態（scheduled/open/closing/closed）を見る。
    let status = source
        .election_status()
        .context("選挙状態を取得できませんでした")?;
    let phase = domain::ElectionPhase::parse(&status.phase)
        .with_context(|| format!("api が返した選挙状態を解釈できません: {}", status.phase))?;
    let run = TallyRun {
        api: &api,
        public_key: args.public_key.as_deref(),
        ballot_item: &config.labels.ballot_item,
        blank_name: &config.labels.blank_name,
        phase,
        closes_at_display: status.closes_at.map(|secs| {
            shared_types::time::format_offset(secs, status.display_timezone_offset_secs)
        }),
        now_unix: now_unix(),
        allow_interim: args.allow_interim,
        all_districts: args.all_districts,
        out_base: &out_base,
    };
    Ok(match tally_flow(&source, &election, &run)? {
        Flow::Done(_) => ExitCode::SUCCESS,
        Flow::Invalid => ExitCode::from(EXIT_INVALID),
        Flow::NotReady(_) => ExitCode::from(EXIT_NOT_READY),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_demo_and_verify() {
        assert_eq!(parse_args(&args(&["demo"])), Ok(Command::Demo));
        assert_eq!(
            parse_args(&args(&["verify"])),
            Ok(Command::Verify {
                api: None,
                public_key: None
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "verify",
                "--api",
                "http://x:1",
                "--public-key",
                "ab"
            ])),
            Ok(Command::Verify {
                api: Some("http://x:1".to_string()),
                public_key: Some("ab".to_string())
            })
        );
    }

    #[test]
    fn rejects_bad_usage() {
        for bad in [
            &[][..],
            &["unknown"],
            &["demo", "extra"],
            &["verify", "--api"],
            &["verify", "--bogus", "x"],
            &["verify", "http://x"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    // --- 票が非公開（after_close の締切前）---

    #[test]
    fn hidden_ballots_are_reported_as_not_ready_not_as_a_runtime_error() {
        use anyhow::Context;
        let hidden: anyhow::Error = verify::BallotsHidden.into();
        assert_eq!(exit_code_for_error(&hidden), EXIT_NOT_READY);
        // 文脈が足されても（呼び出し側の `with_context`）、区別できる。
        let wrapped = Err::<(), _>(hidden)
            .context("http://x から検証できません")
            .unwrap_err();
        assert_eq!(exit_code_for_error(&wrapped), EXIT_NOT_READY);
        assert_eq!(
            exit_code_for_error(&anyhow::anyhow!("接続できません")),
            EXIT_RUNTIME_ERROR
        );
        assert!(verify::BallotsHidden.to_string().contains("締切後"));
    }

    // --- tally ---

    #[test]
    fn parses_tally_options() {
        assert_eq!(
            parse_args(&args(&["tally"])),
            Ok(Command::Tally(TallyArgs::default()))
        );
        assert_eq!(
            parse_args(&args(&[
                "tally",
                "--api",
                "http://x",
                "--allow-interim",
                "--out",
                "o",
                "--all",
                "--public-key",
                "ab"
            ])),
            Ok(Command::Tally(TallyArgs {
                api: Some("http://x".to_string()),
                public_key: Some("ab".to_string()),
                allow_interim: true,
                out: Some("o".to_string()),
                all_districts: true,
            }))
        );
        for bad in [
            &["tally", "--api"][..],
            &["tally", "--bogus"],
            &["tally", "extra"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    mod flow {
        use std::path::{Path, PathBuf};

        use domain::election::{Candidate, District, Election, ElectionType, VotingMethod};
        use domain::{CandidateCode, DistrictId, ElectionId, ElectionTypeCode};

        use super::super::*;
        use crate::tally::gate::Refusal;
        use crate::verify::tests::{FakeSource, chain, counts_for, reports, source};
        use domain::ElectionPhase;
        use serde_json::json;

        /// `chain()` が作る票（2026-general の東京 1 区・2 区、候補者 c1〜c4）に合わせた選挙マスタ。
        /// `districts` が 1 のときは、東京 2 区を含まない（チェーンにだけ存在する投票用紙を作る）。
        fn election(districts: u32) -> Election {
            let ty = ElectionTypeCode::new("shugiin_smd").expect("code");
            let mut ds = Vec::new();
            let mut cs = Vec::new();
            for n in 1..=districts {
                let id = format!("shugiin_smd.13.0{n}");
                ds.push(District {
                    id: DistrictId::new(&id).expect("district"),
                    election_type: ty.clone(),
                    name: format!("東京{n}区"),
                    prefectures: vec!["13".to_string()],
                    order: n,
                });
                for c in 1..=4 {
                    cs.push(Candidate {
                        id: CandidateCode::parse(&format!("{id}.c{c}")).expect("candidate"),
                        name: format!("候補{n}-{c}"),
                        party: "党".to_string(),
                        profile: String::new(),
                    });
                }
            }
            let types = vec![ElectionType {
                code: ty,
                name: "衆議院小選挙区".to_string(),
                order: 1,
                method: VotingMethod::SingleChoice,
            }];
            Election::new(
                ElectionId::new("2026-general").expect("election"),
                "テスト選挙".to_string(),
                types,
                ds,
                cs,
            )
            .expect("election")
        }

        /// 2 シャード・12 票のチェーンと、突合の値（`extra`: participation の過不足、`pending`: 未封印）。
        fn fixture(extra: i64, pending: u64) -> FakeSource {
            let mut src = source(vec![chain(&[5, 3], 1), chain(&[4], 2)]);
            let r = reports(&src);
            src.counts = counts_for(&r, extra, pending);
            src
        }

        fn temp_out() -> PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            std::env::temp_dir().join(format!("verifier-tally-{}-{nanos}", std::process::id()))
        }

        fn run(out: &Path, phase: ElectionPhase, allow: bool) -> TallyRun<'_> {
            TallyRun {
                api: "http://fake",
                public_key: None,
                ballot_item: "投票用紙",
                blank_name: "白票",
                phase,
                closes_at_display: Some("2026-10-01T09:00:00+09:00".to_string()),
                now_unix: 2_000,
                allow_interim: allow,
                all_districts: false,
                out_base: out,
            }
        }

        fn json_of(dir: &Path) -> serde_json::Value {
            let text = std::fs::read_to_string(dir.join("tally.json")).expect("tally.json");
            serde_json::from_str(&text).expect("json")
        }

        #[test]
        fn a_sealed_and_reconciled_chain_after_the_close_is_tallied_and_exported() {
            let out = temp_out();
            let flow = tally_flow(
                &fixture(0, 0),
                &election(2),
                &run(&out, ElectionPhase::Closed, false),
            )
            .expect("flow");
            let Flow::Done(dir) = flow else {
                panic!("集計されるはず: {flow:?}");
            };
            for f in [
                "districts.csv",
                "candidates.csv",
                "prefectures.csv",
                "types.csv",
                "reconciliation.csv",
                "tally.json",
            ] {
                assert!(dir.join(f).is_file(), "{f}");
            }
            let json = json_of(&dir);
            assert_eq!(json["interim"], false);
            let total: u64 = json["districts"]
                .as_array()
                .expect("districts")
                .iter()
                .map(|d| d["total"].as_u64().expect("total"))
                .sum();
            assert_eq!(total, 12);
            assert_eq!(json["total"]["participation"], 12);
            assert_eq!(json["reconciliation"]["duplicate_ballots"], 0);
            std::fs::remove_dir_all(&out).expect("cleanup");
        }

        #[test]
        fn unsealed_ballots_stop_the_tally_and_nothing_is_written() {
            let out = temp_out();
            // 各投票用紙に 2 件ずつ未封印（participation = 封印済み + 未封印 なので、突合は一致する）。
            let flow = tally_flow(
                &fixture(0, 2),
                &election(2),
                &run(&out, ElectionPhase::Closed, true),
            )
            .expect("flow");
            assert_eq!(
                flow,
                Flow::NotReady(Refusal::Unsealed {
                    ballots: 4,
                    contests: 2
                })
            );
            assert!(!out.exists());
        }

        #[test]
        fn a_tampered_chain_is_never_tallied() {
            let out = temp_out();
            let mut src = fixture(0, 0);
            src.shards[1][1].ballots[0].candidate_id = "shugiin_smd.13.01.c3".to_string();
            let flow = tally_flow(&src, &election(2), &run(&out, ElectionPhase::Closed, true))
                .expect("flow");
            assert_eq!(flow, Flow::Invalid);
            assert!(!out.exists());
        }

        #[test]
        fn a_reconciliation_mismatch_is_never_tallied() {
            let out = temp_out();
            // 投票済み記録が 1 件多い（票の消失）/ 1 件少ない（余分な票）。
            for extra in [1, -1] {
                let flow = tally_flow(
                    &fixture(extra, 0),
                    &election(2),
                    &run(&out, ElectionPhase::Closed, true),
                )
                .expect("flow");
                assert_eq!(flow, Flow::Invalid, "extra={extra}");
            }
            assert!(!out.exists());
        }

        #[test]
        fn not_closed_needs_allow_interim() {
            let out = temp_out();
            for phase in [
                ElectionPhase::Scheduled,
                ElectionPhase::Open,
                ElectionPhase::Closing,
            ] {
                let refused = tally_flow(&fixture(0, 0), &election(2), &run(&out, phase, false))
                    .expect("flow");
                assert_eq!(
                    refused,
                    Flow::NotReady(Refusal::NotClosed { phase }),
                    "{phase}"
                );
                assert!(!out.exists());
            }

            let Flow::Done(dir) = tally_flow(
                &fixture(0, 0),
                &election(2),
                &run(&out, ElectionPhase::Open, true),
            )
            .expect("flow") else {
                panic!("--allow-interim なら集計されるはず");
            };
            assert_eq!(json_of(&dir)["interim"], true);
            std::fs::remove_dir_all(&out).expect("cleanup");
        }

        #[test]
        fn a_contest_missing_from_the_election_data_is_not_tallied() {
            let out = temp_out();
            // チェーンには東京 2 区の票があるが、選挙データには東京 1 区しかない。
            let flow = tally_flow(
                &fixture(0, 0),
                &election(1),
                &run(&out, ElectionPhase::Closed, true),
            )
            .expect("flow");
            assert_eq!(flow, Flow::Invalid);
            assert!(!out.exists());
        }

        /// A → B → 白票（slot 1）と A（slot 2）の再投票のチェーン（ADR 0022）。
        fn revote_fixture() -> FakeSource {
            let mut src = source(vec![crate::verify::tests::a_b_blank()]);
            src.rules = crate::verify::tests::revote_rules(2);
            src.counts = shared_types::AuditCountsResponse {
                contests: vec![shared_types::ContestCountsDto {
                    contest_id: "2026-general/shugiin_smd.13.01".to_string(),
                    participation: 2,
                    pending: 0,
                    cast: Some(4),
                    pending_initial: Some(0),
                }],
            };
            src
        }

        #[test]
        fn revotes_count_only_the_last_ballot_and_the_changes_are_exported_after_the_close() {
            let out = temp_out();
            let Flow::Done(dir) = tally_flow(
                &revote_fixture(),
                &election(2),
                &run(&out, ElectionPhase::Closed, false),
            )
            .expect("flow") else {
                panic!("集計されるはず");
            };
            let json = json_of(&dir);
            let d = &json["districts"][0];
            // 4 票のうち、数えるのは slot ごとの最後の票（白票 1・候補1 1）だけ。
            assert_eq!(
                (d["total"].clone(), d["blank"].clone()),
                (json!(2), json!(1))
            );
            assert_eq!(d["participation"], json!(2));
            assert_eq!(json["revotes"]["revotes"], json!(2));
            let changes = &json["revotes"]["contests"][0]["changes"];
            assert_eq!(changes.as_array().expect("changes").len(), 2);
            let csv = std::fs::read_to_string(dir.join("revotes.csv")).expect("revotes.csv");
            assert!(
                csv.contains("候補1-1") && csv.contains("候補1-2") && csv.contains("白票"),
                "{csv}"
            );
            std::fs::remove_dir_all(&out).expect("cleanup");
        }

        #[test]
        fn an_interim_tally_does_not_output_the_revote_changes() {
            let out = temp_out();
            let Flow::Done(dir) = tally_flow(
                &revote_fixture(),
                &election(2),
                &run(&out, ElectionPhase::Open, true),
            )
            .expect("flow") else {
                panic!("中間集計されるはず");
            };
            assert!(!dir.join("revotes.csv").exists());
            assert!(json_of(&dir).get("revotes").is_none());
            std::fs::remove_dir_all(&out).expect("cleanup");
        }

        #[test]
        fn a_broken_revote_link_is_never_tallied() {
            let out = temp_out();
            let mut src = revote_fixture();
            // 再投票を認めない選挙として検証すると、slot を持つ票があるので不整合。
            src.rules = domain::ElectionRules::default();
            let flow = tally_flow(&src, &election(2), &run(&out, ElectionPhase::Closed, false))
                .expect("flow");
            assert_eq!(flow, Flow::Invalid);
            assert!(!out.exists());
        }
    }
}
