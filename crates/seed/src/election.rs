//! 選挙マスタ（election.toml・districts.csv・candidates/*.csv）の読み込みと検証。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use domain::{
    Candidate, CandidateCode, District, DistrictId, Election, ElectionId, ElectionType,
    ElectionTypeCode, VotingMethod,
};
use serde::Deserialize;

use crate::csv_table::{read_table, split_list};
use crate::{Issues, SeedError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ElectionToml {
    id: String,
    name: String,
    types: Vec<TypeToml>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeToml {
    code: String,
    name: String,
    order: u32,
    method: String,
}

/// `<dir>`（`seed/<election_id>/`）の選挙マスタを読んで検証する。
pub fn load_election(dir: &Path) -> Result<Election, SeedError> {
    let mut issues = Issues::default();
    let toml_path = dir.join("election.toml");
    let Some((id, name, types)) = read_election_toml(dir, &toml_path, &mut issues) else {
        return Err(issues.into_error());
    };
    let districts = read_districts(&dir.join("districts.csv"), &types, &mut issues);
    let candidates = read_candidates(&dir.join("candidates"), &types, &districts, &mut issues);
    if !issues.is_empty() {
        return Err(issues.into_error());
    }
    let districts: Vec<District> = districts.into_values().map(|(_, d)| d).collect();
    Election::new(id, name, types, districts, candidates).map_err(|e| {
        let mut issues = Issues::default();
        // 行を特定できない、全体の整合の問題（候補者のいない選挙区など）。
        issues.push(dir, None, e.to_string());
        issues.into_error()
    })
}

fn read_election_toml(
    dir: &Path,
    path: &Path,
    issues: &mut Issues,
) -> Option<(ElectionId, String, Vec<ElectionType>)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            issues.push(path, None, format!("読み込めません: {e}"));
            return None;
        }
    };
    let parsed: ElectionToml = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            issues.push(path, None, format!("TOML として不正です: {e}"));
            return None;
        }
    };
    let before = issues.len();
    let id = match ElectionId::new(&parsed.id) {
        Ok(id) => Some(id),
        Err(e) => {
            issues.push(path, None, format!("id: {e}"));
            None
        }
    };
    // ディレクトリ名は選挙の ID と一致させる（別の選挙のデータを読み違えないため）。
    if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str())
        && dir_name != parsed.id
    {
        issues.push(
            path,
            None,
            format!(
                "id {:?} が、ディレクトリ名 {dir_name:?} と一致しません",
                parsed.id
            ),
        );
    }
    if parsed.name.trim().is_empty() {
        issues.push(path, None, "name が空です");
    }
    let mut types = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, t) in parsed.types.iter().enumerate() {
        let n = i + 1;
        let code = match ElectionTypeCode::new(&t.code) {
            Ok(code) => Some(code),
            Err(e) => {
                issues.push(path, None, format!("types[{n}].code: {e}"));
                None
            }
        };
        if let Some(first) = seen.insert(t.code.clone(), n) {
            issues.push(
                path,
                None,
                format!(
                    "types[{n}].code {:?} が重複しています（types[{first}] と同じ）",
                    t.code
                ),
            );
        }
        if t.name.trim().is_empty() {
            issues.push(path, None, format!("types[{n}].name が空です"));
        }
        let Some(method) = VotingMethod::parse(&t.method) else {
            issues.push(
                path,
                None,
                format!(
                    "types[{n}].method {:?} は未対応です（single_choice だけが使えます）",
                    t.method
                ),
            );
            continue;
        };
        if let Some(code) = code {
            types.push(ElectionType {
                code,
                name: t.name.clone(),
                order: t.order,
                method,
            });
        }
    }
    if parsed.types.is_empty() {
        issues.push(path, None, "types が 1 件もありません");
    }
    if issues.len() > before {
        return None;
    }
    Some((id?, parsed.name, types))
}

