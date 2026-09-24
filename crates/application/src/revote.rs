//! 再投票の仮名 `slot` と、その鍵 `revote_key`（CLAUDE.md 原則1・ADR 0022）。
//!
//! `slot = HMAC-SHA256(revote_key, election_id ‖ voter_id ‖ contest_id)`。同じ有権者・同じ投票用紙の票（再投票の各版）
//! に共通の値で、票と一緒に記録してよい唯一の仮名。鍵が無ければ、slot から投票者 ID には戻せない。
//!
//! 鍵は `secrets/revote_key`（1 ファイル。64 桁の hex）にだけ置き、ログにも DB にも出力しない。締切の手続きの中で、
//! ファイルごと破棄する（[`RevoteKeyVault::destroy`]）。環境変数で渡せないのは、プロセスの外に残って消せないため。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use domain::{ContestId, ElectionId, Slot, VoterId};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 鍵の長さ（バイト）。
pub const REVOTE_KEY_LEN: usize = 32;
/// `secrets/` の下の、鍵のファイル名。
pub const REVOTE_KEY_FILE: &str = "revote_key";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RevoteKeyError {
    #[error("revote_key は {REVOTE_KEY_LEN} バイト（64 桁の hex）にしてください")]
    Malformed,
    #[error("revote_key を読めません: {0}")]
    Io(String),
}

/// 再投票の鍵。値は `Debug` に出さず、破棄（drop）のときに 0 で上書きする。
pub struct RevoteKey([u8; REVOTE_KEY_LEN]);

impl RevoteKey {
    pub fn new(bytes: [u8; REVOTE_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// 64 桁の hex（前後の空白・改行は無視）から作る。
    pub fn from_hex(text: &str) -> Result<Self, RevoteKeyError> {
        let text = text.trim();
        if text.len() != REVOTE_KEY_LEN * 2 || !text.is_ascii() {
            return Err(RevoteKeyError::Malformed);
        }
        let mut out = [0u8; REVOTE_KEY_LEN];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| RevoteKeyError::Malformed)?;
        }
        Ok(Self(out))
    }

    /// `HMAC-SHA256(revote_key, election_id ‖ voter_id ‖ contest_id)`。
    ///
    /// 各 ID の前に、2 バイトの固定幅（ビッグエンディアン）の長さを置く（原則4 と同じ理由: ID の境界が曖昧になって、
    /// 別の組み合わせが同じ入力にならないように）。
    pub fn slot(&self, election: &ElectionId, voter: &VoterId, contest: &ContestId) -> Slot {
        // HMAC は、ブロック長（64 バイト）より短い鍵を 0 で埋めて使う（RFC 2104）。32 バイトの鍵を自分で 0 埋めして
        // 固定長の鍵として渡すので、長さの検査（失敗し得る `new_from_slice`）が要らない。
        let mut padded = [0u8; 64];
        padded[..REVOTE_KEY_LEN].copy_from_slice(&self.0);
        let mut mac = <HmacSha256 as KeyInit>::new(&padded.into());
        padded.fill(0);
        for part in [election.as_str(), voter.as_str(), contest.as_str()] {
            // ID の最大長は検証済み（どれも 100 文字未満）なので、`u16` に収まる。
            mac.update(&(part.len() as u16).to_be_bytes());
            mac.update(part.as_bytes());
        }
        Slot(mac.finalize().into_bytes().into())
    }
}

impl Drop for RevoteKey {
    fn drop(&mut self) {
        // ベストエフォート: メモリ上の鍵を 0 で上書きしてから解放する（unsafe を使わないので、最適化で
        // 消されない保証はない。`black_box` で、書き込みが不要と判断されにくくする）。
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl fmt::Debug for RevoteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RevoteKey(<redacted>)")
    }
}

/// 鍵を破棄した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDestroyed {
    /// 保存先（ファイル）から消した。
    Removed,
    /// 保存先には、もともと無かった（メモリ上の鍵は捨てた）。
    AlreadyAbsent,
}

/// 再投票の鍵の置き場所（`secrets/revote_key`）と、プロセスのメモリ上の写し。
///
/// api は、投票（slot の計算）のたびにメモリ上の写しを使い、選挙状態が closing 以降になったら写しを捨てる
/// （[`RevoteKeyVault::forget`]）。締切の手続き（sealer / memory モードの api 内蔵のスケジューラ）は、ファイルごと消す
/// （[`RevoteKeyVault::destroy`]）。
pub struct RevoteKeyVault {
    path: Option<PathBuf>,
    key: Mutex<Option<Arc<RevoteKey>>>,
}

