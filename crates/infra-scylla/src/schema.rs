//! スキーマ（`docs/schema.cql`）のテンプレートを、キースペース名を埋め込んで描画する。
//!
//! このクレートはスキーマを作成しない（docker compose の schema ジョブや確認スクリプトが投入する）。
//! 統合テストと計測ツールが、専用のキースペースを作るときに使う。

/// `docs/schema.cql` のテンプレート（キースペース名は `{{KEYSPACE}}`）。
pub const TEMPLATE: &str = include_str!("../../../docs/schema.cql");

const PLACEHOLDER: &str = "{{KEYSPACE}}";

/// テンプレートのキースペース名を置き換えた CQL 全体。
pub fn render(keyspace: &str) -> String {
    TEMPLATE.replace(PLACEHOLDER, keyspace)
}

/// 描画した CQL を、コメントを除いて 1 文ずつに分ける（`;` 区切り。スキーマに文字列リテラル内の `;` はない）。
pub fn statements(keyspace: &str) -> Vec<String> {
    let cql: String = render(keyspace)
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    cql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_has_no_hardcoded_keyspace() {
        // 実行ごとの専用キースペースに投入できるよう、コメント以外に固定の `vote` があってはならない。
        for line in TEMPLATE
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
        {
            // `vote.テーブル` や `KEYSPACE IF NOT EXISTS vote`（`voter_id` などの列名は対象外）。
            assert!(
                !line.contains("vote.") && !line.contains("EXISTS vote"),
                "固定のキースペース名が残っています: {line}"
            );
        }
        assert!(TEMPLATE.contains(PLACEHOLDER));
    }

    #[test]
    fn render_replaces_every_placeholder() {
        let rendered = render("vote_s6_1_2");
        assert!(!rendered.contains(PLACEHOLDER));
        assert!(rendered.contains("CREATE KEYSPACE IF NOT EXISTS vote_s6_1_2"));
        assert!(rendered.contains("CREATE TABLE IF NOT EXISTS vote_s6_1_2.participation"));
    }

    #[test]
    fn statements_are_split_and_fully_qualified() {
        let stmts = statements("ks_x");
        // キースペース 1 + テーブル 12（participation, ballot_pool, blocks, sealer_lease, anchors, signer_keys,
        // credentials, voter_roll, voter_registry, cluster_config, election_state, election_audit）
        assert_eq!(stmts.len(), 13, "{stmts:#?}");
        assert!(stmts[0].starts_with("CREATE KEYSPACE IF NOT EXISTS ks_x"));
        for stmt in &stmts[1..] {
            assert!(
                stmt.starts_with("CREATE TABLE IF NOT EXISTS ks_x."),
                "{stmt}"
            );
        }
        assert!(
            stmts
                .iter()
                .all(|s| !s.contains("--") && !s.contains('{') || s.contains("replication"))
        );
    }
}
