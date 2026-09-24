//! ダミーの選挙データの生成ツール。
//!
//!   seedgen --out DIR [--election-id ID] [--prefectures N] [--districts-per-pref K]
//!           [--candidates-per-district C] [--voters V] [--municipalities-per-pref M]
//!           [--pref-assembly-districts-per-pref A] [--voter-prefix P] [--seed S] [--force]
//!   seedgen --check DIR [--election-id ID]     既存のデータ（手で編集した CSV など）を検証するだけ
//!
//! `DIR/<election_id>/` に、選挙データ一式（election.toml・districts.csv・candidates/*.csv・voters.csv）を作る。
//! `DIR` を選挙データのディレクトリ（設定 `election.seed_dir`）に、`--election-id` を `election.election_id` に指定すると、
//! api・bench が読める。生成したデータは、`seed` クレートの検証（ID の形式・重複・参照の整合）を通してから終わる。

mod generate;

use std::path::PathBuf;

use anyhow::{Context, bail};

const USAGE: &str = "使い方: seedgen --check DIR [--election-id ID（既定 2026-general）]   既存のデータの検証だけ（不正なら、ファイル・行・原因を表示して終了コード 1）
        seedgen --out DIR [--election-id ID（既定 2026-general）] [--prefectures N（1〜47、既定 47）]
                [--districts-per-pref K（小選挙区の数。既定 6）] [--candidates-per-district C（既定 8）]
                [--voters V（既定 1000）] [--municipalities-per-pref M（市区町村の数。既定 10）]
                [--pref-assembly-districts-per-pref A（都道府県議会の選挙区の数。既定 4）]
                [--voter-prefix P（有権者 ID の接頭辞。既定 voter-）] [--seed S（既定 1）] [--force（既存のデータを上書き）]";

/// 生成の規模と、出力先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub out: PathBuf,
    pub election_id: String,
    pub prefectures: u32,
    pub districts_per_pref: u32,
    pub candidates_per_district: u32,
    pub voters: u64,
    pub municipalities_per_pref: u32,
    pub pref_assembly_districts_per_pref: u32,
    pub voter_prefix: String,
    pub seed: u64,
    pub force: bool,
}

fn parse_args(args: &[String]) -> anyhow::Result<Options> {
    let mut out = None;
    let mut options = Options {
        out: PathBuf::new(),
        election_id: "2026-general".to_string(),
        prefectures: 47,
        districts_per_pref: 6,
        candidates_per_district: 8,
        voters: 1000,
        municipalities_per_pref: 10,
        pref_assembly_districts_per_pref: 4,
        voter_prefix: "voter-".to_string(),
        seed: 1,
        force: false,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if flag == "--force" {
            options.force = true;
            continue;
        }
        let key = flag
            .strip_prefix("--")
            .with_context(|| format!("不明な引数: {flag}"))?;
        let value = it
            .next()
            .with_context(|| format!("--{key} には値が必要です"))?;
        let number = |what: &str| -> anyhow::Result<u64> {
            value
                .parse()
                .with_context(|| format!("--{what} は整数が必要です: {value:?}"))
        };
        match key {
            "out" => out = Some(PathBuf::from(value)),
            "election-id" => options.election_id = value.clone(),
            "prefectures" => options.prefectures = u32::try_from(number(key)?)?,
            "districts-per-pref" => options.districts_per_pref = u32::try_from(number(key)?)?,
            "candidates-per-district" => {
                options.candidates_per_district = u32::try_from(number(key)?)?;
            }
            "voters" => options.voters = number(key)?,
            "municipalities-per-pref" => {
                options.municipalities_per_pref = u32::try_from(number(key)?)?;
            }
            "pref-assembly-districts-per-pref" => {
                options.pref_assembly_districts_per_pref = u32::try_from(number(key)?)?;
            }
            "voter-prefix" => options.voter_prefix = value.clone(),
            "seed" => options.seed = number(key)?,
            other => bail!("不明なオプション: --{other}"),
        }
    }
    options.out = out.context("--out が必要です")?;
    if !(1..=47).contains(&options.prefectures) {
        bail!("--prefectures は 1〜47 が必要です: {}", options.prefectures);
    }
    for (name, value) in [
        ("districts-per-pref", options.districts_per_pref),
        ("candidates-per-district", options.candidates_per_district),
        ("municipalities-per-pref", options.municipalities_per_pref),
        (
            "pref-assembly-districts-per-pref",
            options.pref_assembly_districts_per_pref,
        ),
    ] {
        if value == 0 {
            bail!("--{name} は 1 以上が必要です");
        }
    }
    if options.districts_per_pref > 99 || options.pref_assembly_districts_per_pref > 99 {
        bail!("小選挙区・都道府県議会の選挙区の数は、2 桁（99 以下）にしてください");
    }
    if options.municipalities_per_pref > 999 {
        bail!("--municipalities-per-pref は 3 桁（999 以下）にしてください");
    }
    Ok(options)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    if let Some(position) = args.iter().position(|a| a == "--check") {
        return check(&args, position);
    }
    let options = parse_args(&args).map_err(|e| anyhow::anyhow!("{e}\n{USAGE}"))?;
    let summary = generate::generate(&options)?;
    println!(
        "{} を生成しました: 選挙区 {} 件・候補者 {} 人・有権者 {} 人（選挙の種類 {}）",
        summary.dir.display(),
        summary.districts,
        summary.candidates,
        summary.voters,
        summary.types
    );
    Ok(())
}

