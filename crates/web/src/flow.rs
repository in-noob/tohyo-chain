//! 画面遷移のロジック。UI（Leptos / ブラウザ API）から切り離した**純粋関数**だけを置く。
//!
//! 入力を受けて値を返すだけで、副作用も時計も持たない。そのため wasm-bindgen-test なしの
//! 通常の `cargo test` で検証できる。UI 側はこのモジュールの返す値に従って描画・遷移する。
//!
//! **投票する順番は固定**: 利用者は順番を選べない。API は、有権者に関係する投票用紙だけを表示順に返す
//! （`ballot-status`）ので、ここでは、その並びの**先頭の未投票のもの**を「今」の投票用紙とし、投票が済んだら
//! 表示順で次の未投票へ進む（前に戻る・最後まで行ったら最初に戻る、という動きはしない）。
//!
//! 秘密投票の観点で、ここが保持する候補者 ID は「投票を送信し終えるまで」の一時的な状態
//! （[`VotePhase`]）だけで、送信が終わると必ず捨てる。

use shared_types::BallotStatusDto;

use crate::error::ApiFailure;

/// 投票完了画面に表示する文言（既定）。**これだけ**を表示する。
///
/// 注意: この文言は「票がサーバに受理された」ことだけを示す。ブロックの封印（sealer が非同期に
/// 行う）や、改ざん検証の結果とは無関係であり、封印済み・検証済みを意味しない。
/// 候補者名・`ballot_id`・投票用紙の名前などは表示しない（秘密投票のため）。
pub const DONE_MESSAGE: &str = "投票を受け付けました";

/// 画面（ルート）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Login,
    /// 進捗の表示（済み／今／これから）。リンクはなく、読むだけの画面。
    Progress,
    /// 投票用紙 1 枚の投票画面。`contest_id`（`{election_id}/{district_id}`）だけを持つ（候補者は含めない）。
    Ballot(String),
    /// 投票完了画面。投票用紙も候補者も URL に含めない。
    Done,
}

impl Route {
    pub fn path(&self) -> String {
        match self {
            Self::Login => "/".to_string(),
            Self::Progress => "/progress".to_string(),
            // `contest_id` は `{election_id}/{district_id}`（`/` を含む）なので、そのままパスの 2 セグメントになる。
            Self::Ballot(contest_id) => format!("/ballots/{contest_id}"),
            Self::Done => "/done".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 投票用紙の並び（表示順・固定）
// ---------------------------------------------------------------------------

/// 未投票の投票用紙の数。
pub fn remaining(ballots: &[BallotStatusDto]) -> usize {
    ballots.iter().filter(|b| !b.voted).count()
}

pub fn all_voted(ballots: &[BallotStatusDto]) -> bool {
    remaining(ballots) == 0
}

/// 「今」の投票用紙の位置: 表示順の**先頭の未投票**。すべて投票済み（または 1 枚もない）なら `None`。
pub fn current_index(ballots: &[BallotStatusDto]) -> Option<usize> {
    ballots.iter().position(|b| !b.voted)
}

/// 「今」の投票用紙の `contest_id`。
pub fn current_contest(ballots: &[BallotStatusDto]) -> Option<&str> {
    current_index(ballots).map(|i| ballots[i].contest_id.as_str())
}

/// 進捗（`{total}枚中{current}枚目` の値）。`current` は、「今」の投票用紙が表示順で何枚目か（1 始まり）。
/// すべて投票済みなら `current == total`。投票用紙が 1 枚もなければ、どちらも 0。
pub fn progress(ballots: &[BallotStatusDto]) -> (usize, usize) {
    let total = ballots.len();
    let current = current_index(ballots).map_or(total, |i| i + 1);
    (current, total)
}

/// 進捗の表示文。`template` は設定 `labels.progress`（`{total}` と `{current}` を置き換える）。
pub fn format_progress(template: &str, current: usize, total: usize) -> String {
    template
        .replace("{total}", &total.to_string())
        .replace("{current}", &current.to_string())
}

/// 進捗の一覧での、投票用紙 1 枚の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BallotState {
    /// 投票済み。
    Done,
    /// 今（先頭の未投票）。
    Current,
    /// これから。
    Upcoming,
}

impl BallotState {
    /// 状態を、色だけに頼らず示す記号（文字の `label` と一緒に表示する）。
    pub fn mark(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Current => "▶",
            Self::Upcoming => "○",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Done => "済み",
            Self::Current => "今",
            Self::Upcoming => "これから",
        }
    }
}

/// 一覧の各投票用紙の状態（表示順）。投票済み = 済み、先頭の未投票 = 今、残りの未投票 = これから。
pub fn ballot_states(ballots: &[BallotStatusDto]) -> Vec<BallotState> {
    let current = current_index(ballots);
    ballots
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if b.voted {
                BallotState::Done
            } else if Some(i) == current {
                BallotState::Current
            } else {
                BallotState::Upcoming
            }
        })
        .collect()
}

