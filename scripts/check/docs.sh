#!/usr/bin/env bash
# docs スイート: 呼び名・環境変数名・スクリプト・文書とコードの一貫性（対応表: docs/testing.md）。
#   1. 旧来の呼び名「コンテスト」が、コード・設定・選挙データ・スクリプト・README・CLAUDE.md・docs/*.md のどこにもない
#      （画面・API・集計の呼び名は labels.ballot_item から読む: 原則12）
#   2. 旧来の環境変数名（SESSION_SECRET など）が残っていない。app-config 以外のコードが、環境変数を直接読んでいない。
#      CLAUDE.md に原則 11（設定の集約）の記載がある
#   3. 封印ルールの旧名が残っていない
#   4. common.sh を読み込むスクリプトが、1 回だけ読み込み、common.sh と同じ名前の関数を定義しない（許可した上書きを除く）
#   5. すべてのシェルスクリプトが構文として正しい（bash -n）
#   6. docs/environment.md の版の区間が、scripts/env_report.sh で再生成した内容と一致する
#   7. config/default.toml のすべての項目が、docs/configuration.md に書かれている
#   8. README.md・CLAUDE.md・docs/*.md に出てくるスクリプト名が実在し、--help が終了コード 0 で使い方を表示する
set -euo pipefail

# --help: 先頭のコメント（使い方）を表示して終わる（何も起動・変更しない。scripts/check/docs.sh#8 が確認する）。
[[ "${1:-}" == -h || "${1:-}" == --help ]] && { sed -n '2,/^[^#]/{/^#/s/^# \{0,1\}//p}' "$0"; exit 0; }

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "== 1. 旧来の呼び名「コンテスト」が、どこにもない"
OLD_WORD="コン""テスト"
hits="$(grep -rn "$OLD_WORD" crates config seed scripts README.md CLAUDE.md docs/*.md 2>/dev/null \
    | grep -v 'scripts/check/docs.sh' || true)"
if [[ -n "$hits" ]]; then
    echo "$hits" >&2
    fail "旧来の呼び名が残っています（画面・API・集計の呼び名は labels.ballot_item から読む）"
fi
echo "crates / config / seed / scripts / README / CLAUDE.md / docs: 0 件 OK"

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

echo "== 3. 封印ルールの旧名が残っていない（原則9。ADR 0020）"
# 旧ルールの設定名・判定・ログのきっかけ（max_interval_secs・ResetWindowOnly・decide_flush・trigger=flush）が、
# コード・設定・スクリプト・README・CLAUDE.md・docs/*.md に残っていない（ADR は経緯の記録なので対象外）。
OLD_SEAL='(max_interval_secs|APP__SEAL__MAX_INTERVAL_SECS|ResetWindowOnly|decide_flush|trigger=flush|Trigger::Flush|窓をリセットしました)'
old_seal_hits="$(grep -rnE "$OLD_SEAL" crates config scripts README.md CLAUDE.md docs/*.md 2>/dev/null \
    | grep -v 'scripts/check/docs.sh' || true)"
if [[ -n "$old_seal_hits" ]]; then
    echo "$old_seal_hits" >&2
    fail "封印ルールの旧名が残っています（seal.interval_secs / seal.min_ballots_after_interval / trigger=close へ）"
fi
grep -q 'seal.min_ballots_after_interval' CLAUDE.md && grep -q 'CloseFlush' CLAUDE.md || fail "CLAUDE.md に原則9 の実装（ADR 0020）の記載がありません"
echo "封印ルールの旧名なし / CLAUDE.md の記載: OK"

