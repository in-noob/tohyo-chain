//! 選挙定義のハッシュ（ADR 0025）。
//!
//! seed から読み込んで検証した後の選挙定義（[`Election`]）を、ID・コード順に並べた固定長ビッグエンディアンの
//! バイナリに正規化して SHA-256 を取る（原則4。serde / JSON は使わない）。ジェネシスブロックのヘッダーに入れ、
//! 以後のブロックが引き継ぐので、init の後に seed の候補者名などを書き換えると、手元の seed と食い違って検出できる。
//!
//! ```text
//! SHA256("vote/election-definition/v1"
//!        ‖ str(election_id) ‖ str(name)
//!        ‖ u32(種類の数)   ‖ 種類ごと（code 順）:   str(code) ‖ str(name) ‖ u32(order) ‖ str(method)
//!        ‖ u32(選挙区の数) ‖ 選挙区ごと（ID 順）:   str(id) ‖ str(election_type) ‖ str(name)
//!                                                  ‖ u32(都道府県の数) ‖ str(都道府県)…（コード順） ‖ u32(order)
//!        ‖ u32(候補者の数) ‖ 候補者ごと（選挙区 ID 順、次に連番の数値順）:
//!                             str(id) ‖ str(name) ‖ str(party) ‖ str(profile))
//! str(s) = u32(UTF-8 のバイト数) ‖ UTF-8 のバイト列
//! ```
//!
//! 並べ直すので、ファイルの行・列の順番が違っても、同じ内容なら同じハッシュになる。有権者名簿は含めない。

use sha2::{Digest, Sha256};

use crate::election::{Contest, Election, ElectionType};
use crate::types::Hash32;

/// 選挙定義のハッシュのドメイン分離タグ。
pub const ELECTION_DEFINITION_DOMAIN: &[u8] = b"vote/election-definition/v1";

/// 選挙定義のハッシュ。
pub fn election_definition_hash(election: &Election) -> Hash32 {
    let mut out = Encoder(Sha256::new());
    out.0.update(ELECTION_DEFINITION_DOMAIN);
    out.str(election.id().as_str());
    out.str(election.name());

    // 借用（&ElectionType・&Contest）の Vec を並べ替える。値は複製しない。
    let mut types: Vec<&ElectionType> = election.types().iter().collect();
    types.sort_by(|a, b| a.code.cmp(&b.code));
    out.count(types.len());
    for t in types {
        out.str(t.code.as_str());
        out.str(&t.name);
        out.u32(t.order);
        out.str(t.method.as_str());
    }

    let mut contests: Vec<&Contest> = election.contests().iter().collect();
    contests.sort_by(|a, b| a.district.id.cmp(&b.district.id));
    out.count(contests.len());
    for c in &contests {
        let d = &c.district;
        out.str(d.id.as_str());
        out.str(d.election_type.as_str());
        out.str(&d.name);
        let mut prefectures: Vec<&str> = d.prefectures.iter().map(String::as_str).collect();
        prefectures.sort_unstable();
        out.count(prefectures.len());
        for p in prefectures {
            out.str(p);
        }
        out.u32(d.order);
    }

    // 候補者は、選挙区 ID 順（上の contests の順）に、選挙区の中では連番の数値順（`Election` が並べ済み）。
    out.count(contests.iter().map(|c| c.candidates.len()).sum());
    for c in &contests {
        for candidate in &c.candidates {
            out.str(candidate.id.as_str());
            out.str(&candidate.name);
            out.str(&candidate.party);
            out.str(&candidate.profile);
        }
    }
    out.0.finalize().into()
}

/// 固定幅・ビッグエンディアンで、ハッシュに直接書き込む（中間のバイト列を作らない）。
struct Encoder(Sha256);

impl Encoder {
    fn u32(&mut self, value: u32) {
        self.0.update(value.to_be_bytes());
    }

