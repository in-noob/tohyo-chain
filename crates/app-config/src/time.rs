//! RFC 3339 の日時（例: `2026-10-01T09:00:00+09:00`）の解釈。投票期間の検証（開始 < 締切）に使う。

/// UNIX 秒に変換する。形式が不正なら理由を返す。
pub fn parse_rfc3339(text: &str) -> Result<i64, String> {
    let bytes = text.as_bytes();
    let err = || "RFC 3339 の日時が必要です（例: 2026-10-01T09:00:00+09:00）".to_string();
    // YYYY-MM-DDTHH:MM:SS の 19 文字。
    if bytes.len() < 20 || !text.is_ascii() {
        return Err(err());
    }
    let digits = |range: std::ops::Range<usize>| -> Result<i64, String> {
        let part = &text[range];
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().map_err(|_| err())
        } else {
            Err(err())
        }
    };
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err(err());
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    let mut rest = &text[19..];
    // 小数秒は読み飛ばす。
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if frac_len == 0 {
            return Err(err());
        }
        rest = &after_dot[frac_len..];
    }
    let offset_secs = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return Err(err()),
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return Err(err());
            }
            let offset_hour: i64 = rest[1..3].parse().map_err(|_| err())?;
            let offset_minute: i64 = rest[4..6].parse().map_err(|_| err())?;
            if offset_hour > 23 || offset_minute > 59 {
                return Err(err());
            }
            sign * (offset_hour * 3600 + offset_minute * 60)
        }
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err("存在しない日付です".to_string());
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err("存在しない時刻です".to_string());
    }
    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// 1970-01-01 からの日数（グレゴリオ暦。Howard Hinnant の days_from_civil）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_instants() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Ok(951_868_800));
        // +09:00 は UTC より 9 時間進んでいる。
        assert_eq!(
            parse_rfc3339("2026-10-01T09:00:00+09:00"),
            parse_rfc3339("2026-10-01T00:00:00Z")
        );
        assert_eq!(
            parse_rfc3339("2026-10-01T00:00:00.123Z"),
            parse_rfc3339("2026-10-01T00:00:00Z")
        );
    }

    #[test]
    fn orders_instants_across_offsets() {
        let a = parse_rfc3339("2026-10-01T09:00:00+09:00").expect("valid");
        let b = parse_rfc3339("2026-10-01T00:00:01Z").expect("valid");
        assert!(a < b);
    }

    #[test]
    fn rejects_malformed_and_nonexistent_values() {
        for bad in [
            "",
            "2026-10-01",
            "2026-10-01 09:00:00Z",
            "2026-10-01T09:00:00",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-01T24:00:00Z",
            "2026-10-01T00:00:00+0900",
            "2026-10-01T00:00:00.Z",
            "明日T00:00:00Z",
        ] {
            assert!(parse_rfc3339(bad).is_err(), "{bad}");
        }
        assert!(parse_rfc3339("2028-02-29T00:00:00Z").is_ok());
        assert!(parse_rfc3339("2027-02-29T00:00:00Z").is_err());
    }
}
