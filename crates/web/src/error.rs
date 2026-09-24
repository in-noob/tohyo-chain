//! API 呼び出しの失敗の種類（純粋なデータと写像。ブラウザ API には依存しない）。

/// UI が区別すべき API 失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFailure {
    /// 401: 資格情報またはセッションが無効。
    Unauthorized,
    /// 403: この有権者の投票対象ではない投票用紙。
    NotEligible,
    /// 404: 投票用紙などが存在しない。
    NotFound,
    /// 409: その投票用紙には投票済み。
    AlreadyVoted,
    /// 409 `revote_conflict`: 同時に送られた別のやり直しが先に受理された。
    RevoteConflict,
    /// 409 `revote_limit_reached`: やり直しの上限に達している。
    RevoteLimitReached,
    /// 409 `revote_not_allowed` / `not_voted`: やり直せない。
    RevoteUnavailable,
    /// 422: 選んだ候補者がその投票用紙にいない。
    InvalidCandidate,
    /// 5xx: サーバ側の一時的な障害。
    Unavailable,
    /// 通信そのものに失敗した（オフライン、サーバ停止など）。
    Network,
    /// 想定外のステータス。
    Unexpected(u16),
}

/// エラーの文言の先頭に「⚠」を付ける。エラーを、文字の色（赤系）だけで表さないため。
pub fn alert_text(message: &str) -> String {
    format!("⚠ {message}")
}

/// HTTP ステータスと、エラー応答の `error`（機械可読なコード）を失敗種別に写す。409 は、コードで区別する
/// （投票済み・やり直しの競合・上限・やり直せない）。成功（2xx）は `None`。
pub fn classify_error(status: u16, code: Option<&str>) -> Option<ApiFailure> {
    match (status, code) {
        (409, Some("revote_conflict")) => Some(ApiFailure::RevoteConflict),
        (409, Some("revote_limit_reached")) => Some(ApiFailure::RevoteLimitReached),
        (409, Some("revote_not_allowed" | "not_voted")) => Some(ApiFailure::RevoteUnavailable),
        _ => classify_status(status),
    }
}

/// HTTP ステータスを失敗種別に写す。成功（2xx）は `None`。
pub fn classify_status(status: u16) -> Option<ApiFailure> {
    match status {
        200..=299 => None,
        401 => Some(ApiFailure::Unauthorized),
        403 => Some(ApiFailure::NotEligible),
        404 => Some(ApiFailure::NotFound),
        409 => Some(ApiFailure::AlreadyVoted),
        422 => Some(ApiFailure::InvalidCandidate),
        500..=599 => Some(ApiFailure::Unavailable),
        other => Some(ApiFailure::Unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_start_with_a_warning_mark_so_they_do_not_rely_on_color() {
        assert_eq!(alert_text("通信に失敗しました。"), "⚠ 通信に失敗しました。");
    }

    #[test]
    fn success_statuses_are_not_failures() {
        for status in [200, 201, 204, 299] {
            assert_eq!(classify_status(status), None, "{status}");
        }
    }

    #[test]
    fn known_failures_are_distinguished() {
        assert_eq!(classify_status(401), Some(ApiFailure::Unauthorized));
        assert_eq!(classify_status(403), Some(ApiFailure::NotEligible));
        assert_eq!(classify_status(404), Some(ApiFailure::NotFound));
        assert_eq!(classify_status(409), Some(ApiFailure::AlreadyVoted));
        assert_eq!(classify_status(422), Some(ApiFailure::InvalidCandidate));
        for status in [500, 503, 599] {
            assert_eq!(classify_status(status), Some(ApiFailure::Unavailable));
        }
    }

    #[test]
    fn conflicts_are_told_apart_by_the_error_code() {
        assert_eq!(
            classify_error(409, Some("revote_conflict")),
            Some(ApiFailure::RevoteConflict)
        );
        assert_eq!(
            classify_error(409, Some("revote_limit_reached")),
            Some(ApiFailure::RevoteLimitReached)
        );
        for code in ["revote_not_allowed", "not_voted"] {
            assert_eq!(
                classify_error(409, Some(code)),
                Some(ApiFailure::RevoteUnavailable)
            );
        }
        assert_eq!(
            classify_error(409, Some("already_voted")),
            Some(ApiFailure::AlreadyVoted)
        );
        assert_eq!(classify_error(409, None), Some(ApiFailure::AlreadyVoted));
        assert_eq!(classify_error(201, None), None);
    }

    #[test]
    fn everything_else_is_unexpected() {
        for status in [100, 301, 400, 408, 429, 600] {
            assert_eq!(
                classify_status(status),
                Some(ApiFailure::Unexpected(status))
            );
        }
    }
}
