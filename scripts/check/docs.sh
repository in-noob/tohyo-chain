#!/usr/bin/env bash
# docs スイート: 呼び名・環境変数名・スクリプトの構文の一貫性（対応表: docs/testing.md）。
#   1. 旧来の呼び名「コンテスト」が、コード・設定・選挙データ・スクリプト・README・CLAUDE.md のどこにもない
#      （画面・API・集計の呼び名は labels.ballot_item から読む: 原則12）
#   2. 旧来の環境変数名（SESSION_SECRET など）が残っていない。app-config 以外のコードが、環境変数を直接読んでいない。
#      CLAUDE.md に原則 11（設定の集約）の記載がある
#   3. すべてのシェルスクリプトが構文として正しい（bash -n）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "== 1. 旧来の呼び名「コンテスト」が、どこにもない"
OLD_WORD="コン""テスト"
hits="$(grep -rn "$OLD_WORD" crates config seed scripts README.md CLAUDE.md docs/manual_check_step5.md 2>/dev/null \
    | grep -v 'scripts/check/docs.sh' || true)"
if [[ -n "$hits" ]]; then
    echo "$hits" >&2
    fail "旧来の呼び名が残っています（画面・API・集計の呼び名は labels.ballot_item から読む）"
fi
echo "crates / config / seed / scripts / README / CLAUDE.md: 0 件 OK"

echo "== 2. 旧来の環境変数名・環境変数の直接参照・CLAUDE.md の記載"
LEGACY='(SESSION_SECRET|SEAL_MAX_BALLOTS|SEAL_MAX_INTERVAL_SECS|SHARD_COUNT|SEALER_SIGNING_SEED|SEALER_LEASE_TTL_SECS|SEALER_ID|SCYLLA_NODES|SCYLLA_KEYSPACE|SCYLLA_URI|ELECTION_SEED_PATH|SESSION_TTL_SECS|STORAGE|API_PORT)'
# 旧名（APP__ が付かない裸の名前）を、実行するコードとスクリプトから探す。コメントの説明文は対象外にできないので、行全体で検査する。
legacy_hits="$(grep -rnE "(^|[^A-Za-z0-9_])${LEGACY}([^A-Za-z0-9_]|\$)" crates scripts docker-compose.yml 2>/dev/null \
    | grep -v 'scripts/check/docs.sh' || true)"
if [[ -n "$legacy_hits" ]]; then
    echo "$legacy_hits" >&2
    fail "旧来の環境変数名が残っています（APP__<セクション>__<項目> に移行してください）"
fi
# 設定を環境変数から直接読んでいるコードが無い（app-config 以外）。
direct_env="$(grep -rnE 'env::var(_os)?\(' crates --include=*.rs | grep -v '^crates/app-config/' | grep -vE 'BENCH_KEYSPACE_PREFIX|TEST_KEYSPACE_PREFIX' || true)"
if [[ -n "$direct_env" ]]; then
    echo "$direct_env" >&2
    fail "app-config 以外のコードが環境変数を直接読んでいます"
fi
grep -q '11\.' CLAUDE.md && grep -q 'config/' CLAUDE.md || fail "CLAUDE.md に原則 11（設定の集約）がありません"
echo "旧来の環境変数名なし / 環境変数の直接参照なし / CLAUDE.md の記載: OK"

echo "== 3. 全スクリプトの構文（bash -n）"
n=0
for f in scripts/*.sh scripts/check/*.sh scripts/lib/*.sh; do
    bash -n "$f" || fail "$f に構文エラーがあります"
    n=$((n + 1))
done
echo "構文チェック: ${n} 本 OK"

echo "OK: check/docs.sh"