/// 投票を受理した後の一覧。元の一覧は変えず、新しい `Vec` を返す（他の項目は不変）。
pub fn mark_voted(ballots: &[BallotStatusDto], contest_id: &str) -> Vec<BallotStatusDto> {
    ballots
        .iter()
        .map(|b| BallotStatusDto {
            voted: b.voted || b.contest_id == contest_id,
            ..b.clone()
        })
        .collect()
}

/// 投票完了画面のボタンの行き先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continue {
    /// 次の未投票の投票用紙へ（表示順で、先頭の未投票）。
    Ballot(String),
    /// すべて投票済み。終了（ログアウト）する。
    Finish,
}

/// 投票を受理した後、次にどこへ進むか（`ballots` は投票反映済みの一覧）。
/// 表示順で先頭の未投票へ進むだけで、前には戻らず、最後まで行っても最初には戻らない。
pub fn continue_after_vote(ballots: &[BallotStatusDto]) -> Continue {
    match current_contest(ballots) {
        Some(id) => Continue::Ballot(id.to_string()),
        None => Continue::Finish,
    }
}

/// 完了画面のボタンの表示文。`ballot_item` は設定 `labels.ballot_item`。
pub fn continue_label(next: &Continue, ballot_item: &str) -> String {
    match next {
        Continue::Ballot(_) => format!("次の{ballot_item}へ"),
        Continue::Finish => "終了".to_string(),
    }
}

/// ログイン直後・リロード後・完了画面から進む先: 先頭の未投票の投票用紙。なければ進捗の画面。
pub fn entry_route(ballots: &[BallotStatusDto]) -> Route {
    current_contest(ballots).map_or(Route::Progress, |id| Route::Ballot(id.to_string()))
}

// ---------------------------------------------------------------------------
// ルートの保護
// ---------------------------------------------------------------------------

/// ルートの保護判定に使う現在の状態。
#[derive(Debug, Clone, Copy)]
pub struct GuardContext<'a> {
    pub logged_in: bool,
    /// 取得済みの投票用紙の一覧（表示順。未取得なら `None`）。
    pub ballots: Option<&'a [BallotStatusDto]>,
    /// たった今投票を受理された投票用紙（完了画面の表示条件）。候補者は含まない。
    pub last_voted: Option<&'a str>,
}

/// `route` を表示してよいか。リダイレクトが必要なら行き先を返す。
///
/// - 未ログインではログイン画面以外に入れない。ログイン済みでログイン画面に来たら、先頭の未投票へ。
/// - 投票画面に入れるのは「今」の投票用紙だけ。投票済みのもの・まだ先のもの・存在しないものの URL を開いても、
///   先頭の未投票へ戻される（順番は選べない）。
/// - 完了画面は、投票直後でなければ表示しない（先頭の未投票へ）。
pub fn guard(ctx: &GuardContext<'_>, route: &Route) -> Option<Route> {
    let entry = || ctx.ballots.map_or(Route::Progress, entry_route);
    match route {
        Route::Login => ctx.logged_in.then(entry),
        _ if !ctx.logged_in => Some(Route::Login),
        Route::Progress => None,
        Route::Ballot(contest_id) => match ctx.ballots {
            // まだ一覧が無い間は判定できないので、そのまま表示する（取得後に描画される）。
            None => None,
            Some(ballots) => (current_contest(ballots) != Some(contest_id.as_str()))
                .then(|| entry_route(ballots)),
        },
        Route::Done => ctx.last_voted.is_none().then(entry),
    }
}

// ---------------------------------------------------------------------------
// 投票画面の状態遷移
// ---------------------------------------------------------------------------

/// 投票を試みて失敗したが、その場に留まる種類の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 通信エラー・サーバ障害。再試行できる。
    Unavailable,
    /// 選んだ候補者が受け付けられなかった。選び直す必要がある。
    InvalidCandidate,
}

impl FailureKind {
    pub fn can_retry(self) -> bool {
        matches!(self, Self::Unavailable)
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Unavailable => "サーバに接続できませんでした。もう一度お試しください。",
            Self::InvalidCandidate => {
                "選んだ候補者は受け付けられませんでした。選び直してください。"
            }
        }
    }
}

