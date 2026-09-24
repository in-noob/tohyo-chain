//! 選挙・選挙区・投票用紙・候補者の ID（CLAUDE.md の原則 13: 変更されない文字列コード）。
//!
//! | ID | 形式 | 例 |
//! |---|---|---|
//! | `election_id` | 選挙の単位 | `2026-general` |
//! | `election_type` | 選挙の種類のコード | `shugiin_smd` |
//! | `district_id` | 選挙区。先頭のセグメントが選挙の種類 | `shugiin_smd.13.01`（都道府県は JIS X 0401 の 2 桁）|
//! | `contest_id` | `{election_id}/{district_id}`（投票用紙 1 枚 = 選挙 × 選挙区）| `2026-general/shugiin_smd.13.01` |
//! | `candidate_id` | `{district_id}.c{連番}`（連番の桁数は固定しない）。票では、白票の予約値 `blank` も使う | `shugiin_smd.13.01.c3` |
//!
//! 文字種は小文字の ASCII 英数字・`_`・`-`（選挙区と候補者はセグメントの区切りに `.`、`contest_id` は
//! `election_id` と `district_id` の区切りに `/`）。**選挙区の再編などで将来変わり得る意味は ID に埋め込まない**
//! （合区のように 1 つの選挙区が複数の都道府県にまたがる場合も、ID は変えず、対象の都道府県は選挙区の属性で持つ）。
//! 最大長も決めておき、生成時（読み込み時）にすべて検証する。票の正規化形式は、これらを長さ接頭辞つきで
//! 埋め込む（`encoding` を参照）。

use std::fmt;

pub const ELECTION_ID_MAX_LEN: usize = 32;
pub const ELECTION_TYPE_MAX_LEN: usize = 32;
pub const DISTRICT_ID_MAX_LEN: usize = 64;
/// `contest_id` = `election_id` + `/` + `district_id`。
pub const CONTEST_ID_MAX_LEN: usize = ELECTION_ID_MAX_LEN + 1 + DISTRICT_ID_MAX_LEN;
/// 候補者の連番の最大桁数。
pub const CANDIDATE_SEQ_MAX_DIGITS: usize = 14;
/// `candidate_id` = `district_id` + `.c` + 連番。
pub const CANDIDATE_ID_MAX_LEN: usize = DISTRICT_ID_MAX_LEN + 2 + CANDIDATE_SEQ_MAX_DIGITS;

/// ID の検証エラー。`kind` は ID の種類の名前（`election_id` など）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} が空です")]
    Empty { kind: &'static str },
    #[error("{kind} が長すぎます（最大 {max} 文字）: {len} 文字")]
    TooLong {
        kind: &'static str,
        max: usize,
        len: usize,
    },
    #[error("{kind} に使えない文字 {ch:?} が含まれています（{allowed}）")]
    InvalidChar {
        kind: &'static str,
        ch: char,
        allowed: &'static str,
    },
    #[error("{kind} の形式が不正です: {reason}")]
    Malformed {
        kind: &'static str,
        reason: &'static str,
    },
    #[error("{kind} に {value:?} は使えません（{reason}）")]
    Reserved {
        kind: &'static str,
        value: &'static str,
        reason: &'static str,
    },
}

fn check_len(kind: &'static str, raw: &str, max: usize) -> Result<(), IdError> {
    if raw.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if raw.len() > max {
        return Err(IdError::TooLong {
            kind,
            max,
            len: raw.len(),
        });
    }
    Ok(())
}

fn check_chars(
    kind: &'static str,
    raw: &str,
    allowed: &'static str,
    ok: impl Fn(char) -> bool,
) -> Result<(), IdError> {
    match raw.chars().find(|&c| !ok(c)) {
        Some(ch) => Err(IdError::InvalidChar { kind, ch, allowed }),
        None => Ok(()),
    }
}

