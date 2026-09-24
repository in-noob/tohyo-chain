//! ポート（外部とのインターフェース）。実装は infra-* クレートや api が提供する。

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use domain::{
    Anchor, Ballot, BallotId, Block, ContestId, DistrictId, Election, ElectionPhase, Period,
    ShardId, VoterId,
};

/// ログイン時に受け取る資格情報。
pub struct Credentials {
    /// ログイン ID（stub 認証では、入力した ID がそのまま投票者 ID になる）。
    pub voter_id: String,
    /// パスワード（`auth.mode=db` のときだけ使う。stub は無視する）。
    pub password: Option<String>,
    /// マイナンバー欄。画面と API の項目として存在するだけで、値は保存も出力もしない（受け取って破棄する）。
    pub my_number: Option<String>,
}

// パスワードとマイナンバーが誤ってログに出ないよう、Debug では伏せる。
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("voter_id", &self.voter_id)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("my_number", &self.my_number.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("資格情報が不正です")]
    InvalidCredentials,
    #[error("認証基盤が利用できません")]
    Unavailable,
}

/// 認証（スコープ外）。実装を差し替えられるようにトレイトにしている。
#[async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, credentials: Credentials) -> Result<VoterId, AuthError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("ストアが利用できません")]
    Unavailable,
    #[error("不正なシャードが指定されました")]
    InvalidShard,
    /// 高さや `prev_hash` が連続しない、消費件数がプールを超える等、書き込みの前提が崩れた。
    #[error("ストアの状態と書き込みが矛盾しています")]
    Conflict,
    /// 保存されたデータを解釈できない（型・長さの不整合など）。
    #[error("ストアのデータが不正です")]
    Corrupt,
}

/// 選挙マスタの取得。読み取り専用のキャッシュ（原則6の例外）。
#[async_trait]
pub trait ElectionRepository: Send + Sync {
    async fn election(&self) -> Result<Arc<Election>, StoreError>;
}

/// 有権者ごとの、属する選挙区のリスト（有権者名簿）。読み取り専用のキャッシュ（原則6の例外）。
///
/// 名簿に無い有権者は `None`（ログインは通るが、投票できる投票用紙はない）。
#[async_trait]
pub trait VoterRoll: Send + Sync {
    async fn districts_of(&self, voter: &VoterId) -> Result<Option<Vec<DistrictId>>, StoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CastError {
    #[error("投票済みです")]
    AlreadyVoted,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// 投票用紙ごとの集計（監査用。投票者や票の中身は含まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestCounts {
    pub contest: ContestId,
    /// 投票済みの記録（participation）の件数。
    pub participation: u64,
    /// まだ封印されていない（プールにある）票の件数。
    pub pending: u64,
}

/// 投票の保存先。
///
/// 実装は次の 2 つを**別々のデータ**として持たなければならない（原則1）。
/// - participation: 「(投票用紙, 投票者) が投票済み」という事実だけ。ballot_id・候補者・時刻・順序を持たない。
/// - ballot_pool: シャードごとの未封印の票。投票者を特定する情報を持たない。
///
/// 両者を結ぶキーを作ってはならない。
#[async_trait]
pub trait VoteStore: Send + Sync {
    /// 二重投票の判定・participation への記録・ballot_pool への追加を**不可分に**行う。
    /// 既に投票済みなら何も保存せず `AlreadyVoted` を返す。
    ///
    /// 投票先の投票用紙は `ballot.contest_id`。
    async fn cast(&self, voter: &VoterId, shard: ShardId, ballot: Ballot) -> Result<(), CastError>;

    /// 投票者が投票済みの投票用紙。
    async fn voted_contests(&self, voter: &VoterId) -> Result<Vec<ContestId>, StoreError>;

    /// シャードごとの未封印の票の件数（添字がシャード番号）。件数のみで、票の中身は返さない。
    async fn pending_by_shard(&self) -> Result<Vec<usize>, StoreError>;

