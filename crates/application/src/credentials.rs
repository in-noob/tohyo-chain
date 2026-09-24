//! ID・パスワードによる認証（`auth.mode=db`）と、その材料。
//!
//! 郵送する ID・パスワードは、`credgen` が事前に登録する。DB に残るのは、ログイン ID・パスワードのハッシュ
//! （Argon2id の PHC 文字列）・内部用の `voter_id` だけで、平文のパスワードは保存しない。
//!
//! ログインは、登録済みの組み合わせのときだけ通す。**ID が存在しない場合も、ダミーのハッシュで同じ照合処理を行い**、
//! 応答の内容も応答時間も、パスワード違いの場合と同じにする（ID が存在するかどうかを推測させないため）。

use std::sync::Arc;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use async_trait::async_trait;
use domain::{DistrictId, VoterId};

use crate::ports::{AuthError, Authenticator, Credentials, StoreError};

/// ログイン ID・パスワードに使う文字（32 種類）。見間違えやすい `0/O`・`1/I/l` を含まない（郵送で送るため）。
/// 大文字 24 字（`I` と `O` を除く）と数字 8 字（`2`〜`9`）。小文字は使わないので、`l` も出ない。
/// 32 種類なので、乱数 1 バイトの下位 5 ビットで、偏りなく選べる。
pub const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 入力の最大長（バイト）。これを超える入力は、照合は行うが、常に失敗にする（巨大な入力で計算を増やされないため）。
pub const MAX_INPUT_LEN: usize = 256;

/// ダミーの照合に使う、存在しない ID とパスワード。
const UNKNOWN_LOGIN_ID: &str = "?";
const DUMMY_PASSWORD: &str = "dummy-password-for-unknown-login-ids";

/// 入力（ログイン ID・パスワード）をそろえる: 前後・途中の空白とハイフンを除き、英字を大文字にする。
/// 郵送された文字列を、区切りを入れて書き写したり、小文字で入力したりしても通るように。
pub fn normalize_input(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// 登録済みの ID に使える形か（`ALPHABET` の文字だけ）。
pub fn is_valid_login_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_INPUT_LEN && id.bytes().all(|b| ALPHABET.contains(&b))
}

/// パスワードのハッシュ（Argon2id）のパラメータ。設定 `auth.argon2.*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("Argon2 のパラメータが不正です: {0}")]
    InvalidParams(String),
    #[error("パスワードのハッシュ化に失敗しました: {0}")]
    Hash(String),
}

/// パスワードを、ランダムなソルトで Argon2id ハッシュ化する。返すのは、パラメータとソルトを含む PHC 文字列
/// （`$argon2id$v=19$m=…,t=…,p=…$<salt>$<hash>`）。
pub fn hash_password(password: &str, params: &PasswordParams) -> Result<String, CredentialError> {
    let params = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        None,
    )
    .map_err(|e| CredentialError::InvalidParams(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    // ソルトは OS 由来の暗号論的乱数（16 バイト）。
    let salt: [u8; 16] = rand::random();
    let hash = argon2
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map_err(|e| CredentialError::Hash(e.to_string()))?;
    Ok(hash.to_string())
}

/// 保存されたハッシュ（PHC 文字列）と、パスワードを照合する。照合のパラメータは、ハッシュに含まれるもの。
/// ハッシュが壊れていて読めないときも `false`（認証を通さない）。
pub fn verify_password(phc: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 登録済みの認証情報 1 件（`credentials` テーブルの行）。
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialRecord {
    pub login_id: String,
    pub password_hash: String,
    /// 内部用のランダムな値。ログイン ID とは無関係。
    pub voter_id: VoterId,
}

// ハッシュは、ログに出ても意味は小さいが、念のため伏せる。
impl std::fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("login_id", &self.login_id)
            .field("password_hash", &"<redacted>")
            .field("voter_id", &self.voter_id)
            .finish()
    }
}

/// 認証情報の読み取り（api が使う）。
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn find(&self, login_id: &str) -> Result<Option<CredentialRecord>, StoreError>;
}

/// credgen の台帳の 1 件（`voter_registry` テーブルの行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    /// 名簿の有権者の ID（seed の `voters.csv` の `voter_id`）。
    pub external_id: String,
    pub voter_id: VoterId,
    /// 現在有効なログイン ID。
    pub login_id: String,
}