/// districts.csv。`(行番号, 選挙区)` を、ID → 値で返す（読み込み順は保たない。表示順は `Election` が決める）。
fn read_districts(
    path: &Path,
    types: &[ElectionType],
    issues: &mut Issues,
) -> HashMap<DistrictId, (usize, District)> {
    let mut out: HashMap<DistrictId, (usize, District)> = HashMap::new();
    let Some(table) = read_table(
        path,
        &[
            "district_id",
            "election_type",
            "name",
            "prefectures",
            "order",
        ],
        &[],
        issues,
    ) else {
        return out;
    };
    for row in &table.rows {
        let line = row.line;
        let raw_id = table.get(row, "district_id");
        let id = match DistrictId::new(raw_id) {
            Ok(id) => id,
            Err(e) => {
                issues.push(path, Some(line), format!("district_id: {e}"));
                continue;
            }
        };
        if let Some((first, _)) = out.get(&id) {
            issues.push(
                path,
                Some(line),
                format!("選挙区 {id} が重複しています（{first} 行目と同じ ID）"),
            );
            continue;
        }
        let raw_type = table.get(row, "election_type");
        let election_type = match ElectionTypeCode::new(raw_type) {
            Ok(code) if types.iter().any(|t| t.code == code) => code,
            Ok(code) => {
                issues.push(
                    path,
                    Some(line),
                    format!("選挙区 {id} の選挙の種類 {code} が、election.toml にありません"),
                );
                continue;
            }
            Err(e) => {
                issues.push(path, Some(line), format!("election_type: {e}"));
                continue;
            }
        };
        if id.type_segment() != election_type.as_str() {
            issues.push(
                path,
                Some(line),
                format!("選挙区 {id} の ID が、選挙の種類 {election_type} で始まっていません"),
            );
            continue;
        }
        let name = table.get(row, "name");
        if name.is_empty() {
            issues.push(path, Some(line), format!("選挙区 {id} の name が空です"));
            continue;
        }
        let prefectures: Vec<String> = split_list(table.get(row, "prefectures"))
            .into_iter()
            .map(str::to_string)
            .collect();
        if prefectures.is_empty() {
            issues.push(
                path,
                Some(line),
                format!("選挙区 {id} の prefectures が空です（`;` 区切りの 2 桁コード）"),
            );
            continue;
        }
        if let Some(bad) = prefectures
            .iter()
            .find(|p| !domain::ids::is_prefecture_code(p))
        {
            issues.push(
                path,
                Some(line),
                format!("選挙区 {id} の都道府県コード {bad:?} が不正です（01〜47）"),
            );
            continue;
        }
        let order = match table.get(row, "order").parse::<u32>() {
            Ok(order) => order,
            Err(_) => {
                issues.push(
                    path,
                    Some(line),
                    format!(
                        "選挙区 {id} の order {:?} が整数ではありません",
                        table.get(row, "order")
                    ),
                );
                continue;
            }
        };
        out.insert(
            id.clone(),
            (
                line,
                District {
                    id,
                    election_type,
                    name: name.to_string(),
                    prefectures,
                    order,
                },
            ),
        );
    }
    out
}

/// candidates/<election_type>.csv をすべて読む。
fn read_candidates(
    dir: &Path,
    types: &[ElectionType],
    districts: &HashMap<DistrictId, (usize, District)>,
    issues: &mut Issues,
) -> Vec<Candidate> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "csv"))
            .collect(),
        Err(e) => {
            issues.push(dir, None, format!("候補者のディレクトリを読めません: {e}"));
            return Vec::new();
        }
    };
    files.sort();
    let mut out = Vec::new();
    // 候補者 ID → 初出の (ファイル, 行)。
    let mut seen: HashMap<CandidateCode, (PathBuf, usize)> = HashMap::new();
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Some(file_type) = types.iter().find(|t| t.code.as_str() == stem) else {
            issues.push(
                &path,
                None,
                format!(
                    "ファイル名 {stem:?} が、election.toml にある選挙の種類のコードではありません"
                ),
            );
            continue;
        };
        let Some(table) = read_table(
            &path,
            &["candidate_id", "district_id", "name"],
            &["party", "profile"],
            issues,
        ) else {
            continue;
        };
        for row in &table.rows {
            let line = row.line;
            // 白票の予約値 "blank" は、候補者コードとして拒否される（CandidateCode::parse）。
            let candidate_id = match CandidateCode::parse(table.get(row, "candidate_id")) {
                Ok(id) => id,
                Err(e) => {
                    issues.push(&path, Some(line), format!("candidate_id: {e}"));
                    continue;
                }
            };
            if let Some((first_file, first_line)) = seen.get(&candidate_id) {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "候補者 {candidate_id} が重複しています（{}:{first_line} と同じ ID）",
                        first_file.display()
                    ),
                );
                continue;
            }
            seen.insert(candidate_id.clone(), (path.clone(), line));
            let raw_district = table.get(row, "district_id");
            let district_id = match DistrictId::new(raw_district) {
                Ok(id) => id,
                Err(e) => {
                    issues.push(&path, Some(line), format!("district_id: {e}"));
                    continue;
                }
            };
            let Some((_, district)) = districts.get(&district_id) else {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "候補者 {candidate_id} が、存在しない選挙区 {district_id} を参照しています"
                    ),
                );
                continue;
            };
            if district.election_type != file_type.code {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "選挙区 {district_id} の選挙の種類は {} で、このファイル（{}）のものではありません",
                        district.election_type, file_type.code
                    ),
                );
                continue;
            }
            if candidate_id.district_part() != district_id.as_str() {
                issues.push(
                    &path,
                    Some(line),
                    format!(
                        "候補者 {candidate_id} の ID の選挙区部分が、district_id {district_id} と一致しません"
                    ),
                );
                continue;
            }
            let name = table.get(row, "name");
            if name.is_empty() {
                issues.push(
                    &path,
                    Some(line),
                    format!("候補者 {candidate_id} の name が空です"),
                );
                continue;
            }
            out.push(Candidate {
                id: candidate_id,
                name: name.to_string(),
                party: table.get(row, "party").to_string(),
                profile: table.get(row, "profile").to_string(),
            });
        }
    }
    out
}
