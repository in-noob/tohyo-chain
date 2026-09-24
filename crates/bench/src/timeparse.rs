//! ログの時刻（tracing の RFC 3339 / UTC）を UNIX ミリ秒にする。

/// 1970-01-01 からの日数（グレゴリオ暦。Howard Hinnant の days_from_civil）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-09-20T05:59:23.770465Z`（小数秒は省略可）→ UNIX ミリ秒。UTC（末尾 `Z`）のみ対応。
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (year, month, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, m, sec) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // 小数秒はミリ秒までを使う（足りなければ 0 埋め）。
    let millis: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    Some(((days_from_civil(year, month, day) * 24 + h) * 60 + m) * 60_000 + sec * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1_500));
        // 2027-01-15T08:00:00Z = 1_800_000_000 秒（Python: calendar.timegm で確認した値）
        assert_eq!(
            parse_rfc3339_ms("2027-01-15T08:00:00Z"),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-09-20T05:59:23.770465Z"),
            Some(1_789_883_963_770)
        );
    }

    #[test]
    fn leap_years_and_month_ends() {
        // 2024 は閏年、2100 は閏年ではない。
        let d = |s| parse_rfc3339_ms(s).expect("valid");
        assert_eq!(
            d("2024-03-01T00:00:00Z") - d("2024-02-28T00:00:00Z"),
            2 * 86_400_000
        );
        assert_eq!(
            d("2100-03-01T00:00:00Z") - d("2100-02-28T00:00:00Z"),
            86_400_000
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "2026-09-20",
            "2026-09-20T05:59:23",
            "2026-13-20T00:00:00Z",
            "2026-09-20T25:00:00Z",
            "2026-09-20T00:00:00.x1Z",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn fraction_is_truncated_to_millis() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.123999Z"), Some(123));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.1Z"), Some(100));
    }
}
