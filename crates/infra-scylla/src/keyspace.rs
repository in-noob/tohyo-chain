//! キースペース名の検査。名前は CQL の識別子として、完全修飾名（`<keyspace>.<table>`）に埋め込まれる。
//! 設定（`app-config`）でも同じ規則で検査するが、DB に接続する側でも防御として検査する（CQL の注入を防ぐ）。

/// キースペース名の最大長（Cassandra / ScyllaDB の上限は 48 文字）。
pub const MAX_KEYSPACE_LEN: usize = 48;

/// 先頭が英字で、英数字とアンダースコアだけ、48 文字以内。
pub fn validate_keyspace(keyspace: &str) -> Result<(), String> {
    let valid = keyspace.len() <= MAX_KEYSPACE_LEN
        && keyspace
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && keyspace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "キースペース名が不正です: {keyspace:?}（先頭は英字、英数字・_ のみ、{MAX_KEYSPACE_LEN} 文字以内）"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyspace_validation() {
        assert!(validate_keyspace("vote").is_ok());
        assert!(validate_keyspace("vote_prod").is_ok());
        // 確認スクリプトが実行ごとに作る専用の名前（vote_s6_<UNIXTIME>_<PID>）と、長さの上限。
        assert!(validate_keyspace("vote_s6_1789912345_123456").is_ok());
        assert!(validate_keyspace(&"a".repeat(48)).is_ok());
        for bad in [
            "",
            "vote; DROP",
            "a-b",
            "a.b",
            "1vote",
            "_vote",
            &"a".repeat(49),
        ] {
            assert!(validate_keyspace(bad).is_err(), "{bad:?}");
        }
    }
}