fn is_lower_alnum(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

macro_rules! id_common {
    ($name:ident) => {
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

/// 選挙の単位（例: `2026-general`）。小文字英数字・`_`・`-`、先頭は英数字、最大 32 文字。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ElectionId(String);

impl ElectionId {
    pub const KIND: &'static str = "election_id";

    pub fn new(raw: &str) -> Result<Self, IdError> {
        check_len(Self::KIND, raw, ELECTION_ID_MAX_LEN)?;
        check_chars(
            Self::KIND,
            raw,
            "小文字の英数字・_・- のみ",
            |c| is_lower_alnum(c) || c == '_' || c == '-',
        )?;
        if !raw.starts_with(is_lower_alnum) {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "先頭は英数字",
            });
        }
        Ok(Self(raw.to_string()))
    }
}
id_common!(ElectionId);

/// 選挙の種類のコード（例: `shugiin_smd`）。小文字英数字・`_`、先頭は英字、最大 32 文字。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ElectionTypeCode(String);

impl ElectionTypeCode {
    pub const KIND: &'static str = "election_type";

    pub fn new(raw: &str) -> Result<Self, IdError> {
        check_len(Self::KIND, raw, ELECTION_TYPE_MAX_LEN)?;
        check_chars(Self::KIND, raw, "小文字の英数字・_ のみ", |c| {
            is_lower_alnum(c) || c == '_'
        })?;
        if !raw.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "先頭は英字",
            });
        }
        Ok(Self(raw.to_string()))
    }
}
id_common!(ElectionTypeCode);

/// 選挙区のコード（例: `shugiin_smd.13.01`）。
///
/// 小文字英数字・`_`・`-` のセグメントを `.` で区切る（空のセグメントは不可）。最大 64 文字。
/// 先頭のセグメントは、その選挙区が属する選挙の種類のコード。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DistrictId(String);

impl DistrictId {
    pub const KIND: &'static str = "district_id";

    pub fn new(raw: &str) -> Result<Self, IdError> {
        check_len(Self::KIND, raw, DISTRICT_ID_MAX_LEN)?;
        check_chars(
            Self::KIND,
            raw,
            "小文字の英数字・_・-・. のみ",
            |c| is_lower_alnum(c) || matches!(c, '_' | '-' | '.'),
        )?;
        if raw.split('.').any(str::is_empty) {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "空のセグメントがあります（先頭・末尾・連続した . は不可）",
            });
        }
        if raw.split('.').next().is_some_and(|first| {
            !first.starts_with(|c: char| c.is_ascii_lowercase())
                || !first.chars().all(|c| is_lower_alnum(c) || c == '_')
        }) {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "先頭のセグメントは選挙の種類のコード（英字で始まる英数字・_）",
            });
        }
        Ok(Self(raw.to_string()))
    }

    /// 先頭のセグメント（選挙の種類のコード）。
    pub fn type_segment(&self) -> &str {
        self.0.split('.').next().unwrap_or_default()
    }
}
id_common!(DistrictId);

/// 投票用紙 1 枚を指す ID: `{election_id}/{district_id}`。最大 97 文字。
///
/// 1 つの選挙区の投票は 1 つの選挙の 1 種類につき 1 枚なので、選挙区が決まれば投票用紙が決まる。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContestId(String);

impl ContestId {
    pub const KIND: &'static str = "contest_id";

    pub fn new(election: &ElectionId, district: &DistrictId) -> Self {
        Self(format!("{election}/{district}"))
    }

    pub fn parse(raw: &str) -> Result<Self, IdError> {
        check_len(Self::KIND, raw, CONTEST_ID_MAX_LEN)?;
        let Some((election, district)) = raw.split_once('/') else {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "{election_id}/{district_id} の形式が必要です",
            });
        };
        ElectionId::new(election)?;
        DistrictId::new(district)?;
        Ok(Self(raw.to_string()))
    }

    /// `/` の前（選挙の ID）。
    pub fn election_part(&self) -> &str {
        self.0.split_once('/').map_or("", |(election, _)| election)
    }

    /// `/` の後（選挙区の ID）。
    pub fn district_part(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, district)| district)
    }
}
id_common!(ContestId);

/// 白票を表す `candidate_id` の予約値。候補者の ID（[`CandidateCode`]）には使えない。
pub const BLANK_CANDIDATE_ID: &str = "blank";

