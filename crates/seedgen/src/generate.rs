//! 選挙データの生成。乱数は自前の小さな生成器（同じ `--seed` なら、同じデータになる）。

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use domain::ids::PREFECTURE_NAMES;
use domain::{CandidateId, DistrictId};

use crate::Options;

/// 生成した規模。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub dir: PathBuf,
    pub types: usize,
    pub districts: usize,
    pub candidates: usize,
    pub voters: u64,
}

/// 選挙の種類: (コード, 表示名, 表示順)。
const TYPES: [(&str, &str, u32); 9] = [
    ("shugiin_smd", "衆議院小選挙区選挙", 10),
    ("shugiin_pr", "衆議院比例代表選挙", 20),
    ("sangiin_district", "参議院選挙区選挙", 30),
    ("sangiin_pr", "参議院比例代表選挙", 40),
    ("governor", "都道府県知事選挙", 50),
    ("pref_assembly", "都道府県議会議員選挙", 60),
    ("municipal_head", "市区町村長選挙", 70),
    ("municipal_assembly", "市区町村議会議員選挙", 80),
    ("supreme_court_review", "最高裁判所裁判官国民審査", 90),
];

/// 衆議院の比例ブロック: (ID の末尾, 表示名, 属する都道府県コード)。
const PR_BLOCKS: [(&str, &str, &[u8]); 11] = [
    ("hokkaido", "北海道ブロック", &[1]),
    ("tohoku", "東北ブロック", &[2, 3, 4, 5, 6, 7]),
    ("kita_kanto", "北関東ブロック", &[8, 9, 10, 11]),
    ("minami_kanto", "南関東ブロック", &[12, 14, 19]),
    ("tokyo", "東京ブロック", &[13]),
    (
        "hokuriku_shinetsu",
        "北陸信越ブロック",
        &[15, 16, 17, 18, 20],
    ),
    ("tokai", "東海ブロック", &[21, 22, 23, 24]),
    ("kinki", "近畿ブロック", &[25, 26, 27, 28, 29, 30]),
    ("chugoku", "中国ブロック", &[31, 32, 33, 34, 35]),
    ("shikoku", "四国ブロック", &[36, 37, 38, 39]),
    ("kyushu", "九州ブロック", &[40, 41, 42, 43, 44, 45, 46, 47]),
];

/// 参議院の合区（1 つの選挙区が複数の都道府県にまたがる）: (ID の末尾, 表示名, 都道府県コード)。
const MERGED_SANGIIN: [(&str, &str, [u8; 2]); 2] = [
    ("31_32", "鳥取県・島根県選挙区（合区）", [31, 32]),
    ("36_39", "徳島県・高知県選挙区（合区）", [36, 39]),
];

const SURNAMES: [&str; 20] = [
    "佐藤",
    "鈴木",
    "高橋",
    "田中",
    "伊藤",
    "渡辺",
    "山本",
    "中村",
    "小林",
    "加藤",
    "吉田",
    "山田",
    "佐々木",
    "山口",
    "松本",
    "井上",
    "木村",
    "林",
    "斎藤",
    "清水",
];
const GIVEN: [&str; 20] = [
    "太郎", "花子", "健", "美咲", "一郎", "恵", "誠", "愛", "大輔", "由美", "翔", "陽子", "拓也",
    "彩", "和也", "真理", "浩二", "香織", "隆", "理恵",
];
const PARTIES: [&str; 8] = [
    "未来党",
    "みらい共和",
    "緑の会",
    "公正連合",
    "新風党",
    "国民ひろば",
    "地域の会",
    "無所属",
];
const JOBS: [&str; 8] = [
    "元会社員",
    "元教員",
    "弁護士",
    "医師",
    "元市議会議員",
    "農業",
    "NPO 法人代表",
    "元公務員",
];

/// splitmix64。同じ種なら同じ列（再現できるダミーデータのため）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
}

/// 生成する選挙区（表示順の `order` は、種類の中での通し番号）。
struct GenDistrict {
    id: String,
    election_type: &'static str,
    name: String,
    prefectures: Vec<u8>,
    order: u32,
}

/// 都道府県 `p`（1 始まり）が属する、衆議院の比例ブロックの ID の末尾。
fn pr_block_of(p: u8) -> Option<&'static str> {
    PR_BLOCKS
        .iter()
        .find(|(_, _, prefs)| prefs.contains(&p))
        .map(|(id, _, _)| *id)
}

/// 都道府県 `p` の参議院選挙区の ID の末尾（合区に含まれる県は、合区の ID）。
fn sangiin_district_of(p: u8, n: u32) -> String {
    for (id, _, pair) in MERGED_SANGIIN {
        if pair.contains(&p) && pair.iter().all(|q| u32::from(*q) <= n) {
            return id.to_string();
        }
    }
    format!("{p:02}")
}

