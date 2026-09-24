//! UNIX 時刻の UTC 表示（外部クレートを使わない小さな暦計算）。api・verifier・web で同じ表示にそろえる。

/// 1970-01-01 からの日数を、グレゴリオ暦の (年, 月, 日) にする（Howard Hinnant の civil_from_days）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// (年, 月, 日, 時, 分, 秒)
fn parts(unix_secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let (year, month, day) = civil_from_days(unix_secs.div_euclid(86_400));
    let secs = unix_secs.rem_euclid(86_400);
    (year, month, day, secs / 3_600, secs % 3_600 / 60, secs % 60)
}

/// ディレクトリ名用: `20260921T101500Z`。
pub fn utc_compact(unix_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = parts(unix_secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `2026-09-21T10:15:00Z`。
pub fn utc_iso(unix_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// 固定オフセットでの表示: `2026-10-25T16:00:00+09:00`（`election.display_timezone`。原則17・18）。
pub fn format_offset(unix_secs: i64, offset_secs: i64) -> String {
    let shifted = unix_secs.saturating_add(offset_secs);
    let (y, mo, d, h, mi, s) = parts(shifted);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let (oh, om) = (abs / 3_600, abs % 3_600 / 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{sign}{oh:02}:{om:02}")
}

/// 分単位の時刻（UNIX 分）の表示: `2026-09-21 10:15 UTC`。範囲外の値は、数値のまま `<n> 分` と表示する。
pub fn format_minute_utc(unix_minute: u64) -> String {
    match unix_minute
        .checked_mul(60)
        .and_then(|secs| i64::try_from(secs).ok())
    {
        Some(secs) => {
            let (y, mo, d, h, mi, _) = parts(secs);
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
        }
        None => format!("{unix_minute} 分"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_time_is_formatted_as_utc() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_compact(0), "19700101T000000Z");
        // 2026-09-21T10:15:30Z
        assert_eq!(utc_iso(1_789_985_730), "2026-09-21T10:15:30Z");
        assert_eq!(utc_compact(1_789_985_730), "20260921T101530Z");
        // うるう日・年末・負の値（1969）。
        assert_eq!(utc_iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_iso(1_798_761_599), "2026-12-31T23:59:59Z");
        assert_eq!(utc_iso(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn offsets_shift_the_displayed_time() {
        // 2026-09-21T10:15:30Z + 9h = 2026-09-21T19:15:30+09:00
        assert_eq!(
            format_offset(1_789_985_730, 9 * 3_600),
            "2026-09-21T19:15:30+09:00"
        );
        assert_eq!(format_offset(1_789_985_730, 0), "2026-09-21T10:15:30+00:00");
        // 負のオフセット。
        assert_eq!(format_offset(0, -9 * 3_600), "1969-12-31T15:00:00-09:00");
    }

    #[test]
    fn minutes_are_shown_without_seconds() {
        assert_eq!(format_minute_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(
            format_minute_utc(1_789_985_730 / 60),
            "2026-09-21 10:15 UTC"
        );
        assert_eq!(format_minute_utc(u64::MAX), format!("{} 分", u64::MAX));
    }
}
