//! 読み込みと検証のテスト。正しいデータを一時ディレクトリに書き、1 か所ずつ壊して、
//! 「どのファイルの何行目がなぜ不正か」が出ることを確認する。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use domain::{ContestId, DistrictId, VoterId};

use super::*;

/// テスト用の一時ディレクトリ（終了時に消す）。ディレクトリ名 `2026-general` の下にデータを置く。
struct TempSeed {
    root: PathBuf,
}

impl TempSeed {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "seed-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("2026-general/candidates")).expect("create dirs");
        Self { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("2026-general")
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir().join(name), text).expect("write file");
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir().join(name)
    }
}

impl Drop for TempSeed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const ELECTION_TOML: &str = r#"id = "2026-general"
name = "テスト選挙"

[[types]]
code = "shugiin_smd"
name = "衆議院小選挙区"
order = 20
method = "single_choice"

[[types]]
code = "governor"
name = "知事選挙"
order = 10
method = "single_choice"
"#;

const DISTRICTS: &str = "district_id,election_type,name,prefectures,order
shugiin_smd.13.01,shugiin_smd,東京1区,13,1
shugiin_smd.13.02,shugiin_smd,東京2区,13,2
shugiin_smd.31_32.01,shugiin_smd,鳥取・島根1区,31;32,3
governor.13,governor,東京都知事,13,1
";

const SMD_CANDIDATES: &str = "candidate_id,district_id,name,party,profile
shugiin_smd.13.01.c1,shugiin_smd.13.01,山田 太郎,A党,元会社員
shugiin_smd.13.01.c2,shugiin_smd.13.01,鈴木 花子,B党,
shugiin_smd.13.02.c1,shugiin_smd.13.02,佐藤 健,無所属,
shugiin_smd.31_32.01.c1,shugiin_smd.31_32.01,高橋 美咲,A党,
";

const GOVERNOR_CANDIDATES: &str = "candidate_id,district_id,name,party,profile
governor.13.c1,governor.13,田中 一郎,無所属,
governor.13.c2,governor.13,伊藤 二郎,無所属,
";

const VOTERS: &str = "voter_id,districts
alice,shugiin_smd.13.01;governor.13
bob,shugiin_smd.31_32.01
";

/// 正しいデータ一式を書く。
fn valid() -> TempSeed {
    let seed = TempSeed::new();
    seed.write("election.toml", ELECTION_TOML);
    seed.write("districts.csv", DISTRICTS);
    seed.write("candidates/shugiin_smd.csv", SMD_CANDIDATES);
    seed.write("candidates/governor.csv", GOVERNOR_CANDIDATES);
    seed.write("voters.csv", VOTERS);
    seed
}

fn error_text(result: Result<SeedData, SeedError>) -> String {
    result.expect_err("should be invalid").to_string()
}

fn load_dir(seed: &TempSeed) -> Result<SeedData, SeedError> {
    load(&seed.root, "2026-general")
}

#[test]
fn a_valid_dataset_loads_in_display_order() {
    let seed = valid();
    let data = load_dir(&seed).expect("valid");
    let e = &data.election;
    assert_eq!(e.id().as_str(), "2026-general");
    assert_eq!(e.candidate_count(), 6);
    // 種類の表示順（知事 10 < 小選挙区 20）、次に選挙区の順。
    let ids: Vec<&str> = e.contests().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "2026-general/governor.13",
            "2026-general/shugiin_smd.13.01",
            "2026-general/shugiin_smd.13.02",
            "2026-general/shugiin_smd.31_32.01",
        ]
    );
    // 候補者の属性と、合区の都道府県のリスト。
    let contest = e
        .contest(&ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"))
        .expect("exists");
    assert_eq!(contest.candidates[0].name, "山田 太郎");
    assert_eq!(contest.candidates[0].party, "A党");
    assert_eq!(contest.candidates[0].profile, "元会社員");
    let merged = e
        .contest_for_district(&DistrictId::new("shugiin_smd.31_32.01").expect("valid"))
        .expect("exists");
    assert_eq!(merged.district.prefectures, vec!["31", "32"]);
    // 有権者と選挙区の対応。
    assert_eq!(data.voters.len(), 2);
    let alice = data
        .voters
        .get(&VoterId::new("alice").expect("valid"))
        .expect("alice");
    let alice: Vec<&str> = alice.iter().map(DistrictId::as_str).collect();
    assert_eq!(alice, vec!["shugiin_smd.13.01", "governor.13"]);
    assert!(
        data.voters
            .get(&VoterId::new("nobody").expect("valid"))
            .is_none()
    );
}

#[test]
fn spreadsheet_friendly_csv_is_accepted() {
    // 列の順序の入れ替え、余分な空白、空行、BOM、任意の列（party / profile）の省略。
    let seed = valid();
    seed.write(
        "districts.csv",
        "\u{feff}name , order,prefectures,election_type,district_id\n\n東京1区,1,13,shugiin_smd, shugiin_smd.13.01\n東京2区,2,13,shugiin_smd,shugiin_smd.13.02\n鳥取・島根1区,3,31;32,shugiin_smd,shugiin_smd.31_32.01\n東京都知事,1,13,governor,governor.13\n",
    );
    seed.write(
        "candidates/governor.csv",
        "candidate_id,district_id,name\ngovernor.13.c1,governor.13,田中 一郎\n",
    );
    let data = load_dir(&seed).expect("valid");
    assert_eq!(data.election.candidate_count(), 5);
}