/// 候補者の ID（候補者コード）: `{district_id}.c{連番}`（連番は 1 以上の 10 進数で、先頭に 0 を付けない。
/// 桁数は固定しない）。最大 80 文字。白票の予約値 [`BLANK_CANDIDATE_ID`] は、この形式に合わないうえ、
/// 読み込み時にも明示的に拒否する（選挙データの候補者を白票と取り違えないため）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateCode(String);

impl CandidateCode {
    pub const KIND: &'static str = "candidate_id";

    /// 選挙区と連番から作る。
    pub fn new(district: &DistrictId, seq: u64) -> Result<Self, IdError> {
        Self::parse(&format!("{district}.c{seq}"))
    }

    pub fn parse(raw: &str) -> Result<Self, IdError> {
        if raw == BLANK_CANDIDATE_ID {
            return Err(IdError::Reserved {
                kind: Self::KIND,
                value: BLANK_CANDIDATE_ID,
                reason: "白票の予約値",
            });
        }
        check_len(Self::KIND, raw, CANDIDATE_ID_MAX_LEN)?;
        let Some((district, seq)) = raw.rsplit_once(".c") else {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "{district_id}.c{連番} の形式が必要です",
            });
        };
        DistrictId::new(district)?;
        if seq.is_empty()
            || seq.len() > CANDIDATE_SEQ_MAX_DIGITS
            || !seq.bytes().all(|b| b.is_ascii_digit())
            || seq.starts_with('0')
        {
            return Err(IdError::Malformed {
                kind: Self::KIND,
                reason: "連番は、先頭に 0 を付けない 1 以上の 10 進数（最大 14 桁）",
            });
        }
        Ok(Self(raw.to_string()))
    }

    /// 連番（`.c` の後）。生成時に検証済みなので、常に 1 以上の整数。
    pub fn sequence(&self) -> u64 {
        self.0
            .rsplit_once(".c")
            .and_then(|(_, seq)| seq.parse().ok())
            .unwrap_or(0)
    }

    /// この候補者の選挙区（`.c{連番}` の前）。
    pub fn district_part(&self) -> &str {
        self.0
            .rsplit_once(".c")
            .map_or("", |(district, _)| district)
    }
}
id_common!(CandidateCode);

/// 票の投票先（票の `candidate_id`）: 候補者か、白票（どの候補者にも投票しない）。
///
/// 文字列表現は、候補者なら候補者コード、白票なら予約値 [`BLANK_CANDIDATE_ID`]（`"blank"`）。
/// ブロックの正規化形式（原則4・ADR 0013）はこの文字列を埋め込むので、白票を加えても形式の版は変わらない。
/// `enum` にしてあるのは、集計・表示のたびに `match` で白票の扱いを必ず決めさせるため（文字列の比較だと漏れる）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidateId {
    /// 白票。
    Blank,
    /// 候補者。
    Candidate(CandidateCode),
}

impl CandidateId {
    pub const KIND: &'static str = CandidateCode::KIND;

    /// 選挙区と連番から、候補者への投票先を作る。
    pub fn new(district: &DistrictId, seq: u64) -> Result<Self, IdError> {
        CandidateCode::new(district, seq).map(Self::Candidate)
    }

    /// `"blank"` なら白票、それ以外は候補者コードとして検証する。
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        if raw == BLANK_CANDIDATE_ID {
            Ok(Self::Blank)
        } else {
            CandidateCode::parse(raw).map(Self::Candidate)
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Blank => BLANK_CANDIDATE_ID,
            Self::Candidate(code) => code.as_str(),
        }
    }

    pub fn is_blank(&self) -> bool {
        matches!(self, Self::Blank)
    }

    /// 候補者への票なら、その候補者コード。白票なら `None`。
    pub fn candidate(&self) -> Option<&CandidateCode> {
        match self {
            Self::Blank => None,
            Self::Candidate(code) => Some(code),
        }
    }
}

impl From<CandidateCode> for CandidateId {
    fn from(code: CandidateCode) -> Self {
        Self::Candidate(code)
    }
}