fn build_districts(o: &Options) -> Vec<GenDistrict> {
    let n = o.prefectures;
    let prefs: Vec<u8> = (1..=n as u8).collect();
    let pref_name = |p: u8| PREFECTURE_NAMES[usize::from(p) - 1];
    let mut out = Vec::new();
    let mut push =
        |id: String, ty: &'static str, name: String, prefectures: Vec<u8>, order: u32| {
            out.push(GenDistrict {
                id,
                election_type: ty,
                name,
                prefectures,
                order,
            });
        };

    // 衆議院小選挙区。
    let mut order = 0;
    for &p in &prefs {
        for k in 1..=o.districts_per_pref {
            order += 1;
            push(
                format!("shugiin_smd.{p:02}.{k:02}"),
                "shugiin_smd",
                format!("{}{k}区", pref_name(p)),
                vec![p],
                order,
            );
        }
    }
    // 衆議院比例代表（ブロックは、複数の都道府県にまたがる）。
    let mut order = 0;
    for (id, name, block_prefs) in PR_BLOCKS {
        let present: Vec<u8> = block_prefs
            .iter()
            .copied()
            .filter(|p| u32::from(*p) <= n)
            .collect();
        if !present.is_empty() {
            order += 1;
            push(
                format!("shugiin_pr.{id}"),
                "shugiin_pr",
                format!("比例代表 {name}"),
                present,
                order,
            );
        }
    }
    // 参議院選挙区（合区は、複数の都道府県にまたがる 1 つの選挙区）。
    let mut order = 0;
    let mut merged_done: Vec<&str> = Vec::new();
    for &p in &prefs {
        let tail = sangiin_district_of(p, n);
        if let Some((id, name, pair)) = MERGED_SANGIIN.iter().find(|(id, _, _)| *id == tail) {
            if merged_done.contains(id) {
                continue;
            }
            merged_done.push(id);
            order += 1;
            push(
                format!("sangiin_district.{id}"),
                "sangiin_district",
                (*name).to_string(),
                pair.to_vec(),
                order,
            );
        } else {
            order += 1;
            push(
                format!("sangiin_district.{tail}"),
                "sangiin_district",
                format!("{}選挙区", pref_name(p)),
                vec![p],
                order,
            );
        }
    }
    // 参議院比例代表・国民審査（全国）。
    push(
        "sangiin_pr.national".to_string(),
        "sangiin_pr",
        "参議院比例代表（全国）".to_string(),
        prefs.clone(),
        1,
    );
    // 知事。
    for (i, &p) in prefs.iter().enumerate() {
        push(
            format!("governor.{p:02}"),
            "governor",
            format!("{}知事", pref_name(p)),
            vec![p],
            i as u32 + 1,
        );
    }
    // 都道府県議会・市区町村長・市区町村議会。
    let (mut a, mut h, mut m) = (0, 0, 0);
    for &p in &prefs {
        for k in 1..=o.pref_assembly_districts_per_pref {
            a += 1;
            push(
                format!("pref_assembly.{p:02}.{k:02}"),
                "pref_assembly",
                format!("{}議会 第{k}選挙区", pref_name(p)),
                vec![p],
                a,
            );
        }
        for k in 1..=o.municipalities_per_pref {
            h += 1;
            push(
                format!("municipal_head.{p:02}.{k:03}"),
                "municipal_head",
                format!("{}第{k}市区町村長", pref_name(p)),
                vec![p],
                h,
            );
            m += 1;
            push(
                format!("municipal_assembly.{p:02}.{k:03}"),
                "municipal_assembly",
                format!("{}第{k}市区町村議会", pref_name(p)),
                vec![p],
                m,
            );
        }
    }
    push(
        "supreme_court_review.national".to_string(),
        "supreme_court_review",
        "最高裁判所裁判官国民審査（全国）".to_string(),
        prefs,
        1,
    );
    out
}

/// 有権者 `n`（0 始まり）の、属する選挙区のリスト。都道府県は順に割り当て、その中の選挙区も巡回させる。
fn voter_districts(o: &Options, n: u64) -> Vec<String> {
    let np = u64::from(o.prefectures);
    let p = (n % np) as u8 + 1;
    let round = n / np;
    let smd = round % u64::from(o.districts_per_pref) + 1;
    let assembly = round % u64::from(o.pref_assembly_districts_per_pref) + 1;
    let municipality = round % u64::from(o.municipalities_per_pref) + 1;
    let mut districts = vec![format!("shugiin_smd.{p:02}.{smd:02}")];
    if let Some(block) = pr_block_of(p) {
        districts.push(format!("shugiin_pr.{block}"));
    }
    districts.push(format!(
        "sangiin_district.{}",
        sangiin_district_of(p, o.prefectures)
    ));
    districts.push("sangiin_pr.national".to_string());
    districts.push(format!("governor.{p:02}"));
    districts.push(format!("pref_assembly.{p:02}.{assembly:02}"));
    districts.push(format!("municipal_head.{p:02}.{municipality:03}"));
    districts.push(format!("municipal_assembly.{p:02}.{municipality:03}"));
    districts.push("supreme_court_review.national".to_string());
    districts
}