/// `--check DIR [--election-id ID]`: 既存の選挙データを、`seed` クレートの検証にかける。
fn check(args: &[String], position: usize) -> anyhow::Result<()> {
    let dir = args
        .get(position + 1)
        .with_context(|| format!("--check にはディレクトリが必要です\n{USAGE}"))?;
    let election_id = args
        .iter()
        .position(|a| a == "--election-id")
        .and_then(|i| args.get(i + 1))
        .map_or("2026-general", String::as_str);
    let data = seed::load(std::path::Path::new(dir), election_id)?;
    println!(
        "OK: {dir}/{election_id}: 選挙区 {} 件・候補者 {} 人・有権者 {} 人",
        data.election.contests().len(),
        data.election.candidate_count(),
        data.voters.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn defaults_are_47_prefectures_scale() {
        let o = parse_args(&args(&["--out", "x"])).expect("valid");
        assert_eq!(
            (
                o.prefectures,
                o.districts_per_pref,
                o.candidates_per_district
            ),
            (47, 6, 8)
        );
        assert_eq!(o.election_id, "2026-general");
        assert!(!o.force);
    }

    #[test]
    fn every_option_is_parsed() {
        let o = parse_args(&args(&[
            "--out",
            "d",
            "--election-id",
            "2027-local",
            "--prefectures",
            "3",
            "--districts-per-pref",
            "2",
            "--candidates-per-district",
            "4",
            "--voters",
            "50",
            "--municipalities-per-pref",
            "5",
            "--pref-assembly-districts-per-pref",
            "2",
            "--voter-prefix",
            "v",
            "--seed",
            "9",
            "--force",
        ]))
        .expect("valid");
        assert_eq!(
            (
                o.prefectures,
                o.districts_per_pref,
                o.candidates_per_district,
                o.voters
            ),
            (3, 2, 4, 50)
        );
        assert_eq!(
            (
                o.municipalities_per_pref,
                o.pref_assembly_districts_per_pref
            ),
            (5, 2)
        );
        assert_eq!((o.voter_prefix.as_str(), o.seed, o.force), ("v", 9, true));
        assert_eq!(o.election_id, "2027-local");
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            vec![],
            vec!["--out", "d", "--prefectures", "0"],
            vec!["--out", "d", "--prefectures", "48"],
            vec!["--out", "d", "--districts-per-pref", "0"],
            vec!["--out", "d", "--districts-per-pref", "100"],
            vec!["--out", "d", "--candidates-per-district", "x"],
            vec!["--out", "d", "--unknown", "1"],
            vec!["--out"],
            vec!["out", "d"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }
}