impl fmt::Display for CandidateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for CandidateId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// 都道府県名（JIS X 0401 の順。添字 0 が `01`）。
pub const PREFECTURE_NAMES: [&str; 47] = [
    "北海道",
    "青森県",
    "岩手県",
    "宮城県",
    "秋田県",
    "山形県",
    "福島県",
    "茨城県",
    "栃木県",
    "群馬県",
    "埼玉県",
    "千葉県",
    "東京都",
    "神奈川県",
    "新潟県",
    "富山県",
    "石川県",
    "福井県",
    "山梨県",
    "長野県",
    "岐阜県",
    "静岡県",
    "愛知県",
    "三重県",
    "滋賀県",
    "京都府",
    "大阪府",
    "兵庫県",
    "奈良県",
    "和歌山県",
    "鳥取県",
    "島根県",
    "岡山県",
    "広島県",
    "山口県",
    "徳島県",
    "香川県",
    "愛媛県",
    "高知県",
    "福岡県",
    "佐賀県",
    "長崎県",
    "熊本県",
    "大分県",
    "宮崎県",
    "鹿児島県",
    "沖縄県",
];

/// 都道府県コード（`01`〜`47`）の名前。コードが不正なら `None`。
pub fn prefecture_name(code: &str) -> Option<&'static str> {
    if !is_prefecture_code(code) {
        return None;
    }
    let n: usize = code.parse().ok()?;
    PREFECTURE_NAMES.get(n - 1).copied()
}