impl RevoteKeyVault {
    /// `path` にファイルがあれば読み込む（無ければ鍵なし）。形式が不正ならエラー。
    pub fn load(path: &Path) -> Result<Self, RevoteKeyError> {
        let key = match std::fs::read_to_string(path) {
            Ok(text) => Some(Arc::new(RevoteKey::from_hex(&text)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(RevoteKeyError::Io(e.to_string())),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            key: Mutex::new(key),
        })
    }

    /// 保存先を持たない（テスト・鍵を使わない構成）。
    pub fn in_memory(key: Option<RevoteKey>) -> Self {
        Self {
            path: None,
            key: Mutex::new(key.map(Arc::new)),
        }
    }

    /// 鍵なし（再投票を使わない構成）。
    pub fn none() -> Self {
        Self::in_memory(None)
    }

    /// 今の鍵（破棄済み・未設定なら `None`）。
    pub fn key(&self) -> Option<Arc<RevoteKey>> {
        self.key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn has_key(&self) -> bool {
        self.key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// メモリ上の鍵を捨てる（ファイルは消さない）。
    pub fn forget(&self) {
        self.key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// メモリ上の鍵を捨て、保存先のファイルも消す（冪等）。
    pub fn destroy(&self) -> io::Result<KeyDestroyed> {
        self.forget();
        let Some(path) = &self.path else {
            return Ok(KeyDestroyed::AlreadyAbsent);
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(KeyDestroyed::Removed),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(KeyDestroyed::AlreadyAbsent),
            Err(e) => Err(e),
        }
    }
}

impl fmt::Debug for RevoteKeyVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevoteKeyVault")
            .field("path", &self.path)
            .field("has_key", &self.has_key())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (ElectionId, VoterId, ContestId) {
        (
            ElectionId::new("2026-general").expect("valid"),
            VoterId::new("alice").expect("valid"),
            ContestId::parse("2026-general/governor.13").expect("valid"),
        )
    }

    #[test]
    fn slot_is_hmac_sha256_over_length_prefixed_ids() {
        let key = RevoteKey::new([7; 32]);
        let (e, v, c) = ids();
        // 独立実装（Python hmac + hashlib）で計算した期待値:
        // hmac.new(b"\x07"*32, b"\x00\x0c2026-general" + b"\x00\x05alice" + b"\x00\x182026-general/governor.13",
        //          hashlib.sha256).hexdigest()
        let hex: String = key
            .slot(&e, &v, &c)
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, EXPECTED_SLOT);
    }

    const EXPECTED_SLOT: &str = "6fbf44139b5d89730e728790c197147db5451242978f54c1a01f37fea328da08";

    #[test]
    fn slots_differ_by_key_voter_and_contest() {
        let (e, v, c) = ids();
        let key = RevoteKey::new([7; 32]);
        let base = key.slot(&e, &v, &c);
        assert_eq!(base, key.slot(&e, &v, &c));
        assert_ne!(base, RevoteKey::new([8; 32]).slot(&e, &v, &c));
        assert_ne!(base, key.slot(&e, &VoterId::new("bob").expect("valid"), &c));
        assert_ne!(
            base,
            key.slot(
                &e,
                &v,
                &ContestId::parse("2026-general/governor.14").expect("valid")
            )
        );
    }

    #[test]
    fn keys_are_64_hex_digits_and_never_printed() {
        let key = RevoteKey::from_hex(&format!("{}\n", "ab".repeat(32))).expect("valid");
        assert_eq!(format!("{key:?}"), "RevoteKey(<redacted>)");
        for bad in ["", "ab", &"zz".repeat(32), &"ab".repeat(33)] {
            assert_eq!(
                RevoteKey::from_hex(bad).err(),
                Some(RevoteKeyError::Malformed)
            );
        }
    }

    #[test]
    fn the_vault_loads_forgets_and_destroys_the_key_file() {
        let dir = std::env::temp_dir().join(format!(
            "revote-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(REVOTE_KEY_FILE);
        // ファイルが無い: 鍵なし。
        let vault = RevoteKeyVault::load(&path).expect("load");
        assert!(!vault.has_key());
        assert_eq!(
            vault.destroy().expect("destroy"),
            KeyDestroyed::AlreadyAbsent
        );

        std::fs::write(&path, "cd".repeat(32)).expect("write");
        let vault = RevoteKeyVault::load(&path).expect("load");
        assert!(vault.has_key());
        assert!(!format!("{vault:?}").contains("cdcd"));
        // forget はメモリだけ。ファイルは残る。
        vault.forget();
        assert!(!vault.has_key());
        assert!(path.exists());
        // destroy はファイルも消す（2 回目は、もう無い）。
        let vault = RevoteKeyVault::load(&path).expect("load");
        assert_eq!(vault.destroy().expect("destroy"), KeyDestroyed::Removed);
        assert!(!vault.has_key());
        assert!(!path.exists());
        assert_eq!(
            vault.destroy().expect("destroy"),
            KeyDestroyed::AlreadyAbsent
        );

        std::fs::write(&path, "not hex").expect("write");
        assert_eq!(
            RevoteKeyVault::load(&path).err(),
            Some(RevoteKeyError::Malformed)
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
