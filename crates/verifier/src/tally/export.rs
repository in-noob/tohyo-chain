//! 集計の CSV / JSON 出力（`out/tally/{日時}/`）。日時は UTC（`YYYYMMDDTHHMMSSZ`）。
//!
//! 出力先のディレクトリとファイルは、すでにあれば上書きせず、失敗する（過去の集計を消さない）。

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

use super::compute::{DistrictTally, GroupTotal, Tally};
use super::gate::Phase;
use super::render::rank;
use super::{Meta, Reconciliation};
use shared_types::time::{utc_compact, utc_iso};

#[derive(Serialize)]
struct Labels<'a> {
    ballot_item: &'a str,
}

#[derive(Serialize)]
struct Document<'a> {
    election_id: &'a str,
    election_name: &'a str,
    generated_at: String,
    /// 締切前（または締切未設定）の中間集計なら真。
    interim: bool,
    voting_closes_at: Option<&'a str>,
    labels: Labels<'a>,
    districts: &'a [DistrictTally],
    prefectures: &'a [GroupTotal],
    types: &'a [GroupTotal],
    total: GroupTotal,
    reconciliation: &'a Reconciliation,
}

fn create_new(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "{} を作成できません（すでにあれば上書きしません）",
                path.display()
            )
        })
}

fn csv_file(dir: &Path, name: &str) -> anyhow::Result<csv::Writer<File>> {
    Ok(csv::Writer::from_writer(create_new(&dir.join(name))?))
}

fn group_csv(
    dir: &Path,
    name: &str,
    key_header: &str,
    groups: &[GroupTotal],
) -> anyhow::Result<()> {
    let mut w = csv_file(dir, name)?;
    w.write_record([
        key_header,
        "名称",
        "複数の都道府県にまたがる",
        "選挙区数",
        "有効票",
        "白票（無効票）",
        "合計",
        "投票済み数",
    ])?;
    for g in groups {
        w.write_record([
            g.key.clone(),
            g.name.clone(),
            g.wide.to_string(),
            g.districts.to_string(),
            g.valid.to_string(),
            g.blank.to_string(),
            g.total.to_string(),
            g.participation.to_string(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

/// `base/{日時}/` を作り、CSV 5 つと JSON を書く。作ったディレクトリを返す。
pub fn write_all(
    base: &Path,
    tally: &Tally,
    recon: &Reconciliation,
    meta: &Meta<'_>,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(base).with_context(|| format!("{} を作成できません", base.display()))?;
    let dir = base.join(utc_compact(meta.generated_at_unix));
    fs::create_dir(&dir).with_context(|| {
        format!(
            "{} を作成できません（すでにあれば上書きしません）",
            dir.display()
        )
    })?;

    // 選挙区ごとの合計。
    let mut w = csv_file(&dir, "districts.csv")?;
    w.write_record([
        "contest_id",
        "district_id",
        "選挙区",
        "選挙の種類",
        "都道府県",
        "有効票",
        "白票（無効票）",
        "合計",
        "投票済み者数",
    ])?;
    for d in &tally.districts {
        w.write_record([
            d.contest_id.clone(),
            d.district_id.clone(),
            d.name.clone(),
            d.type_name.clone(),
            d.prefectures.join(";"),
            d.valid.to_string(),
            d.blank.to_string(),
            d.total.to_string(),
            d.participation.to_string(),
        ])?;
    }
    w.flush()?;

    // 候補者別の得票（選挙区内は多い順）。
    let mut w = csv_file(&dir, "candidates.csv")?;
    w.write_record([
        "contest_id",
        "district_id",
        "選挙区",
        "順位",
        "candidate_id",
        "候補者",
        "政党",
        "得票数",
    ])?;
    for d in &tally.districts {
        for (i, c) in d.candidates.iter().enumerate() {
            w.write_record([
                d.contest_id.clone(),
                d.district_id.clone(),
                d.name.clone(),
                rank(d, i).to_string(),
                c.candidate_id.clone(),
                c.name.clone(),
                c.party.clone(),
                c.votes.to_string(),
            ])?;
        }
    }
    w.flush()?;

    group_csv(&dir, "prefectures.csv", "key", &tally.prefectures)?;
    group_csv(&dir, "types.csv", "election_type", &tally.types)?;

    let mut w = csv_file(&dir, "reconciliation.csv")?;
    w.write_record(["contest_id", "投票済み記録", "封印済み", "未封印", "一致"])?;
    for r in &recon.contests {
        w.write_record([
            r.contest_id.clone(),
            r.participation.to_string(),
            r.sealed.to_string(),
            r.pending.to_string(),
            r.consistent.to_string(),
        ])?;
    }
    w.flush()?;

    let document = Document {
        election_id: &tally.election_id,
        election_name: &tally.election_name,
        generated_at: utc_iso(meta.generated_at_unix),
        interim: meta.phase == Phase::Interim,
        voting_closes_at: meta.voting_closes_at,
        labels: Labels {
            ballot_item: meta.ballot_item,
        },
        districts: &tally.districts,
        prefectures: &tally.prefectures,
        types: &tally.types,
        total: tally.grand_total(),
        reconciliation: recon,
    };
    let json = create_new(&dir.join("tally.json"))?;
    serde_json::to_writer_pretty(json, &document).context("tally.json を書けません")?;
    Ok(dir)
}
