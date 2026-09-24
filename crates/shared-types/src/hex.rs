//! 小文字 hex の変換（ハッシュ・署名・公開鍵を JSON で運ぶため）。依存なしの最小実装。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexError {
    /// 文字数が `2 * N` ではない。
    InvalidLength,
    /// 16 進数字以外の文字が含まれている。
    InvalidChar,
}

impl std::fmt::Display for HexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hex の長さが不正です"),
            Self::InvalidChar => f.write_str("hex に不正な文字が含まれています"),
        }
    }
}

impl std::error::Error for HexError {}

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

fn nibble(c: u8) -> Result<u8, HexError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(HexError::InvalidChar),
    }
}

/// ちょうど `N` バイトになる hex 文字列を固定長配列にする。
pub fn decode_array<const N: usize>(s: &str) -> Result<[u8; N], HexError> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return Err(HexError::InvalidLength);
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut out = [0u8; N];
    for (slot, [hi, lo]) in out.iter_mut().zip(pairs) {
        *slot = (nibble(*hi)? << 4) | nibble(*lo)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = [0x00, 0x01, 0x7f, 0x80, 0xab, 0xff];
        let s = encode(&data);
        assert_eq!(s, "00017f80abff");
        assert_eq!(decode_array::<6>(&s), Ok(data));
        assert_eq!(decode_array::<6>("00017F80ABFF"), Ok(data));
    }

    #[test]
    fn rejects_bad_length_and_chars() {
        assert_eq!(decode_array::<2>("abc"), Err(HexError::InvalidLength));
        assert_eq!(decode_array::<2>("abcdef"), Err(HexError::InvalidLength));
        assert_eq!(decode_array::<2>("abcg"), Err(HexError::InvalidChar));
        assert_eq!(decode_array::<2>("ab cd"), Err(HexError::InvalidLength));
        assert_eq!(decode_array::<1>("日"), Err(HexError::InvalidLength));
    }
}