    /// 件数。選挙データの件数は u32 に収まる（収まらない件数は、読み込みの時点でメモリが足りない）ので、
    /// 万一超えたら上限に張り付かせる（値が変わるので、食い違いとして検出される）。
    fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }

    fn str(&mut self, s: &str) {
        self.count(s.len());
        self.0.update(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::election::{Candidate, District, VotingMethod};
    use crate::ids::{CandidateCode, DistrictId, ElectionId, ElectionTypeCode};

    fn etype(code: &str, name: &str, order: u32) -> ElectionType {
        ElectionType {
            code: ElectionTypeCode::new(code).expect("valid"),
            name: name.to_string(),
            order,
            method: VotingMethod::SingleChoice,
        }
    }

    fn district(id: &str, name: &str, prefectures: &[&str], order: u32) -> District {
        let id = DistrictId::new(id).expect("valid");
        District {
            election_type: ElectionTypeCode::new(id.type_segment()).expect("valid"),
            name: name.to_string(),
            prefectures: prefectures.iter().map(|p| (*p).to_string()).collect(),
            order,
            id,
        }
    }

    fn candidate(district: &str, seq: u64, name: &str, party: &str) -> Candidate {
        Candidate {
            id: CandidateCode::new(&DistrictId::new(district).expect("valid"), seq).expect("valid"),
            name: name.to_string(),
            party: party.to_string(),
            profile: "略歴".to_string(),
        }
    }

    fn types() -> Vec<ElectionType> {
        vec![
            etype("shugiin_smd", "衆議院小選挙区選挙", 10),
            etype("governor", "都道府県知事選挙", 50),
        ]
    }

    fn districts() -> Vec<District> {
        vec![
            district("shugiin_smd.13.01", "東京1区", &["13"], 1),
            district("governor.31_32", "鳥取・島根", &["31", "32"], 1),
        ]
    }

    fn candidates() -> Vec<Candidate> {
        vec![
            candidate("shugiin_smd.13.01", 1, "山田 太郎", "未来党"),
            candidate("shugiin_smd.13.01", 2, "鈴木 花子", "みらい共和"),
            candidate("governor.31_32", 1, "佐藤 健", "緑の会"),
        ]
    }

    fn build(
        types: Vec<ElectionType>,
        districts: Vec<District>,
        candidates: Vec<Candidate>,
    ) -> Election {
        Election::new(
            ElectionId::new("2026-general").expect("valid"),
            "サンプル選挙".to_string(),
            types,
            districts,
            candidates,
        )
        .expect("valid")
    }

    fn hex(hash: &Hash32) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_order_of_rows_does_not_change_the_hash() {
        let base = election_definition_hash(&build(types(), districts(), candidates()));
        let mut t = types();
        t.reverse();
        let mut d = districts();
        d.reverse();
        // 合区の都道府県の並びも問わない。
        d[0].prefectures.reverse();
        let mut c = candidates();
        c.reverse();
        assert_eq!(election_definition_hash(&build(t, d, c)), base);
    }

    #[test]
    fn changing_one_character_of_any_attribute_changes_the_hash() {
        let base = election_definition_hash(&build(types(), districts(), candidates()));
        let mut c = candidates();
        c[0].name = "山田 次郎".to_string();
        assert_ne!(
            election_definition_hash(&build(types(), districts(), c)),
            base
        );
        // 候補者の名前の入れ替え（ID は同じ）。
        let mut c = candidates();
        let (a, b) = (c[0].name.clone(), c[1].name.clone());
        c[0].name = b;
        c[1].name = a;
        assert_ne!(
            election_definition_hash(&build(types(), districts(), c)),
            base
        );
        let mut c = candidates();
        c[2].party = "無所属".to_string();
        assert_ne!(
            election_definition_hash(&build(types(), districts(), c)),
            base
        );
        let mut c = candidates();
        c[2].profile.push('。');
        assert_ne!(
            election_definition_hash(&build(types(), districts(), c)),
            base
        );
        let mut d = districts();
        d[0].name = "東京2区".to_string();
        assert_ne!(
            election_definition_hash(&build(types(), d, candidates())),
            base
        );
        let mut d = districts();
        d[1].order = 2;
        assert_ne!(
            election_definition_hash(&build(types(), d, candidates())),
            base
        );
        let mut t = types();
        t[1].order = 5;
        assert_ne!(
            election_definition_hash(&build(t, districts(), candidates())),
            base
        );
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        // 長さ接頭辞があるので、隣の欄との境目をずらしても同じバイト列にならない。
        let mut a = candidates();
        a[0].name = "山田".to_string();
        a[0].party = " 太郎未来党".to_string();
        let mut b = candidates();
        b[0].name = "山田 太郎".to_string();
        b[0].party = "未来党".to_string();
        assert_ne!(
            election_definition_hash(&build(types(), districts(), a)),
            election_definition_hash(&build(types(), districts(), b))
        );
    }

    #[test]
    fn known_vector() {
        // 独立実装（Python hashlib + struct.pack(">I", …)）で、上の形式どおりに計算した期待値。
        let expected = "efcb902f009c9e5f7b84ff92014fcb83c62cddc3835ba27b92a04386cc1f419d";
        let actual = hex(&election_definition_hash(&build(
            types(),
            districts(),
            candidates(),
        )));
        assert_eq!(actual, expected);
    }
}