echo "== 4. scripts/lib/common.sh の関数を、読み込んだスクリプトが上書きしない"
# bash では、同じ名前の関数を後から定義すると、前の定義が置き換わる。引数の形が違う同名の関数があると、どちらが
# 呼ばれるかが読み込みの順番で変わり、壊れても構文の検査（bash -n）では見つからない（dev_up.sh cassandra が、
# 途中で common.sh を読み直して自身の alive / wait_until を置き換えられ、起動待ちで必ず失敗していた）。
#   - common.sh を読み込むのは、1 つのスクリプトにつき 1 回だけ
#   - common.sh と同じ名前の関数を定義しない（例外: fail は、common.sh が「未定義のときだけ」既定を定義するので、
#     各スクリプトが先に定義してよい。確認スイートの count / pool_total は、スイートの中だけで意図して置き換えている）
COMMON_FUNCS="$(grep -oE '^[[:space:]]*[a-z_][a-z0-9_]*\(\)' scripts/lib/common.sh | tr -d '() ' | sort -u)"
ALLOWED_OVERRIDES='^(fail|count|pool_total)$'
shadow_hits=""
for f in scripts/*.sh scripts/check/*.sh; do
    sources="$(grep -cE '^[[:space:]]*(source|\.)[[:space:]].*lib/common\.sh' "$f" || true)"
    [[ "$sources" == 0 ]] && continue
    [[ "$sources" == 1 ]] || shadow_hits+="${f}: common.sh を ${sources} 回読み込んでいます"$'\n'
    while read -r name; do
        [[ -z "$name" || "$name" =~ $ALLOWED_OVERRIDES ]] && continue
        grep -qx "$name" <<<"$COMMON_FUNCS" && shadow_hits+="${f}: common.sh と同じ名前の関数 ${name}() を定義しています"$'\n'
    done < <(grep -oE '^[[:space:]]*[a-z_][a-z0-9_]*\(\)' "$f" | tr -d '() ' | sort -u)
done
if [[ -n "$shadow_hits" ]]; then
    printf '%s' "$shadow_hits" >&2
    fail "common.sh の関数を上書きしているスクリプトがあります（別の名前にするか、common.sh の関数を使ってください）"
fi
echo "common.sh の読み込みは 1 回だけ・同名の関数の上書きなし: OK"

echo "== 5. 全スクリプトの構文（bash -n）"
n=0
for f in scripts/*.sh scripts/check/*.sh scripts/lib/*.sh; do
    bash -n "$f" || fail "$f に構文エラーがあります"
    n=$((n + 1))
done
echo "構文チェック: ${n} 本 OK"

echo "== 6. docs/environment.md の版が、scripts/env_report.sh で再生成した内容と一致する"
# 版は rust-toolchain.toml・crates/web/Trunk.toml・Cargo.lock・docker-compose.yml と、各ツールの --version から作る。
# 手順（docs/environment.md の「入れ方」）どおりに入れた環境なら、どの機械でも同じ出力になる。
scripts/env_report.sh --check || fail "docs/environment.md の版が古いか、手順どおりのツールが入っていません（上の差分を参照）"

echo "== 7. config/default.toml のすべての項目が、docs/configuration.md に書かれている"
# 「セクション.項目」（[auth.argon2] の下は auth.argon2.memory_kib など）を、バッククォートで囲んだ形で探す。
missing_keys=""
n=0
while read -r key; do
    n=$((n + 1))
    grep -qF "\`${key}\`" docs/configuration.md || missing_keys+="  - ${key}"$'\n'
done < <(awk '
    /^\[[a-z0-9_.]+\][[:space:]]*$/ { section = substr($0, 2, index($0, "]") - 2); next }
    /^[a-z0-9_]+[[:space:]]*=/ { key = $1; sub(/=.*/, "", key); print section "." key }
' config/default.toml)
((n > 0)) || fail "config/default.toml から項目を読み取れません"
if [[ -n "$missing_keys" ]]; then
    printf '%s' "$missing_keys" >&2
    fail "docs/configuration.md に書かれていない設定項目があります（上の一覧）"
fi
echo "設定項目 ${n} 件がすべて docs/configuration.md にある: OK"

echo "== 8. 文書に出てくるスクリプトが実在し、--help が動く"
# --help は、何も起動・変更せずに、先頭のコメント（使い方）を表示して終わる決まり。
n=0
while read -r script; do
    n=$((n + 1))
    [[ -f "$script" ]] || fail "文書に出てくる ${script} がありません（$(grep -lF "$script" README.md CLAUDE.md docs/*.md | paste -sd, -)）"
    help_out="$(timeout 30 bash "$script" --help </dev/null 2>&1)" || fail "${script} --help が失敗しました: ${help_out}"
    [[ -n "$help_out" ]] || fail "${script} --help が何も表示しません"
done < <(grep -ohE 'scripts/[A-Za-z0-9_./-]+\.sh' README.md CLAUDE.md docs/*.md | sort -u)
((n > 0)) || fail "文書からスクリプト名を読み取れません"
echo "スクリプト ${n} 本: 実在し、--help が動く OK"

echo "OK: check/docs.sh"