    /// 投票用紙ごとの participation 件数と未封印の票の件数（監査用。全体を走査するので頻繁には呼ばない）。
    /// participation と票を突き合わせるのではなく、それぞれを投票用紙別に**数えるだけ**で、
    /// 投票者と票を結び付けない。`contest_id` の昇順。
    async fn audit_counts(&self) -> Result<Vec<ContestCounts>, StoreError>;
}

/// 封印済みチェーンの読み取り（公開データ。API と verifier 向け）。
#[async_trait]
pub trait ChainRead: Send + Sync {
    /// シャードの最新ブロック。チェーンが未初期化、またはシャードが存在しなければ `None`。
    async fn head(&self, shard: ShardId) -> Result<Option<Block>, StoreError>;

    /// 指定した高さのブロック。存在しなければ `None`。
    async fn block(&self, shard: ShardId, height: u64) -> Result<Option<Block>, StoreError>;

    /// 最新のアンカー。まだ無ければ `None`。
    async fn latest_anchor(&self) -> Result<Option<Anchor>, StoreError>;

    /// ブロック署名の検証に使う公開鍵（sealer が登録したもの）。未登録なら `None`。
    async fn signer_public_key(&self) -> Result<Option<[u8; 32]>, StoreError>;

    /// 高さの新しい順に、`before_height` より低いブロックを最大 `limit` 件（`None` なら先頭から）。
    /// チェーンが無ければ空。ビューアのページ送り用（既定の実装は `head` と `block` の組み合わせ。ストアは、
    /// 範囲読み取りで置き換えてよい）。
    async fn blocks_before(
        &self,
        shard: ShardId,
        before_height: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Block>, StoreError> {
        let Some(head) = self.head(shard).await? else {
            return Ok(Vec::new());
        };
        let mut next = before_height
            .unwrap_or(u64::MAX)
            .min(head.header.height.saturating_add(1));
        let mut blocks = Vec::new();
        while next > 0 && blocks.len() < limit {
            next -= 1;
            match self.block(shard, next).await? {
                Some(block) => blocks.push(block),
                None => break,
            }
        }
        Ok(blocks)
    }

    /// 新しい順に、最大 `limit` 件のアンカー。既定の実装は、最新の 1 件だけ。
    async fn latest_anchors(&self, limit: usize) -> Result<Vec<Anchor>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self.latest_anchor().await?.into_iter().collect())
    }
}

/// sealer が使うストア。シャードごとに**単一の sealer だけ**が書き込む前提。
#[async_trait]
pub trait SealStore: ChainRead {
    /// シャードの未封印の票の件数。
    async fn pending_len(&self, shard: ShardId) -> Result<usize, StoreError>;

    /// 未封印の票の先頭（到着順）から最大 `n` 件を、削除せずに返す。
    async fn peek_pending(&self, shard: ShardId, n: usize) -> Result<Vec<Ballot>, StoreError>;

    /// ブロックをチェーンに追加し、プールの先頭 `consumed` 件を**不可分に**削除する。
    ///
    /// チェーンが空なら高さ 0（ジェネシス）、そうでなければ `head.height + 1` かつ
    /// `prev_hash == head.block_hash` でなければ `Conflict`。`consumed` がプールの件数を
    /// 超えても `Conflict`。失敗時は何も変更しない。
    async fn commit(&self, shard: ShardId, block: Block, consumed: usize)
    -> Result<(), StoreError>;

    /// 署名の公開鍵を登録する。既に**別の鍵**が登録されていれば `Conflict`（1 本のチェーンに
    /// 別の鍵の署名が混ざるのを防ぐ）。同じ鍵の再登録は成功する。
    async fn register_signer(&self, public_key: [u8; 32]) -> Result<(), StoreError>;

    /// アンカーを追加する。同じ `seq` が既にあれば追加せず `false`（競り負け）を返す。
    async fn append_anchor(&self, anchor: &Anchor) -> Result<bool, StoreError>;

    /// 起動時の復旧。`commit` の途中（ブロック追加後、プール削除前）で落ちた場合に、
    /// 先頭ブロックの票がプールに残っていれば取り除く（冪等）。
    /// ブロック追加とプール削除を不可分にできるストア（メモリ）では何もしない。
    async fn recover(&self, _shard: ShardId) -> Result<(), StoreError> {
        Ok(())
    }
}

