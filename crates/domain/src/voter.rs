//! 投票者 ID。票（`Ballot`）には決して含めない。

pub const MAX_VOTER_ID_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VoterIdError {
    #[error("voter_id が空です")]
    Empty,
    #[error("voter_id が長すぎます（最大 {MAX_VOTER_ID_LEN} 文字）")]
    TooLong,
    #[error("voter_id に使えない文字が含まれています（英数字・_・- のみ）")]
    InvalidChar,
}

/// 検証済みの投票者 ID（1〜64 文字、ASCII 英数字・`_`・`-`）。
/// 区切り文字 `.` を含まないので、セッショントークンにそのまま埋め込める。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VoterId(String);

impl VoterId {
    pub fn new(raw: &str) -> Result<Self, VoterIdError> {
        if raw.is_empty() {
            return Err(VoterIdError::Empty);
        }
        if raw.len() > MAX_VOTER_ID_LEN {
            return Err(VoterIdError::TooLong);
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(VoterIdError::InvalidChar);
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_ids() {
        for s in ["a", "voter-01", "Voter_2", &"x".repeat(64)] {
            assert_eq!(VoterId::new(s).expect("valid").as_str(), s);
        }
    }

    #[test]
    fn rejects_invalid_ids() {
        assert_eq!(VoterId::new(""), Err(VoterIdError::Empty));
        assert_eq!(VoterId::new(&"x".repeat(65)), Err(VoterIdError::TooLong));
        for s in ["a.b", "a b", "日本語", "a/b", "a\n"] {
            assert_eq!(VoterId::new(s), Err(VoterIdError::InvalidChar), "{s:?}");
        }
    }
}
