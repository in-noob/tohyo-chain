//! 設定の読み込みと検証（CLAUDE.md の原則 11）。
//!
//! 層（後のものが勝つ）: `config/default.toml`（埋め込み）→ `config/<app.env>.toml` → `config/local.toml` →
//! `secrets/` → 環境変数 `APP__<セクション>__<項目>`。秘密情報（`session.secret`、`sealer.signing_seed`）は
//! 設定ファイルに書けず、`secrets/` か環境変数で渡す。不正な値は、起動時に「どのファイル（環境変数）の
//! どの項目がなぜ不正か」を出して終了する。
//!
//! api / sealer / verifier / bench / スクリプトは、すべてここから読む。web は、ビルド時に必要な値だけを
//! `app-config web-env` の出力（環境変数）で受け取る（web は他のワークスペースクレートに依存しない）。

mod error;
mod load;
mod model;
mod secret;
mod show;
mod time;

pub use error::{ConfigError, Issue, Origin};
pub use load::{DEFAULT_TOML, ENV_PREFIX, Loaded, SECRET_KEYS, Sources, env_name, load, load_from};
pub use model::{
    Admin, Api, App, AppConfig, Argon2, Auth, AuthMode, Chain, Credentials, Db, DbBackend,
    DisplayTimezone, Election, Env, Labels, MAX_KEYSPACE_LEN, MAX_REVOTES_LIMIT,
    MAX_SAMPLE_OPEN_HOURS, MIN_LEASE_TTL_SECS, MIN_SESSION_SECRET_LEN, Mode, RevealBallots, Sample,
    Seal, Sealer, Session, Shard, Timestamp, Web, check_keyspace,
};
pub use secret::{MASK, Secret};

/// `secrets/` の下の、再投票の鍵のファイル名（`application::REVOTE_KEY_FILE` と同じ）。
pub const REVOTE_KEY_FILE: &str = "revote_key";
/// RFC 3339 の日時（秒まで、タイムゾーンのオフセット必須）の解釈。管理用 API（`scripts/election.sh
/// schedule`）が、開始・終了時刻を解釈するのに使う（原則17）。
pub use time::parse_rfc3339;

impl Loaded {
    /// この版で実装済みの機能だけを使う設定であることを確認する。設定項目はあっても機能が未実装のもの
    /// （投票の開始時刻、締切後の公開）が指定されていたら、黙って無視せず、エラーにする。
    pub fn ensure_supported(&self) -> Result<(), ConfigError> {
        // 投票期間（election.voting_opens_at / voting_closes_at）と選挙状態の遷移は実装済み
        // （原則17・18。ADR 0019）。現時点で「未実装」として弾く項目はない。
        let _ = &self.config;
        Ok(())
    }

    /// 再投票の鍵のファイル（`<secrets_dir>/revote_key`。ADR 0022）。鍵は設定項目ではなく、このファイルにだけ置く
    /// （締切の手続きでファイルごと破棄するため、環境変数では渡せない）。secrets のディレクトリが無い（テスト用の
    /// 読み込み）なら `None`。
    pub fn revote_key_path(&self) -> Option<std::path::PathBuf> {
        self.secrets_dir
            .as_ref()
            .map(|dir| dir.join(REVOTE_KEY_FILE))
    }

    /// 項目の出所。
    pub fn origin(&self, key: &str) -> Option<&Origin> {
        self.entries.get(key).map(|entry| &entry.origin)
    }
}

/// テスト用: ファイルなし。`(項目, 値)` を環境変数として重ねた設定（例: `("seal.max_ballots", "5")`）。
pub fn load_for_test(overrides: &[(&str, &str)]) -> Result<Loaded, ConfigError> {
    load_from(&Sources::overrides(overrides))
}

#[cfg(test)]
mod tests;
