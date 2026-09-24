//! API と画面（web）で共有する DTO。ワークスペース内の他クレートには依存しない。
//!
//! 秘密投票の原則により、投票者と投票内容を同時に含む型は作らない。

pub mod hex;
pub mod time;

use std::fmt;

use serde::{Deserialize, Serialize};

/// `POST /api/v1/login` のリクエスト。
#[derive(Clone, Deserialize, Serialize)]
pub struct LoginRequest {
    /// ログイン ID。旧名 `voter_id` も受け付ける（stub 認証では、入力した ID がそのまま投票者 ID になる）。
    #[serde(alias = "voter_id")]
    pub login_id: String,
    /// パスワード。`auth.mode=db` のときだけ使う（stub 認証は無視する）。
    #[serde(default)]
    pub password: Option<String>,
    /// マイナンバー欄。画面と API の項目として存在するだけで、値は保存もログ出力もしない（受け取って破棄する）。
    #[serde(default)]
    pub my_number: Option<String>,
}

// パスワードとマイナンバーが誤ってログに出ないよう、Debug では伏せる。
impl fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginRequest")
            .field("login_id", &self.login_id)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("my_number", &self.my_number.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub expires_in_secs: u64,
}

/// `GET /api/v1/election-status`（認証不要。ログイン画面・進捗画面が、期間と今の状態を表示するのに使う。
/// 原則17・18）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ElectionStatusResponse {
    /// `scheduled` | `open` | `closing` | `closed`。
    pub phase: String,
    /// 開始時刻（UNIX 秒）。未指定なら `None`。
    pub opens_at: Option<i64>,
    /// 終了時刻（UNIX 秒）。未指定なら `None`。
    pub closes_at: Option<i64>,
    /// api の現在時刻（UNIX 秒）。
    pub now: i64,
    /// 表示用タイムゾーン名（`election.display_timezone`。例 `Asia/Tokyo`）。
    pub display_timezone: String,
    /// 表示用タイムゾーンの UTC からのオフセット秒。
    pub display_timezone_offset_secs: i64,
}

/// 投票方式。今回実装しているのは、候補者を 1 人選ぶ `single_choice` だけ（将来の拡張に備えて `enum`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VotingMethod {
    SingleChoice,
}

/// 投票用紙 1 枚の状況。`contest_id` は `{election_id}/{district_id}`（例: `2026-general/shugiin_smd.13.01`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BallotStatusDto {
    pub contest_id: String,
    /// 選挙区の名前。
    pub name: String,
    /// 選挙の種類のコード（例: `shugiin_smd`）。
    pub election_type: String,
    /// 選挙の種類の表示名。
    pub type_name: String,
    pub method: VotingMethod,
    /// ログイン中の有権者が、この投票用紙に投票済みか。
    pub voted: bool,
}

/// `GET /api/v1/ballot-status` のレスポンス。有権者に関係する投票用紙だけが、**表示順**（選挙の種類の順、
/// 次に選挙区の順）で入る。投票する順番は、この並びで固定（利用者は選べない）。名簿に無い有権者には空。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BallotStatusResponse {
    pub ballots: Vec<BallotStatusDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CandidateDto {
    /// `{district_id}.c{連番}`（例: `shugiin_smd.13.01.c3`）。
    pub candidate_id: String,
    pub name: String,
    pub party: String,
}

/// `GET /api/v1/contests/{election_id}/{district_id}/candidates` のレスポンス。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CandidatesResponse {
    pub candidates: Vec<CandidateDto>,
}

/// `POST /api/v1/contests/{election_id}/{district_id}/vote` のリクエスト。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VoteRequest {
    pub candidate_id: String,
}

/// 投票の受理応答。ballot_id などのレシートは意図的に含めない（買収・強要の証拠になるため）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VoteResponse {
    pub status: String,
}

/// 封印済みブロックのヘッダ。ハッシュ類は小文字 hex。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeaderDto {
    pub version: u16,
    pub height: u64,
    pub prev_hash: String,
    pub merkle_root: String,
    pub ballot_count: u32,
    /// 封印時刻（UNIX 分）。
    pub sealed_at_minute: u64,
}

/// 封印済みの 1 票。投票者を特定する情報は含まない。
///
/// 表示名（`district_name`・`candidate_name`・`party`）は、ビューア向けに API が選挙データから付ける（検証には使わない。
/// ブロックのハッシュに含まれるのは ID だけ）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BallotDto {
    pub ballot_id: String,
    pub contest_id: String,
    pub candidate_id: String,
    /// 選挙区の表示名。選挙データに無ければ省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub district_name: Option<String>,
    /// 候補者の表示名。選挙データに無ければ省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
}

fn default_true() -> bool {
    true
}