/// 投票画面の段階。候補者 ID は送信が終わるまでの一時的な保持で、終わると必ず捨てる。
///
/// 候補者 ID は文字列（`Copy` ではない）なので、段階を進める関数は `VotePhase` の**所有権を受け取って**
/// 新しい段階を返す（古い段階を使い回さない）。UI は `std::mem::take` で信号の中身を取り出して渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VotePhase {
    /// 候補者を選んでいる。
    Choosing { selected: Option<String> },
    /// 「この内容で投票しますか」の確認中。
    Confirming { candidate_id: String },
    /// 送信中（二重送信を防ぐため、この間は操作できない）。
    Submitting { candidate_id: String },
    /// 送信に失敗して留まっている。
    Failed {
        candidate_id: String,
        kind: FailureKind,
    },
}

impl Default for VotePhase {
    fn default() -> Self {
        Self::Choosing { selected: None }
    }
}

/// 候補者を選ぶ（選び直しも可）。選択中以外の段階では何もしない。
pub fn pick(phase: VotePhase, candidate_id: String) -> VotePhase {
    match phase {
        VotePhase::Choosing { .. } => VotePhase::Choosing {
            selected: Some(candidate_id),
        },
        other => other,
    }
}

/// 選択を確認画面へ進める。未選択なら何もしない。
pub fn confirm(phase: VotePhase) -> VotePhase {
    match phase {
        VotePhase::Choosing {
            selected: Some(candidate_id),
        } => VotePhase::Confirming { candidate_id },
        other => other,
    }
}

/// 確認・失敗表示から選択画面へ戻る（選択は保持）。送信中は戻れない。
pub fn cancel(phase: VotePhase) -> VotePhase {
    match phase {
        VotePhase::Confirming { candidate_id } | VotePhase::Failed { candidate_id, .. } => {
            VotePhase::Choosing {
                selected: Some(candidate_id),
            }
        }
        other => other,
    }
}

/// 再試行できる失敗から、確認画面へ戻る。
pub fn retry(phase: VotePhase) -> VotePhase {
    match phase {
        VotePhase::Failed { candidate_id, kind } if kind.can_retry() => {
            VotePhase::Confirming { candidate_id }
        }
        other => other,
    }
}

/// 確認済みの選択を送信段階へ進め、送信する候補者 ID を返す。確認中以外は `None`
/// （＝送信してはいけない。二重送信の防止）。段階は借用するだけで、`None` のときは何も変わらない。
pub fn submit(phase: &VotePhase) -> Option<(VotePhase, String)> {
    match phase {
        VotePhase::Confirming { candidate_id } => Some((
            VotePhase::Submitting {
                candidate_id: candidate_id.clone(),
            },
            candidate_id.clone(),
        )),
        _ => None,
    }
}

/// 投票 API の結果を UI が扱う種類にまとめたもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteOutcome {
    /// 201: 票が受理された。
    Accepted,
    /// 409: この投票用紙には既に投票済みだった（今回の票は受理されていない）。
    AlreadyVoted,
    /// 401: セッションが無効・期限切れ。
    SessionExpired,
    /// 404: 投票用紙が存在しない。
    BallotGone,
    /// 403: この有権者の投票対象ではない投票用紙。
    NotEligible,
    /// 422: 候補者が受け付けられなかった。
    InvalidCandidate,
    /// 通信エラー・5xx など。
    Unavailable,
}

pub fn vote_outcome(result: Result<(), ApiFailure>) -> VoteOutcome {
    match result {
        Ok(()) => VoteOutcome::Accepted,
        Err(ApiFailure::AlreadyVoted) => VoteOutcome::AlreadyVoted,
        Err(ApiFailure::Unauthorized) => VoteOutcome::SessionExpired,
        Err(ApiFailure::NotFound) => VoteOutcome::BallotGone,
        Err(ApiFailure::NotEligible) => VoteOutcome::NotEligible,
        Err(ApiFailure::InvalidCandidate) => VoteOutcome::InvalidCandidate,
        Err(ApiFailure::Unavailable | ApiFailure::Network | ApiFailure::Unexpected(_)) => {
            VoteOutcome::Unavailable
        }
    }
}