fn candidate_row(rng: &mut Rng, district: &GenDistrict, seq: u64) -> anyhow::Result<[String; 5]> {
    let district_id = DistrictId::new(&district.id).context("選挙区 ID を作れません")?;
    let id = CandidateId::new(&district_id, seq).context("候補者 ID を作れません")?;
    let party = *rng.pick(&PARTIES);
    // 比例代表・国民審査でも、候補者（政党名・裁判官名）として、同じ形式で扱う。
    let name = format!("{} {}", rng.pick(&SURNAMES), rng.pick(&GIVEN));
    let age = 30 + rng.next() % 45;
    Ok([
        id.to_string(),
        district.id.clone(),
        name,
        party.to_string(),
        format!("{age}歳。{}。（ダミーの略歴）", rng.pick(&JOBS)),
    ])
}

/// データ一式を書いて、`seed` クレートで検証する。
pub fn generate(o: &Options) -> anyhow::Result<Summary> {
    let dir = o.out.join(&o.election_id);
    if dir.join("election.toml").exists() && !o.force {
        bail!(
            "{} に、すでにデータがあります（上書きするなら --force）",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir.join("candidates"))
        .with_context(|| format!("{} を作れません", dir.display()))?;
    // 上書きのとき、前回の候補者ファイルが残らないようにする。
    for entry in std::fs::read_dir(dir.join("candidates"))?.flatten() {
        if entry.path().extension().is_some_and(|e| e == "csv") {
            std::fs::remove_file(entry.path())?;
        }
    }

    write_election_toml(&dir, o)?;
    let districts = build_districts(o);
    write_districts(&dir, &districts)?;
    let candidates = write_candidates(&dir, &districts, o)?;
    write_voters(&dir, o)?;

    // 生成したデータが、読み込みの検証（ID の形式・重複・参照）を通ること。
    seed::load(&o.out, &o.election_id)
        .context("生成したデータが、検証を通りません（seedgen の不具合）")?;
    Ok(Summary {
        dir,
        types: TYPES.len(),
        districts: districts.len(),
        candidates,
        voters: o.voters,
    })
}

fn write_election_toml(dir: &Path, o: &Options) -> anyhow::Result<()> {
    let mut text = format!(
        "# seedgen が生成したダミーの選挙データ（{} 都道府県）。手で編集せず、seedgen で作り直す。\nid = \"{}\"\nname = \"ダミー統一選挙（{} 都道府県）\"\n\n",
        o.prefectures, o.election_id, o.prefectures
    );
    for (code, name, order) in TYPES {
        text.push_str(&format!(
            "[[types]]\ncode = \"{code}\"\nname = \"{name}\"\norder = {order}\nmethod = \"single_choice\"\n\n"
        ));
    }
    std::fs::write(dir.join("election.toml"), text).context("election.toml を書けません")
}

fn write_districts(dir: &Path, districts: &[GenDistrict]) -> anyhow::Result<()> {
    let mut w =
        csv::Writer::from_path(dir.join("districts.csv")).context("districts.csv を作れません")?;
    w.write_record([
        "district_id",
        "election_type",
        "name",
        "prefectures",
        "order",
    ])?;
    for d in districts {
        let prefectures: Vec<String> = d.prefectures.iter().map(|p| format!("{p:02}")).collect();
        w.write_record([
            d.id.as_str(),
            d.election_type,
            d.name.as_str(),
            &prefectures.join(";"),
            &d.order.to_string(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

/// 選挙の種類ごとの CSV を書く。候補者の総数を返す。
fn write_candidates(dir: &Path, districts: &[GenDistrict], o: &Options) -> anyhow::Result<usize> {
    let mut rng = Rng(o.seed);
    let mut total = 0;
    for (code, _, _) in TYPES {
        let path = dir.join("candidates").join(format!("{code}.csv"));
        let mut w = csv::Writer::from_path(&path)
            .with_context(|| format!("{} を作れません", path.display()))?;
        w.write_record(["candidate_id", "district_id", "name", "party", "profile"])?;
        for district in districts.iter().filter(|d| d.election_type == code) {
            for seq in 1..=u64::from(o.candidates_per_district) {
                w.write_record(candidate_row(&mut rng, district, seq)?)?;
                total += 1;
            }
        }
        w.flush()?;
    }
    Ok(total)
}

fn write_voters(dir: &Path, o: &Options) -> anyhow::Result<()> {
    let mut w =
        csv::Writer::from_path(dir.join("voters.csv")).context("voters.csv を作れません")?;
    w.write_record(["voter_id", "districts"])?;
    for n in 0..o.voters {
        w.write_record([
            format!("{}{}", o.voter_prefix, n + 1),
            voter_districts(o, n).join(";"),
        ])?;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use domain::{ContestId, VoterId};

    use super::*;

    fn options(out: &Path) -> Options {
        Options {
            out: out.to_path_buf(),
            election_id: "2026-general".to_string(),
            prefectures: 47,
            districts_per_pref: 6,
            candidates_per_district: 8,
            voters: 500,
            municipalities_per_pref: 10,
            pref_assembly_districts_per_pref: 4,
            voter_prefix: "voter-".to_string(),
            seed: 1,
            force: false,
        }
    }

    fn temp() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "seedgen-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_47_prefecture_dataset_has_over_ten_thousand_candidates_and_loads() {
        let out = temp();
        let summary = generate(&options(&out)).expect("generate");
        assert!(summary.candidates >= 10_000, "{}", summary.candidates);
        assert_eq!(summary.types, 9);
        let data = seed::load(&out, "2026-general").expect("valid");
        assert_eq!(data.election.candidate_count(), summary.candidates);
        assert_eq!(data.election.contests().len(), summary.districts);
        assert_eq!(data.voters.len(), 500);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn tokyo_district_1_voters_get_tokyo_and_national_ballots_only() {
        let out = temp();
        generate(&options(&out)).expect("generate");
        let data = seed::load(&out, "2026-general").expect("valid");
        // 東京 1 区（shugiin_smd.13.01）の有権者を探す。
        let (voter, districts) = data
            .voters
            .iter()
            .find(|(_, d)| d.iter().any(|x| x.as_str() == "shugiin_smd.13.01"))
            .expect("a Tokyo 1 voter exists");
        let ids: Vec<&str> = districts.iter().map(|d| d.as_str()).collect();
        assert_eq!(ids.len(), 9, "{voter:?} {ids:?}");
        assert!(ids.contains(&"shugiin_pr.tokyo"));
        assert!(ids.contains(&"sangiin_district.13"));
        assert!(ids.contains(&"governor.13"));
        assert!(ids.contains(&"sangiin_pr.national"));
        assert!(ids.contains(&"supreme_court_review.national"));
        // 他の都道府県の選挙区は含まれない。
        assert!(
            ids.iter()
                .all(|d| !d.contains(".27.") && !d.ends_with(".27"))
        );
        assert!(
            data.voters
                .get(&VoterId::new("voter-13").expect("valid"))
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn merged_sangiin_districts_span_two_prefectures_with_one_id() {
        let out = temp();
        generate(&options(&out)).expect("generate");
        let data = seed::load(&out, "2026-general").expect("valid");
        let contest = data
            .election
            .contest(&ContestId::parse("2026-general/sangiin_district.31_32").expect("valid"))
            .expect("merged district");
        assert_eq!(contest.district.prefectures, vec!["31", "32"]);
        // 合区に含まれる県には、県ごとの選挙区は無い。
        assert!(
            data.election
                .contest(&ContestId::parse("2026-general/sangiin_district.31").expect("valid"))
                .is_none()
        );
        // 比例ブロックも複数の都道府県にまたがる。
        let kinki = data
            .election
            .contest(&ContestId::parse("2026-general/shugiin_pr.kinki").expect("valid"))
            .expect("block");
        assert_eq!(kinki.district.prefectures.len(), 6);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn small_datasets_and_reproducibility() {
        let (a, b) = (temp(), temp());
        let mut small = options(&a);
        small.prefectures = 2;
        small.voters = 20;
        generate(&small).expect("generate small");
        let data = seed::load(&a, "2026-general").expect("valid");
        assert!(data.election.contests().len() < 100);
        // 同じ引数・同じ種なら、同じデータ。
        let mut again = small.clone();
        again.out = b.clone();
        generate(&again).expect("generate again");
        for file in ["districts.csv", "voters.csv", "candidates/governor.csv"] {
            assert_eq!(
                std::fs::read(a.join("2026-general").join(file)).expect("read"),
                std::fs::read(b.join("2026-general").join(file)).expect("read"),
                "{file}"
            );
        }
        // 既存のデータは、--force なしでは上書きしない。
        assert!(generate(&small).is_err());
        small.force = true;
        assert!(generate(&small).is_ok());
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }
}