/// `GET /api/v1/chains/{shard}/blocks/{height}` のレスポンス。
///
/// `chain.reveal_ballots=after_close` の締切前は、票の中身（`ballot_id`・`contest_id`・`candidate_id`）を返さず、
/// `ballots` は空で `ballots_revealed` が偽になる（ヘッダー・票数・ハッシュ・署名だけを返す）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlockDto {
    pub header: HeaderDto,
    /// `ballot_id` のハッシュ昇順。票が非公開のときは空。
    #[serde(default)]
    pub ballots: Vec<BallotDto>,
    pub block_hash: String,
    pub signature: String,
    /// 票の中身を公開しているか。偽のときは `ballots` が空でも、ブロックが空という意味ではない（票数は `header.ballot_count`）。
    #[serde(default = "default_true")]
    pub ballots_revealed: bool,
    /// ブロック署名の検証に使う Ed25519 公開鍵（hex）。API が知らなければ省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer_public_key: Option<String>,
}

/// ブロックの要約（一覧用。票は含まない）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlockSummaryDto {
    pub header: HeaderDto,
    pub block_hash: String,
    pub signature: String,
}

/// シャード 1 本の概要。チェーンがまだ無い（sealer が初期化していない）シャードは `head` が無い。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShardSummaryDto {
    pub shard: u16,
    #[serde(default)]
    pub head: Option<BlockSummaryDto>,
}

/// `GET /api/v1/chains` のレスポンス: シャードの一覧と、それぞれの先頭ブロック。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainsResponse {
    pub shards: Vec<ShardSummaryDto>,
    /// ブロック署名の検証に使う Ed25519 公開鍵（hex）。未登録なら省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer_public_key: Option<String>,
}

/// `GET /api/v1/chains/{shard}/blocks` のレスポンス: 新しい順のブロックの要約。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlocksPageResponse {
    pub shard: u16,
    /// 高さの降順。
    pub blocks: Vec<BlockSummaryDto>,
    /// 次のページ（より古いブロック）を取るときの `before_height`。これ以上古いブロックが無ければ省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_height: Option<u64>,
}

/// `GET /api/v1/anchors` のレスポンス: 新しい順のアンカー。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnchorsResponse {
    pub anchors: Vec<AnchorDto>,
}

/// `GET /api/v1/chains/{shard}/head` のレスポンス。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeadDto {
    pub shard: u16,
    pub height: u64,
    pub block_hash: String,
    pub ballot_count: u32,
    pub sealed_at_minute: u64,
    /// ブロック署名の検証に使う Ed25519 公開鍵（hex）。
    pub signer_public_key: String,
}

/// アンカーに含まれる、あるシャードの head。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeadRefDto {
    pub shard: u16,
    pub height: u64,
    pub block_hash: String,
}

/// `GET /api/v1/anchors/latest` のレスポンス。全シャードの head をまとめて署名したスナップショット。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnchorDto {
    pub seq: u64,
    /// 作成時刻（UNIX 分）。
    pub anchor_minute: u64,
    pub prev_anchor_hash: String,
    /// shard の昇順。
    pub heads: Vec<HeadRefDto>,
    pub anchor_hash: String,
    pub signature: String,
}

/// 投票用紙ごとの集計（監査用）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContestCountsDto {
    pub contest_id: String,
    /// 投票済みの記録（participation）の件数。
    pub participation: u64,
    /// まだ封印されていない票の件数。
    pub pending: u64,
}

/// `GET /api/v1/audit/counts` のレスポンス。投票者や票の中身を含まない集計値のみ。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuditCountsResponse {
    pub contests: Vec<ContestCountsDto>,
}

/// エラー応答。`error` は機械可読なコード、`message` は利用者向けの文言（呼び名は設定の `labels` から作る）。
/// voter_id や candidate_id は含めない。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_debug_redacts_password_and_my_number() {
        let req = LoginRequest {
            login_id: "alice".to_string(),
            password: Some("secret-pass".to_string()),
            my_number: Some("123456789012".to_string()),
        };
        let shown = format!("{req:?}");
        assert!(
            !shown.contains("123456789012") && !shown.contains("secret-pass"),
            "{shown}"
        );
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn login_request_accepts_the_old_voter_id_name() {
        let req: LoginRequest =
            serde_json::from_str(r#"{"voter_id":"alice","password":"p"}"#).expect("parse");
        assert_eq!(
            (req.login_id.as_str(), req.password.as_deref()),
            ("alice", Some("p"))
        );
        let req: LoginRequest = serde_json::from_str(r#"{"login_id":"ABC"}"#).expect("parse");
        assert_eq!(req.login_id, "ABC");
    }

    #[test]
    fn voting_method_serializes_as_snake_case() {
        let json = serde_json::to_string(&VotingMethod::SingleChoice).expect("serialize");
        assert_eq!(json, "\"single_choice\"");
    }

    #[test]
    fn error_response_message_is_optional_when_parsing() {
        let e: ErrorResponse = serde_json::from_str(r#"{"error":"not_found"}"#).expect("parse");
        assert_eq!((e.error.as_str(), e.message.as_str()), ("not_found", ""));
    }

    #[test]
    fn login_request_accepts_missing_my_number() {
        let req: LoginRequest = serde_json::from_str(r#"{"login_id":"a"}"#).expect("parse");
        assert_eq!((req.my_number, req.password), (None, None));
    }
}