/// 送信結果を反映する。返す段階に加えて、画面を移すべきなら行き先を返す。
///
/// 画面を離れる場合は、段階を初期化して**候補者 ID を捨てる**。送信中でなければ何もしない。
/// 「投票済み・存在しない・対象外」で返す `Route::Progress` は「一覧が古い」の合図で、UI は、一覧を取り直してから
/// [`entry_route`] へ進む。
pub fn apply_outcome(phase: VotePhase, outcome: VoteOutcome) -> (VotePhase, Option<Route>) {
    let VotePhase::Submitting { candidate_id } = phase else {
        return (phase, None);
    };
    let reset = VotePhase::default();
    match outcome {
        VoteOutcome::Accepted => (reset, Some(Route::Done)),
        VoteOutcome::AlreadyVoted | VoteOutcome::BallotGone | VoteOutcome::NotEligible => {
            (reset, Some(Route::Progress))
        }
        VoteOutcome::SessionExpired => (reset, Some(Route::Login)),
        VoteOutcome::InvalidCandidate => (
            VotePhase::Failed {
                candidate_id,
                kind: FailureKind::InvalidCandidate,
            },
            None,
        ),
        VoteOutcome::Unavailable => (
            VotePhase::Failed {
                candidate_id,
                kind: FailureKind::Unavailable,
            },
            None,
        ),
    }
}

/// 画面を移した先で一度だけ見せる案内。`ballot_item` は設定 `labels.ballot_item`（投票用紙の呼び名）。
pub fn outcome_notice(outcome: VoteOutcome, ballot_item: &str) -> Option<String> {
    match outcome {
        VoteOutcome::AlreadyVoted => Some(format!("この{ballot_item}にはすでに投票済みです。")),
        VoteOutcome::SessionExpired => {
            Some("セッションの有効期限が切れました。もう一度ログインしてください。".to_string())
        }
        VoteOutcome::BallotGone => Some(format!("指定した{ballot_item}が見つかりませんでした。")),
        VoteOutcome::NotEligible => Some(format!(
            "この{ballot_item}は、あなたの投票対象ではありません。"
        )),
        VoteOutcome::Accepted | VoteOutcome::InvalidCandidate | VoteOutcome::Unavailable => None,
    }
}

// ---------------------------------------------------------------------------
// ログイン入力
// ---------------------------------------------------------------------------

pub const MAX_VOTER_ID_LEN: usize = 64;

/// 投票者 ID の入力の問題。サーバ側の規則（1〜64 文字、ASCII 英数字・`_`・`-`）と同じ。
/// 送信前のヒント用で、最終的な判定はサーバが行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoterIdProblem {
    Empty,
    TooLong,
    InvalidChar,
}

impl VoterIdProblem {
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "ログイン ID を入力してください。",
            Self::TooLong => "ログイン ID は 64 文字以内で入力してください。",
            Self::InvalidChar => {
                "ログイン ID には英数字・アンダースコア（_）・ハイフン（-）だけを使えます。"
            }
        }
    }
}

pub fn validate_voter_id(input: &str) -> Result<(), VoterIdProblem> {
    if input.is_empty() {
        return Err(VoterIdProblem::Empty);
    }
    if input.len() > MAX_VOTER_ID_LEN {
        return Err(VoterIdProblem::TooLong);
    }
    if !input
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(VoterIdProblem::InvalidChar);
    }
    Ok(())
}

/// ログイン失敗の表示文。
pub fn login_failure_message(failure: ApiFailure) -> &'static str {
    match failure {
        ApiFailure::Unauthorized => {
            "ログインできませんでした。ログイン ID とパスワードを確認してください。"
        }
        _ => "サーバに接続できませんでした。しばらくしてからもう一度お試しください。",
    }
}

#[cfg(test)]
mod tests {
    use shared_types::VotingMethod;

    use super::*;

    fn ballot(district: &str, voted: bool) -> BallotStatusDto {
        BallotStatusDto {
            contest_id: format!("2026-general/{district}"),
            name: format!("選挙区 {district}"),
            election_type: "shugiin_smd".to_string(),
            type_name: "種類".to_string(),
            method: VotingMethod::SingleChoice,
            voted,
        }
    }

    fn id(district: &str) -> String {
        format!("2026-general/{district}")
    }

    /// 表示順に (選挙区 ID, 投票済みか) で一覧を作る。
    fn list(spec: &[(&str, bool)]) -> Vec<BallotStatusDto> {
        spec.iter().map(|&(d, voted)| ballot(d, voted)).collect()
    }

    // --- 完了画面の文言 ---

    #[test]
    fn done_message_is_exactly_the_specified_text() {
        assert_eq!(DONE_MESSAGE, "投票を受け付けました");
    }

    // --- ルート ---

    #[test]
    fn route_paths_never_contain_candidates() {
        assert_eq!(Route::Login.path(), "/");
        assert_eq!(Route::Progress.path(), "/progress");
        assert_eq!(
            Route::Ballot(id("shugiin_smd.13.01")).path(),
            "/ballots/2026-general/shugiin_smd.13.01"
        );
        assert_eq!(Route::Done.path(), "/done");
    }

    // --- 固定順: 先頭の未投票が「今」 ---