/// TTL 付きのリース（分散ロック）。同じ `name` のリースを、同時に 1 つの `owner` だけが持てる。
///
/// 実装は、取得・更新・解放をそれぞれ 1 回の条件付き書き込み（LWT）で行う。期限（TTL）が切れたリースは
/// 誰でも取得できる。時計の異なるプロセス間で使うため、保持者は「更新に成功した時刻 + TTL」より
/// 十分前に（余裕を持って）保持をやめること。
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// 誰も持っていなければ取得して `true`。他の owner が持っていれば `false`。
    async fn try_acquire(&self, name: &str, owner: &str, ttl: Duration)
    -> Result<bool, StoreError>;

    /// 自分が持っていれば期限を延ばして `true`。失っていれば（別の owner・期限切れ）`false`。
    async fn renew(&self, name: &str, owner: &str, ttl: Duration) -> Result<bool, StoreError>;

    /// 自分が持っていれば解放する（他の owner のものは何もしない）。
    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError>;
}

/// システム時計。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> u64 {
        // 時計が UNIX エポックより前を指す異常時は 0 として扱う。
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// 現在時刻（UNIX 秒）。純粋なロジックのテストで時刻を固定できるようにする。
pub trait Clock: Send + Sync {
    fn now_unix_secs(&self) -> u64;
}

/// `ballot_id`（UUIDv4）の供給元。
pub trait BallotIdSource: Send + Sync {
    fn next_ballot_id(&self) -> BallotId;
}

/// 選挙状態の変更ログ（原則17: 変更のたびに、日時・変更前・変更後・実行した主体を記録する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionAuditEntry {
    pub at_unix_secs: i64,
    pub from: ElectionPhase,
    pub to: ElectionPhase,
    /// 実行した主体（例: `sealer:<sealer.id>`、`admin:<トークンの下 8 桁>`）。
    pub actor: String,
}

/// 選挙状態のスナップショット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionStateSnapshot {
    pub phase: ElectionPhase,
    pub period: Period,
    /// closing に遷移した時刻（締切の手続きの待ち時間の起点）。closing 以外では意味を持たない。
    pub closing_started_at: Option<i64>,
}

/// 選挙状態（scheduled → open → closing → closed）の保存先（原則17）。
///
/// DB（memory モードではプロセス内）を正とする、単一行の状態。実装は `transition` / `schedule` を
/// 条件付き書き込み（LWT 等）で行い、複数プロセスが同時に呼んでも 1 つだけが成功するようにする。
#[async_trait]
pub trait ElectionStateStore: Send + Sync {
    /// 初回だけ、`period` で `Scheduled` の行を作る（既にあれば何もしない）。
    /// どちらの場合も、実際に保存されている値を返す（呼び出し側が、設定値との食い違いを検出できるように）。
    async fn ensure_initialized(&self, period: Period)
    -> Result<ElectionStateSnapshot, StoreError>;

    async fn get(&self) -> Result<ElectionStateSnapshot, StoreError>;

    /// `Scheduled` の間だけ、期間を書き換える。`Scheduled` でなければ何もせず `false`。
    /// 期間だけの変更は `election_audit` に記録しない（状態は変わらないため）。
    async fn schedule(&self, period: Period, at_unix_secs: i64) -> Result<bool, StoreError>;

    /// `from` → `to`（原則17の順で 1 段）を、条件付きで行う。`from` から動いていなければ成功して
    /// `true` を返し、`election_audit` に 1 行記録する。既に動いていた（他のプロセスが先に遷移させた）
    /// 場合は何もせず `false`。
    async fn transition(
        &self,
        from: ElectionPhase,
        to: ElectionPhase,
        actor: &str,
        at_unix_secs: i64,
    ) -> Result<bool, StoreError>;

    /// 変更の新しい順に、最大 `limit` 件。
    async fn recent_audit(&self, limit: usize) -> Result<Vec<ElectionAuditEntry>, StoreError>;
}