/// 都道府県コード（JIS X 0401 の 2 桁: `01`〜`47`）。
pub fn is_prefecture_code(raw: &str) -> bool {
    matches!(raw.as_bytes(), [a, b] if a.is_ascii_digit() && b.is_ascii_digit())
        && raw.parse::<u8>().is_ok_and(|n| (1..=47).contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids_round_trip() {
        let election = ElectionId::new("2026-general").expect("valid");
        let district = DistrictId::new("shugiin_smd.13.01").expect("valid");
        let contest = ContestId::new(&election, &district);
        assert_eq!(contest.as_str(), "2026-general/shugiin_smd.13.01");
        assert_eq!(ContestId::parse(contest.as_str()), Ok(contest.clone()));
        assert_eq!(contest.election_part(), "2026-general");
        assert_eq!(contest.district_part(), "shugiin_smd.13.01");
        let candidate = CandidateCode::new(&district, 3).expect("valid");
        assert_eq!(candidate.as_str(), "shugiin_smd.13.01.c3");
        assert_eq!(candidate.district_part(), "shugiin_smd.13.01");
        assert_eq!(
            CandidateId::parse(candidate.as_str()),
            Ok(CandidateId::Candidate(candidate))
        );
        assert_eq!(district.type_segment(), "shugiin_smd");
        assert!(ElectionTypeCode::new("supreme_court_review").is_ok());
        // 合区: 都道府県は ID に埋め込まない（属性で持つ）。ID は英数字・_・- のセグメント。
        assert!(DistrictId::new("sangiin_district.tottori_shimane").is_ok());
    }

    #[test]
    fn candidate_sequence_has_no_fixed_width() {
        let district = DistrictId::new("shugiin_pr.tokyo").expect("valid");
        for seq in [1u64, 9, 10, 123, 99_999, 12_345_678_901] {
            let id = CandidateCode::new(&district, seq).expect("valid");
            assert_eq!(id.as_str(), format!("shugiin_pr.tokyo.c{seq}"));
            assert_eq!(id.sequence(), seq);
        }
        assert!(
            CandidateId::parse("shugiin_pr.tokyo.c0").is_err(),
            "0 は不可"
        );
        assert!(
            CandidateId::parse("shugiin_pr.tokyo.c01").is_err(),
            "先頭の 0 は不可"
        );
        assert!(CandidateId::parse("shugiin_pr.tokyo.c").is_err());
        assert!(CandidateId::parse("shugiin_pr.tokyo.cx1").is_err());
        assert!(
            CandidateId::parse("shugiin_pr.tokyo.c123456789012345").is_err(),
            "15 桁"
        );
    }

    #[test]
    fn rejects_bad_characters_shapes_and_lengths() {
        for bad in ["", "Bad", "a b", "a/b", "-x", "é", "a.b"] {
            assert!(ElectionId::new(bad).is_err(), "{bad:?}");
        }
        assert!(ElectionId::new(&"a".repeat(32)).is_ok());
        assert!(ElectionId::new(&"a".repeat(33)).is_err());
        for bad in ["", "1abc", "Abc", "a-b", "a.b"] {
            assert!(ElectionTypeCode::new(bad).is_err(), "{bad:?}");
        }
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "A.1",
            "shugiin smd.13",
            "1.13",
            "shu-giin.13",
            "shugiin/smd",
        ] {
            assert!(DistrictId::new(bad).is_err(), "{bad:?}");
        }
        assert!(DistrictId::new(&format!("a.{}", "b".repeat(62))).is_ok());
        assert!(DistrictId::new(&format!("a.{}", "b".repeat(63))).is_err());
        for bad in ["", "2026-general", "/d.1", "e/", "e/D", "E/d.1", "a/b/c.1"] {
            assert!(ContestId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(ContestId::parse(&format!("{}/a.{}", "e".repeat(32), "b".repeat(62))).is_ok());
    }

    #[test]
    fn maximum_lengths_are_consistent() {
        assert_eq!(CONTEST_ID_MAX_LEN, 97);
        assert_eq!(CANDIDATE_ID_MAX_LEN, 80);
        let district = DistrictId::new(&format!("a.{}", "b".repeat(62))).expect("64 chars");
        assert_eq!(district.as_str().len(), DISTRICT_ID_MAX_LEN);
        let candidate = CandidateCode::new(&district, 12_345_678_901_234).expect("14 digits");
        assert_eq!(candidate.as_str().len(), CANDIDATE_ID_MAX_LEN);
    }

    #[test]
    fn blank_is_a_reserved_value_of_the_vote_but_never_a_candidate_code() {
        // 票の投票先としては、予約値 "blank" が白票になる（文字列表現は往復する）。
        let blank = CandidateId::parse("blank").expect("blank");
        assert_eq!(blank, CandidateId::Blank);
        assert!(blank.is_blank());
        assert_eq!(blank.as_str(), BLANK_CANDIDATE_ID);
        assert_eq!(blank.to_string(), "blank");
        assert_eq!(blank.candidate(), None);
        // 候補者コードには使えない（選挙データの候補者を白票と取り違えない）。
        assert_eq!(
            CandidateCode::parse("blank"),
            Err(IdError::Reserved {
                kind: "candidate_id",
                value: "blank",
                reason: "白票の予約値",
            })
        );
        // 大文字や前後の空白は予約値ではなく、ただの不正な ID。
        for bad in ["Blank", "BLANK", " blank", "blank ", "blank.c1x", ""] {
            assert!(CandidateId::parse(bad).is_err(), "{bad:?}");
        }
        let candidate = CandidateId::parse("shugiin_smd.13.01.c2").expect("valid");
        assert!(!candidate.is_blank());
        assert_eq!(candidate.candidate().map(CandidateCode::sequence), Some(2));
    }

    #[test]
    fn prefecture_names_follow_the_jis_codes() {
        assert_eq!(prefecture_name("01"), Some("北海道"));
        assert_eq!(prefecture_name("13"), Some("東京都"));
        assert_eq!(prefecture_name("27"), Some("大阪府"));
        assert_eq!(prefecture_name("47"), Some("沖縄県"));
        for bad in ["", "0", "00", "48", "1", "ab"] {
            assert_eq!(prefecture_name(bad), None, "{bad:?}");
        }
        assert_eq!(PREFECTURE_NAMES.len(), 47);
    }

    #[test]
    fn prefecture_codes_are_two_digit_jis() {
        for ok in ["01", "13", "27", "47"] {
            assert!(is_prefecture_code(ok), "{ok}");
        }
        for bad in ["", "0", "1", "00", "48", "99", "013", "1a", "٠١"] {
            assert!(!is_prefecture_code(bad), "{bad:?}");
        }
    }
}