    #[test]
    fn current_is_always_the_first_unvoted_in_display_order() {
        let b = list(&[("a.1", true), ("a.2", false), ("a.3", false)]);
        assert_eq!(current_index(&b), Some(1));
        assert_eq!(current_contest(&b), Some("2026-general/a.2"));
        // 先頭が未投票なら、それが「今」。
        assert_eq!(
            current_index(&list(&[("a.1", false), ("a.2", false)])),
            Some(0)
        );
    }

    #[test]
    fn current_is_none_when_everything_is_voted_or_there_are_no_ballots() {
        assert_eq!(current_index(&list(&[("a.1", true), ("a.2", true)])), None);
        assert_eq!(current_index(&[]), None);
        assert_eq!(current_contest(&[]), None);
    }

    #[test]
    fn a_ballot_voted_out_of_order_never_sends_the_user_back_or_around() {
        // 順番外で投票済みのもの（API で直接投票した等）があっても、「今」は先頭の未投票で、後ろは飛ばさない。
        let b = list(&[("a.1", false), ("a.2", true), ("a.3", false)]);
        assert_eq!(current_index(&b), Some(0));
        assert_eq!(
            ballot_states(&b),
            vec![
                BallotState::Current,
                BallotState::Done,
                BallotState::Upcoming
            ]
        );
    }

    #[test]
    fn remaining_and_all_voted() {
        let b = list(&[("a.1", true), ("a.2", false), ("a.3", false)]);
        assert_eq!(remaining(&b), 2);
        assert!(!all_voted(&b));
        assert!(all_voted(&list(&[("a.1", true)])));
        assert!(all_voted(&[]));
    }

    // --- 進捗 ---

    #[test]
    fn progress_counts_the_current_ballot_in_display_order() {
        let b = list(&[("a.1", true), ("a.2", true), ("a.3", false), ("a.4", false)]);
        assert_eq!(progress(&b), (3, 4));
        assert_eq!(progress(&list(&[("a.1", false), ("a.2", false)])), (1, 2));
        // すべて投票済み: current == total。投票用紙が無ければ (0, 0)。
        assert_eq!(progress(&list(&[("a.1", true), ("a.2", true)])), (2, 2));
        assert_eq!(progress(&[]), (0, 0));
    }

    #[test]
    fn progress_text_is_built_from_the_configured_template() {
        assert_eq!(
            format_progress("{total}枚中{current}枚目", 3, 9),
            "9枚中3枚目"
        );
        assert_eq!(format_progress("{current}/{total}", 2, 5), "2/5");
        assert_eq!(
            format_progress("全{total}枚のうち{current}枚目", 1, 1),
            "全1枚のうち1枚目"
        );
    }

    #[test]
    fn every_state_has_its_own_mark_besides_the_color() {
        let marks: Vec<&str> = [
            BallotState::Done,
            BallotState::Current,
            BallotState::Upcoming,
        ]
        .into_iter()
        .map(BallotState::mark)
        .collect();
        assert_eq!(marks, vec!["✓", "▶", "○"]);
    }

    #[test]
    fn ballot_states_mark_done_current_and_upcoming() {
        let b = list(&[
            ("a.1", true),
            ("a.2", true),
            ("a.3", false),
            ("a.4", false),
            ("a.5", false),
        ]);
        assert_eq!(
            ballot_states(&b),
            vec![
                BallotState::Done,
                BallotState::Done,
                BallotState::Current,
                BallotState::Upcoming,
                BallotState::Upcoming
            ]
        );
        let labels: Vec<&str> = ballot_states(&b)
            .into_iter()
            .map(BallotState::label)
            .collect();
        assert_eq!(labels, vec!["済み", "済み", "今", "これから", "これから"]);
        // すべて投票済みなら「今」は無い。
        assert!(
            ballot_states(&list(&[("a.1", true)]))
                .iter()
                .all(|s| *s == BallotState::Done)
        );
    }

    // --- mark_voted / continue ---

    #[test]
    fn mark_voted_updates_only_the_target_and_keeps_the_input_intact() {
        let before = list(&[("a.1", false), ("a.2", false), ("a.3", true)]);
        let after = mark_voted(&before, &id("a.2"));
        assert_eq!(after, list(&[("a.1", false), ("a.2", true), ("a.3", true)]));
        // 元の一覧は変わらない（新しい Vec を返す）。
        assert_eq!(
            before,
            list(&[("a.1", false), ("a.2", false), ("a.3", true)])
        );
        // 冪等。存在しない ID では何も変わらない。
        assert_eq!(mark_voted(&after, &id("a.2")), after);
        assert_eq!(mark_voted(&before, "nope"), before);
    }