/// 認証情報の登録（credgen が使う。api は使わない）。
#[async_trait]
pub trait CredentialAdmin: Send + Sync {
    async fn registry_get(&self, external_id: &str) -> Result<Option<RegistryEntry>, StoreError>;
    /// `login_id` が未使用のときだけ登録する（LWT）。すでに使われていれば `false`（衝突。別の ID で作り直す）。
    async fn insert_credential(&self, record: &CredentialRecord) -> Result<bool, StoreError>;
    async fn delete_credential(&self, login_id: &str) -> Result<(), StoreError>;
    async fn upsert_registry(&self, entry: &RegistryEntry) -> Result<(), StoreError>;
    /// 有権者名簿（内部の `voter_id` → 属する選挙区のリスト）を、書き込む（上書き）。
    async fn put_roll(&self, voter: &VoterId, districts: &[DistrictId]) -> Result<(), StoreError>;
}

/// パスワードの照合。重い計算なので、実行するランタイムに合わせて差し替えられるようにする
/// （api は `spawn_blocking` に載せる）。
#[async_trait]
pub trait PasswordChecker: Send + Sync {
    async fn check(&self, phc: &str, password: &str) -> bool;
}

/// その場で（呼び出したタスクの上で）照合する。
#[derive(Debug, Default, Clone, Copy)]
pub struct InlineChecker;

#[async_trait]
impl PasswordChecker for InlineChecker {
    async fn check(&self, phc: &str, password: &str) -> bool {
        verify_password(phc, password)
    }
}

/// DB に事前登録した ID とパスワードで認証する（`auth.mode=db`）。
pub struct DbAuthenticator {
    store: Arc<dyn CredentialStore>,
    checker: Arc<dyn PasswordChecker>,
    /// 存在しない ID のときに照合するハッシュ。設定のパラメータで作るので、実在する ID の照合と計算量が同じ。
    dummy_hash: String,
}

impl DbAuthenticator {
    pub fn new(
        store: Arc<dyn CredentialStore>,
        checker: Arc<dyn PasswordChecker>,
        params: &PasswordParams,
    ) -> Result<Self, CredentialError> {
        Ok(Self {
            store,
            checker,
            dummy_hash: hash_password(DUMMY_PASSWORD, params)?,
        })
    }
}

