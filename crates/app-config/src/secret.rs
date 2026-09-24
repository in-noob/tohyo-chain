//! 秘密情報を包む型。`Debug` / `Display` では値を出さない（ログやエラーメッセージへの漏洩を防ぐ）。

use std::fmt;

/// 秘密情報。値を取り出すには [`Secret::expose`] を明示的に呼ぶ。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// 値への参照。呼び出し側は、ログ・エラーメッセージに出さないこと。
    pub fn expose(&self) -> &T {
        &self.0
    }
}

/// 伏せ字（`show` や `Debug` で使う）。
pub const MASK: &str = "***";

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_hide_the_value() {
        let s = Secret::new("very-secret".to_string());
        assert_eq!(format!("{s:?}"), MASK);
        assert_eq!(format!("{s}"), MASK);
        assert_eq!(s.expose(), "very-secret");
    }
}