    #[test]
    fn mark_voted_never_unvotes() {
        let b = list(&[("a.1", true)]);
        assert_eq!(mark_voted(&b, &id("zzz")), b);
    }

    #[test]
    fn continue_goes_to_the_next_unvoted_in_display_order_or_finishes() {
        let b = list(&[("a.1", true), ("a.2", false), ("a.3", false)]);
        assert_eq!(continue_after_vote(&b), Continue::Ballot(id("a.2")));
        let b = mark_voted(&b, &id("a.2"));
        assert_eq!(continue_after_vote(&b), Continue::Ballot(id("a.3")));
        let b = mark_voted(&b, &id("a.3"));
        assert_eq!(continue_after_vote(&b), Continue::Finish);
        assert_eq!(continue_after_vote(&[]), Continue::Finish);
    }

    #[test]
    fn a_full_session_visits_each_ballot_once_in_display_order_then_finishes() {
        // ログイン直後（すべて未投票）から、先頭の未投票へ順に進む。前には戻らず、最後まで行っても最初には戻らない。
        let order = ["smd.13.01", "pr.tokyo", "gov.13", "mc.13.101"];
        let mut b = list(&order.map(|d| (d, false)));
        let mut visited = Vec::new();
        let mut next = entry_route(&b);
        while let Route::Ballot(contest) = next {
            visited.push(contest.clone());
            b = mark_voted(&b, &contest);
            next = match continue_after_vote(&b) {
                Continue::Ballot(n) => Route::Ballot(n),
                Continue::Finish => Route::Progress,
            };
        }
        let expected: Vec<String> = order.iter().map(|d| id(d)).collect();
        assert_eq!(visited, expected);
        assert!(all_voted(&b));
        assert_eq!(next, Route::Progress);
    }

    #[test]
    fn reloading_resumes_from_the_first_unvoted_ballot() {
        // リロードしてログインし直すと、サーバの一覧（投票済みフラグ）から、先頭の未投票へ戻る。
        let b = list(&[("a.1", true), ("a.2", true), ("a.3", false), ("a.4", false)]);
        assert_eq!(entry_route(&b), Route::Ballot(id("a.3")));
        // すべて投票済み・投票用紙が無いときは、進捗の画面。
        assert_eq!(entry_route(&list(&[("a.1", true)])), Route::Progress);
        assert_eq!(entry_route(&[]), Route::Progress);
    }

    #[test]
    fn continue_labels_use_the_configured_ballot_item() {
        assert_eq!(
            continue_label(&Continue::Ballot(id("a.1")), "投票用紙"),
            "次の投票用紙へ"
        );
        assert_eq!(
            continue_label(&Continue::Ballot(id("a.1")), "票"),
            "次の票へ"
        );
        assert_eq!(continue_label(&Continue::Finish, "投票用紙"), "終了");
    }

    // --- guard ---