#[async_trait]
impl Authenticator for DbAuthenticator {
    /// 登録済みの組み合わせのときだけ、内部の `voter_id` を返す。
    ///
    /// どの失敗（ID が存在しない・形式が不正・パスワード違い・入力が長すぎる）でも、**同じ 1 回の照合を行い**、
    /// 同じ `InvalidCredentials` を返す（応答内容も応答時間も区別できない）。マイナンバー欄は受け取って破棄する。
    async fn authenticate(&self, credentials: Credentials) -> Result<VoterId, AuthError> {
        // 所有権ごと受け取り、マイナンバーは使わずにここで破棄（drop）する。
        let Credentials {
            voter_id: login_input,
            password,
            my_number: _,
        } = credentials;
        let login_id = normalize_input(&login_input);
        let password = normalize_input(password.as_deref().unwrap_or_default());
        let well_formed = is_valid_login_id(&login_id) && password.len() <= MAX_INPUT_LEN;

        // 形式が不正でも、DB を 1 回引く（引き当てる ID は存在しない値）。応答時間の差を作らない。
        let lookup = if well_formed {
            login_id.as_str()
        } else {
            UNKNOWN_LOGIN_ID
        };
        let record = self
            .store
            .find(lookup)
            .await
            .map_err(|_| AuthError::Unavailable)?;

        let (hash, voter) = match record {
            Some(record) if well_formed => (record.password_hash, Some(record.voter_id)),
            _ => (self.dummy_hash.clone(), None),
        };
        // 入力が長すぎるときは、空のパスワードで照合する（計算量は変えず、結果は常に失敗にする）。
        let attempt = if well_formed { password.as_str() } else { "" };
        let matched = self.checker.check(&hash, attempt).await;

        match (matched && well_formed, voter) {
            (true, Some(voter)) => Ok(voter),
            _ => Err(AuthError::InvalidCredentials),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// 速く回るように、最小に近いパラメータ。
    const FAST: PasswordParams = PasswordParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };

    fn voter(id: &str) -> VoterId {
        VoterId::new(id).expect("valid")
    }

    #[test]
    fn alphabet_has_32_unambiguous_symbols() {
        let mut seen = std::collections::HashSet::new();
        for b in ALPHABET {
            assert!(seen.insert(*b), "重複: {}", *b as char);
            assert!(b.is_ascii_uppercase() || b.is_ascii_digit());
        }
        assert_eq!(seen.len(), 32);
        for banned in "01OIlo".chars() {
            assert!(!ALPHABET.contains(&(banned as u8)), "{banned} は使わない");
        }
    }

    #[test]
    fn normalization_ignores_case_spaces_and_hyphens() {
        assert_eq!(normalize_input(" abcd-efgh jk23 \n"), "ABCDEFGHJK23");
        assert!(is_valid_login_id("ABCDEFGHJK23"));
        for bad in ["", "ABC0", "ABCO", "ABC1", "ABCI", "abc", "AB-C", "AB C"] {
            assert!(!is_valid_login_id(bad), "{bad:?}");
        }
        assert!(!is_valid_login_id(&"A".repeat(MAX_INPUT_LEN + 1)));
    }

    #[test]
    fn hash_is_argon2id_phc_with_random_salt_and_verifies() {
        let a = hash_password("SECRET-PASSWORD", &FAST).expect("hash");
        let b = hash_password("SECRET-PASSWORD", &FAST).expect("hash");
        assert!(a.starts_with("$argon2id$v=19$m=8,t=1,p=1$"), "{a}");
        assert_ne!(a, b, "ソルトが毎回違う");
        assert!(!a.contains("SECRET-PASSWORD"));
        assert!(verify_password(&a, "SECRET-PASSWORD"));
        assert!(!verify_password(&a, "secret-password"));
        assert!(!verify_password(&a, ""));
        // 壊れたハッシュは、認証を通さない。
        assert!(!verify_password("not-a-phc-string", "SECRET-PASSWORD"));
        assert!(!verify_password("", ""));
    }

    #[test]
    fn verification_uses_the_parameters_stored_in_the_hash() {
        // 保存時と、今の設定でパラメータが違っても、保存されたハッシュに含まれるパラメータで照合できる。
        let old = hash_password(
            "PW12345678",
            &PasswordParams {
                memory_kib: 16,
                iterations: 2,
                parallelism: 1,
            },
        )
        .expect("hash");
        assert!(old.contains("m=16,t=2,p=1"));
        assert!(verify_password(&old, "PW12345678"));
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let bad = PasswordParams {
            memory_kib: 1,
            iterations: 0,
            parallelism: 0,
        };
        assert!(matches!(
            hash_password("x", &bad),
            Err(CredentialError::InvalidParams(_))
        ));
    }

    #[test]
    fn credential_record_debug_hides_the_hash() {
        let record = CredentialRecord {
            login_id: "ABCDEFGHJK".to_string(),
            password_hash: hash_password("PW", &FAST).expect("hash"),
            voter_id: voter("v0123456789abcdef"),
        };
        let shown = format!("{record:?}");
        assert!(!shown.contains("argon2id"), "{shown}");
        assert!(shown.contains("ABCDEFGHJK"));
    }

    /// `find` の結果を固定し、呼び出しを記録するストア。
    #[derive(Default)]
    struct FakeStore {
        records: HashMap<String, CredentialRecord>,
        fail: bool,
        lookups: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CredentialStore for FakeStore {
        async fn find(&self, login_id: &str) -> Result<Option<CredentialRecord>, StoreError> {
            self.lookups
                .lock()
                .expect("test lock")
                .push(login_id.to_string());
            if self.fail {
                return Err(StoreError::Unavailable);
            }
            Ok(self.records.get(login_id).cloned())
        }
    }

    /// 照合の呼び出し（どのハッシュに対して行ったか）を記録する。
    #[derive(Default)]
    struct RecordingChecker {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PasswordChecker for RecordingChecker {
        async fn check(&self, phc: &str, password: &str) -> bool {
            self.calls.lock().expect("test lock").push(phc.to_string());
            verify_password(phc, password)
        }
    }

    struct Fixture {
        auth: DbAuthenticator,
        store: Arc<FakeStore>,
        checker: Arc<RecordingChecker>,
        alice_hash: String,
    }

    fn fixture() -> Fixture {
        let alice_hash = hash_password("GOODPASSW0RD", &FAST).expect("hash");
        let mut store = FakeStore::default();
        store.records.insert(
            "ABCDEFGHJK".to_string(),
            CredentialRecord {
                login_id: "ABCDEFGHJK".to_string(),
                password_hash: alice_hash.clone(),
                voter_id: voter("v0123456789abcdef0123456789abcdef"),
            },
        );
        let store = Arc::new(store);
        let checker = Arc::new(RecordingChecker::default());
        let auth = DbAuthenticator::new(store.clone(), checker.clone(), &FAST).expect("auth");
        Fixture {
            auth,
            store,
            checker,
            alice_hash,
        }
    }

    fn login(id: &str, password: Option<&str>) -> Credentials {
        Credentials {
            voter_id: id.to_string(),
            password: password.map(str::to_string),
            my_number: Some("123456789012".to_string()),
        }
    }

    #[tokio::test]
    async fn registered_credentials_authenticate_and_return_the_internal_voter_id() {
        let f = fixture();
        let voter = f
            .auth
            .authenticate(login("ABCDEFGHJK", Some("GOODPASSW0RD")))
            .await
            .expect("ok");
        assert_eq!(voter.as_str(), "v0123456789abcdef0123456789abcdef");
        // 入力の大文字・小文字・区切りの違いは、そろえて照合する。
        let again = f
            .auth
            .authenticate(login(" abcde-fghjk ", Some("goodpassw0rd")))
            .await;
        assert_eq!(
            again.map(|v| v.as_str().to_string()),
            Ok("v0123456789abcdef0123456789abcdef".to_string())
        );
    }

    #[tokio::test]
    async fn every_failure_is_the_same_error_and_does_exactly_one_verification() {
        let f = fixture();
        let too_long = "A".repeat(MAX_INPUT_LEN + 1);
        let cases = [
            ("wrong password", "ABCDEFGHJK", Some("WRONGPASSWORD")),
            ("no password", "ABCDEFGHJK", None),
            ("unknown id", "ZZZZZZZZZZ", Some("GOODPASSW0RD")),
            ("malformed id", "abc0O1", Some("GOODPASSW0RD")),
            ("empty id", "", Some("GOODPASSW0RD")),
            ("too long password", "ABCDEFGHJK", Some(too_long.as_str())),
        ];
        for (name, id, password) in cases {
            let before = f.checker.calls.lock().expect("test lock").len();
            let result = f.auth.authenticate(login(id, password)).await;
            assert_eq!(result, Err(AuthError::InvalidCredentials), "{name}");
            let calls = f.checker.calls.lock().expect("test lock");
            assert_eq!(calls.len(), before + 1, "{name}: 照合は、ちょうど 1 回");
        }
        // 存在しない ID・形式が不正な ID では、実在する ID のハッシュではなく、ダミーのハッシュで照合した
        // （同じ設定のパラメータ）。
        let calls = f.checker.calls.lock().expect("test lock");
        assert_eq!(calls[0], f.alice_hash, "パスワード違いは、実在のハッシュ");
        assert_ne!(calls[2], f.alice_hash, "存在しない ID は、ダミーのハッシュ");
        assert!(
            calls[2].contains("m=8,t=1,p=1"),
            "ダミーは設定のパラメータで作る: {}",
            calls[2]
        );
        assert_eq!(calls[2], calls[3]);
        // DB は、常に 1 回ずつ引く（形式が不正な ID でも、存在しない値を引く）。
        let lookups = f.store.lookups.lock().expect("test lock");
        assert_eq!(lookups.len(), cases.len());
        assert_eq!(lookups[3], UNKNOWN_LOGIN_ID);
    }

    #[tokio::test]
    async fn store_failure_is_unavailable_not_invalid_credentials() {
        let store = FakeStore {
            fail: true,
            ..Default::default()
        };
        let auth =
            DbAuthenticator::new(Arc::new(store), Arc::new(InlineChecker), &FAST).expect("auth");
        assert_eq!(
            auth.authenticate(login("ABCDEFGHJK", Some("X"))).await,
            Err(AuthError::Unavailable)
        );
    }

    #[test]
    fn credentials_debug_redacts_password_and_my_number() {
        let shown = format!("{:?}", login("ABCDEFGHJK", Some("GOODPASSW0RD")));
        assert!(
            !shown.contains("GOODPASSW0RD") && !shown.contains("123456789012"),
            "{shown}"
        );
    }
}