#[test]
fn a_candidate_number_may_have_any_number_of_digits() {
    let seed = valid();
    let mut rows = String::from("candidate_id,district_id,name\n");
    for n in [1u64, 2, 10, 100, 12_345] {
        rows.push_str(&format!("governor.13.c{n},governor.13,候補 {n}\n"));
    }
    seed.write("candidates/governor.csv", &rows);
    assert!(load_dir(&seed).is_ok());
}

#[test]
fn duplicate_ids_are_reported_with_file_and_line() {
    // 選挙区の重複。
    let seed = valid();
    seed.write(
        "districts.csv",
        &format!("{DISTRICTS}shugiin_smd.13.01,shugiin_smd,東京1区（重複）,13,9\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(text.contains("districts.csv:6"), "{text}");
    assert!(
        text.contains("重複") && text.contains("shugiin_smd.13.01"),
        "{text}"
    );
    assert!(text.contains("2 行目"), "初出の行番号: {text}");

    // 候補者の重複（同じファイル内）。
    let seed = valid();
    seed.write(
        "candidates/governor.csv",
        &format!("{GOVERNOR_CANDIDATES}governor.13.c1,governor.13,別人,無所属,\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("governor.csv:4") && text.contains("governor.13.c1") && text.contains("重複"),
        "{text}"
    );

    // 候補者の重複（別のファイルにまたがる。ID が同じなら、選挙区も同じなので、種類の不一致としても検出される）。
    let seed = valid();
    seed.write(
        "candidates/shugiin_smd.csv",
        &format!("{SMD_CANDIDATES}governor.13.c1,governor.13,別人,無所属,\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(text.contains("重複") || text.contains("種類"), "{text}");

    // 有権者の重複。
    let seed = valid();
    seed.write("voters.csv", &format!("{VOTERS}alice,governor.13\n"));
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("voters.csv:4") && text.contains("alice") && text.contains("重複"),
        "{text}"
    );

    // 選挙の種類の重複（election.toml）。
    let seed = valid();
    seed.write(
        "election.toml",
        &format!("{ELECTION_TOML}\n[[types]]\ncode = \"governor\"\nname = \"重複\"\norder = 1\nmethod = \"single_choice\"\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("election.toml") && text.contains("重複") && text.contains("governor"),
        "{text}"
    );
}

#[test]
fn dangling_references_are_reported_with_file_and_line() {
    // 存在しない選挙区を参照する候補者。
    let seed = valid();
    seed.write(
        "candidates/governor.csv",
        &format!("{GOVERNOR_CANDIDATES}governor.99.c1,governor.99,幽霊,無所属,\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(text.contains("governor.csv:4"), "{text}");
    assert!(
        text.contains("存在しない選挙区") && text.contains("governor.99"),
        "{text}"
    );

    // 存在しない選挙の種類の選挙区。
    let seed = valid();
    seed.write(
        "districts.csv",
        &format!("{DISTRICTS}sangiin_pr.national,sangiin_pr,全国比例,13,1\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("districts.csv:6")
            && text.contains("sangiin_pr")
            && text.contains("election.toml"),
        "{text}"
    );

    // 存在しない選挙区に属する有権者。
    let seed = valid();
    seed.write("voters.csv", &format!("{VOTERS}carol,shugiin_smd.99.01\n"));
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("voters.csv:4")
            && text.contains("carol")
            && text.contains("存在しない選挙区"),
        "{text}"
    );

    // 選挙の種類が未定義のファイル名。
    let seed = valid();
    seed.write("candidates/unknown_type.csv", GOVERNOR_CANDIDATES);
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("unknown_type.csv") && text.contains("選挙の種類のコードではありません"),
        "{text}"
    );
}

#[test]
fn inconsistent_rows_are_reported() {
    // 候補者 ID の選挙区部分と district_id が違う。
    let seed = valid();
    seed.write(
        "candidates/shugiin_smd.csv",
        &format!("{SMD_CANDIDATES}shugiin_smd.13.01.c9,shugiin_smd.13.02,取り違え,無所属,\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("shugiin_smd.csv:6") && text.contains("一致しません"),
        "{text}"
    );

    // 選挙の種類が違う選挙区の候補者を、別の種類のファイルに書いた。
    let seed = valid();
    seed.write(
        "candidates/governor.csv",
        &format!(
            "{GOVERNOR_CANDIDATES}shugiin_smd.13.01.c9,shugiin_smd.13.01,置き場違い,無所属,\n"
        ),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("governor.csv:4") && text.contains("このファイル"),
        "{text}"
    );

    // 同じ種類の選挙区に 2 つ属する有権者。
    let seed = valid();
    seed.write(
        "voters.csv",
        &format!("{VOTERS}dave,shugiin_smd.13.01;shugiin_smd.13.02\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("voters.csv:4") && text.contains("複数"),
        "{text}"
    );

    // 候補者のいない選挙区。
    let seed = valid();
    seed.write(
        "candidates/shugiin_smd.csv",
        "candidate_id,district_id,name,party,profile\nshugiin_smd.13.01.c1,shugiin_smd.13.01,山田,A党,\n",
    );
    let text = error_text(load_dir(&seed));
    assert!(text.contains("候補者がいません"), "{text}");
}

#[test]
fn malformed_ids_and_values_are_reported() {
    for (file, content, needle) in [
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\nShugiin.13,shugiin_smd,x,13,1\n",
            "district_id",
        ),
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\nshugiin_smd.13.01,shugiin_smd,x,48,1\n",
            "都道府県コード",
        ),
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\nshugiin_smd.13.01,shugiin_smd,x,,1\n",
            "prefectures が空",
        ),
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\nshugiin_smd.13.01,shugiin_smd,x,13,first\n",
            "order",
        ),
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\nshugiin_smd.13.01,shugiin_smd,,13,1\n",
            "name が空",
        ),
        (
            "districts.csv",
            "district_id,election_type,name,prefectures,order\ngovernor.13,shugiin_smd,x,13,1\n",
            "で始まっていません",
        ),
        (
            "candidates/governor.csv",
            "candidate_id,district_id,name\ngovernor.13.c01,governor.13,x\n",
            "candidate_id",
        ),
        (
            "candidates/governor.csv",
            "candidate_id,district_id,name\ngovernor.13.c1,governor.13,\n",
            "name が空",
        ),
        (
            "voters.csv",
            "voter_id,districts\nbad id,governor.13\n",
            "voter_id",
        ),
    ] {
        let seed = valid();
        seed.write(file, content);
        let text = error_text(load_dir(&seed));
        assert!(
            text.contains(file.rsplit('/').next().unwrap_or(file)),
            "{file}: {text}"
        );
        assert!(text.contains(needle), "{file} / {needle}: {text}");
    }
}

#[test]
fn too_long_ids_are_rejected() {
    let seed = valid();
    let long = format!("governor.{}", "x".repeat(60)); // 69 文字
    seed.write(
        "districts.csv",
        &format!("{DISTRICTS}{long},governor,長すぎる,13,9\n"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("districts.csv:6") && text.contains("長すぎます"),
        "{text}"
    );
}

#[test]
fn csv_structure_errors_name_the_file_and_line() {
    // ヘッダの列が足りない / 未知の列 / 列数が合わない行。
    let seed = valid();
    seed.write("districts.csv", "district_id,name\nx,y\n");
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("districts.csv:1") && text.contains("必須の列"),
        "{text}"
    );

    let seed = valid();
    seed.write(
        "districts.csv",
        "district_id,election_type,name,prefectures,order,extra\nshugiin_smd.13.01,shugiin_smd,x,13,1,z\n",
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("districts.csv:1") && text.contains("未知の列"),
        "{text}"
    );

    let seed = valid();
    seed.write("voters.csv", "voter_id,districts\nalice,governor.13\nbob\n");
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("voters.csv:3") && text.contains("列の数"),
        "{text}"
    );
}

#[test]
fn election_toml_problems_are_reported() {
    let seed = valid();
    seed.write(
        "election.toml",
        "id = \"other-election\"\nname = \"x\"\ntypes = []\n",
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("ディレクトリ名") && text.contains("types が 1 件もありません"),
        "{text}"
    );

    let seed = valid();
    seed.write(
        "election.toml",
        &ELECTION_TOML.replace("single_choice", "ranked_choice"),
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("ranked_choice") && text.contains("single_choice"),
        "{text}"
    );

    let seed = valid();
    seed.write(
        "election.toml",
        "id = \"2026-general\"\nname = \"x\"\nunknown = 1\ntypes = []\n",
    );
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("election.toml") && text.contains("unknown"),
        "{text}"
    );

    // ファイルが無い。
    let seed = valid();
    std::fs::remove_file(seed.path("districts.csv")).expect("remove");
    let text = error_text(load_dir(&seed));
    assert!(
        text.contains("districts.csv") && text.contains("読み込めません"),
        "{text}"
    );
}

#[test]
fn many_problems_are_reported_together_with_a_cap() {
    let seed = valid();
    let mut rows = String::from("candidate_id,district_id,name\n");
    for n in 1..=40 {
        rows.push_str(&format!("governor.99.c{n},governor.99,幽霊{n}\n"));
    }
    seed.write("candidates/governor.csv", &rows);
    let text = error_text(load_dir(&seed));
    assert!(text.contains("（40 件）"), "{text}");
    assert!(text.contains("ほか 10 件"), "{text}");
}

#[test]
fn the_repository_sample_seed_loads() {
    // リポジトリの seed/2026-general（手書きの小さなサンプル）が、読み込めること。
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../seed");
    let data = load(&root, "2026-general").expect("sample seed is valid");
    assert!(data.election.contests().len() >= 8);
    assert!(!data.voters.is_empty());
}