    fn ctx<'a>(
        logged_in: bool,
        ballots: Option<&'a [BallotStatusDto]>,
        last_voted: Option<&'a str>,
    ) -> GuardContext<'a> {
        GuardContext {
            logged_in,
            ballots,
            last_voted,
        }
    }

    #[test]
    fn logged_out_users_are_sent_to_login() {
        let b = list(&[("a.1", false)]);
        for route in [Route::Progress, Route::Ballot(id("a.1")), Route::Done] {
            assert_eq!(
                guard(&ctx(false, Some(&b), None), &route),
                Some(Route::Login)
            );
        }
        assert_eq!(guard(&ctx(false, None, None), &Route::Login), None);
    }

    #[test]
    fn logged_in_users_skip_the_login_page_and_go_to_the_current_ballot() {
        let b = list(&[("a.1", true), ("a.2", false)]);
        assert_eq!(
            guard(&ctx(true, Some(&b), None), &Route::Login),
            Some(Route::Ballot(id("a.2")))
        );
        // 一覧が未取得なら、進捗の画面へ。
        assert_eq!(
            guard(&ctx(true, None, None), &Route::Login),
            Some(Route::Progress)
        );
    }

    #[test]
    fn a_ballot_page_only_opens_for_the_current_ballot() {
        let b = list(&[("a.1", true), ("a.2", false), ("a.3", false)]);
        let g = ctx(true, Some(&b), None);
        assert_eq!(guard(&g, &Route::Ballot(id("a.2"))), None, "今の投票用紙");
        // 順番は選べない: 投票済み・まだ先・存在しないものの URL は、先頭の未投票へ戻される。
        let back = Some(Route::Ballot(id("a.2")));
        assert_eq!(guard(&g, &Route::Ballot(id("a.1"))), back);
        assert_eq!(guard(&g, &Route::Ballot(id("a.3"))), back);
        assert_eq!(
            guard(&g, &Route::Ballot("2026-general/nope".to_string())),
            back
        );
        // すべて投票済みなら、進捗の画面へ。
        let done = list(&[("a.1", true)]);
        assert_eq!(
            guard(&ctx(true, Some(&done), None), &Route::Ballot(id("a.1"))),
            Some(Route::Progress)
        );
        // 一覧が未取得の間は判定を保留して表示する。
        assert_eq!(
            guard(&ctx(true, None, None), &Route::Ballot(id("a.1"))),
            None
        );
    }

    #[test]
    fn done_page_only_right_after_a_vote() {
        let b = list(&[("a.1", true), ("a.2", false)]);
        assert_eq!(
            guard(&ctx(true, Some(&b), None), &Route::Done),
            Some(Route::Ballot(id("a.2")))
        );
        assert_eq!(
            guard(&ctx(true, Some(&b), Some("2026-general/a.1")), &Route::Done),
            None
        );
    }

    #[test]
    fn progress_page_is_open_to_logged_in_users() {
        assert_eq!(guard(&ctx(true, None, None), &Route::Progress), None);
    }

    // --- 投票画面の状態遷移 ---

    fn cand(n: u32) -> String {
        format!("shugiin_smd.13.01.c{n}")
    }

    #[test]
    fn happy_path_pick_confirm_submit_accept() {
        let phase = VotePhase::default();
        let phase = pick(phase, cand(2));
        assert_eq!(
            phase,
            VotePhase::Choosing {
                selected: Some(cand(2))
            }
        );
        let phase = confirm(phase);
        assert_eq!(
            phase,
            VotePhase::Confirming {
                candidate_id: cand(2)
            }
        );
        let (phase, candidate) = submit(&phase).expect("confirming can submit");
        assert_eq!(candidate, cand(2));
        assert_eq!(
            phase,
            VotePhase::Submitting {
                candidate_id: cand(2)
            }
        );
        let (phase, route) = apply_outcome(phase, VoteOutcome::Accepted);
        assert_eq!(route, Some(Route::Done));
        // 画面を離れるとき、選んだ候補者は保持されない。
        assert_eq!(phase, VotePhase::default());
    }

    #[test]
    fn can_change_selection_before_confirming() {
        let phase = pick(pick(VotePhase::default(), cand(1)), cand(3));
        assert_eq!(
            phase,
            VotePhase::Choosing {
                selected: Some(cand(3))
            }
        );
    }

    #[test]
    fn confirm_requires_a_selection() {
        assert_eq!(confirm(VotePhase::default()), VotePhase::default());
    }

    #[test]
    fn cancel_returns_to_choosing_and_keeps_the_selection() {
        let confirming = VotePhase::Confirming {
            candidate_id: cand(5),
        };
        assert_eq!(
            cancel(confirming),
            VotePhase::Choosing {
                selected: Some(cand(5))
            }
        );
        let failed = VotePhase::Failed {
            candidate_id: cand(5),
            kind: FailureKind::InvalidCandidate,
        };
        assert_eq!(
            cancel(failed),
            VotePhase::Choosing {
                selected: Some(cand(5))
            }
        );
    }

    #[test]
    fn cannot_cancel_or_pick_or_confirm_while_submitting() {
        let submitting = VotePhase::Submitting {
            candidate_id: cand(7),
        };
        assert_eq!(cancel(submitting.clone()), submitting);
        assert_eq!(pick(submitting.clone(), cand(8)), submitting);
        assert_eq!(confirm(submitting.clone()), submitting);
        assert_eq!(retry(submitting.clone()), submitting);
    }

    #[test]
    fn submit_is_only_possible_from_confirming() {
        assert_eq!(submit(&VotePhase::default()), None);
        assert_eq!(
            submit(&VotePhase::Choosing {
                selected: Some(cand(1))
            }),
            None
        );
        // 送信中に再度 submit しても二重送信にならない。
        assert_eq!(
            submit(&VotePhase::Submitting {
                candidate_id: cand(1)
            }),
            None
        );
        assert_eq!(
            submit(&VotePhase::Failed {
                candidate_id: cand(1),
                kind: FailureKind::Unavailable
            }),
            None
        );
    }

    #[test]
    fn outcomes_route_the_user_and_reset_the_phase() {
        let submitting = VotePhase::Submitting {
            candidate_id: cand(9),
        };
        for (outcome, route) in [
            (VoteOutcome::Accepted, Route::Done),
            (VoteOutcome::AlreadyVoted, Route::Progress),
            (VoteOutcome::BallotGone, Route::Progress),
            (VoteOutcome::NotEligible, Route::Progress),
            (VoteOutcome::SessionExpired, Route::Login),
        ] {
            let (phase, next) = apply_outcome(submitting.clone(), outcome);
            assert_eq!(next, Some(route), "{outcome:?}");
            assert_eq!(phase, VotePhase::default(), "{outcome:?}");
        }
    }

    #[test]
    fn recoverable_failures_stay_on_the_page() {
        let submitting = VotePhase::Submitting {
            candidate_id: cand(9),
        };
        let (phase, next) = apply_outcome(submitting.clone(), VoteOutcome::Unavailable);
        assert_eq!(next, None);
        assert_eq!(
            phase,
            VotePhase::Failed {
                candidate_id: cand(9),
                kind: FailureKind::Unavailable
            }
        );
        // 再試行できる失敗は確認画面へ戻れる。
        assert_eq!(
            retry(phase),
            VotePhase::Confirming {
                candidate_id: cand(9)
            }
        );

        let (phase, next) = apply_outcome(submitting, VoteOutcome::InvalidCandidate);
        assert_eq!(next, None);
        // 候補者が受け付けられない失敗は再試行できず、選び直すしかない。
        assert_eq!(retry(phase.clone()), phase);
        assert_eq!(
            cancel(phase),
            VotePhase::Choosing {
                selected: Some(cand(9))
            }
        );
    }

    #[test]
    fn outcome_is_ignored_unless_submitting() {
        for phase in [
            VotePhase::default(),
            VotePhase::Confirming {
                candidate_id: cand(1),
            },
            VotePhase::Choosing {
                selected: Some(cand(1)),
            },
        ] {
            assert_eq!(
                apply_outcome(phase.clone(), VoteOutcome::Accepted),
                (phase, None)
            );
        }
    }

    #[test]
    fn api_results_map_to_outcomes() {
        assert_eq!(vote_outcome(Ok(())), VoteOutcome::Accepted);
        assert_eq!(
            vote_outcome(Err(ApiFailure::AlreadyVoted)),
            VoteOutcome::AlreadyVoted
        );
        assert_eq!(
            vote_outcome(Err(ApiFailure::Unauthorized)),
            VoteOutcome::SessionExpired
        );
        assert_eq!(
            vote_outcome(Err(ApiFailure::NotFound)),
            VoteOutcome::BallotGone
        );
        assert_eq!(
            vote_outcome(Err(ApiFailure::NotEligible)),
            VoteOutcome::NotEligible
        );
        assert_eq!(
            vote_outcome(Err(ApiFailure::InvalidCandidate)),
            VoteOutcome::InvalidCandidate
        );
        for failure in [
            ApiFailure::Unavailable,
            ApiFailure::Network,
            ApiFailure::Unexpected(418),
        ] {
            assert_eq!(vote_outcome(Err(failure)), VoteOutcome::Unavailable);
        }
    }

    #[test]
    fn notices_use_the_configured_ballot_item_and_exist_only_for_unhappy_exits() {
        let item = "投票用紙";
        assert_eq!(
            outcome_notice(VoteOutcome::AlreadyVoted, item).as_deref(),
            Some("この投票用紙にはすでに投票済みです。")
        );
        assert_eq!(
            outcome_notice(VoteOutcome::BallotGone, "票").as_deref(),
            Some("指定した票が見つかりませんでした。")
        );
        assert!(outcome_notice(VoteOutcome::NotEligible, item).is_some());
        assert!(outcome_notice(VoteOutcome::SessionExpired, item).is_some());
        assert_eq!(outcome_notice(VoteOutcome::Accepted, item), None);
        assert_eq!(outcome_notice(VoteOutcome::Unavailable, item), None);
    }

    // --- ログイン入力 ---

    #[test]
    fn voter_id_validation_matches_the_server_rules() {
        for ok in ["a", "voter-01", "Voter_2", &"x".repeat(64)] {
            assert_eq!(validate_voter_id(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_voter_id(""), Err(VoterIdProblem::Empty));
        assert_eq!(
            validate_voter_id(&"x".repeat(65)),
            Err(VoterIdProblem::TooLong)
        );
        for bad in ["a.b", "a b", "日本語", "a/b", " a"] {
            assert_eq!(
                validate_voter_id(bad),
                Err(VoterIdProblem::InvalidChar),
                "{bad}"
            );
        }
    }

    #[test]
    fn login_failure_messages() {
        assert!(login_failure_message(ApiFailure::Unauthorized).contains("ログイン ID"));
        assert!(login_failure_message(ApiFailure::Network).contains("接続"));
    }
}
