#!/usr/bin/env bash
# DB のリセット（app.mode=db 用）。対象のキースペースは、設定の db.keyspace（環境変数 APP__DB__KEYSPACE で変えられる）。
#
#   scripts/db_reset.sh [--votes|--all] [--yes]
#
#   --votes（既定）  投票済み記録（participation）・票のプール（ballot_pool）・ブロック（blocks）・アンカー（anchors）・
#                    リース（sealer_lease）を削除する。選挙の定義（cluster_config・signer_keys）と、認証情報
#                    （credentials・voter_roll・voter_registry）は残す。
#   --all            キースペースを削除して、docs/schema.cql から作り直す（認証情報も消える）。
#   --yes            確認を省略する。
#
# 安全のための決まり:
#   - app.env=production のときは、何も削除せずに、エラーで終了する。
#   - 削除する件数を表示して、確認を求める（--yes で省略）。
#   - api や sealer が動いていたら、止めるよう促して終了する（動いている最中に消すと、sealer が持っている
#     チェーンの状態と DB が食い違うため）。
#   - app.mode=memory では、「再起動するとリセットされます」と表示して終了する（消すものが DB にない）。
#   - DB のコンテナが止まっていれば（scripts/dev_down.sh は止める）、件数を数えるために起動する（データは消えない）。
#
# 設定は、手元の実際の設定（config/*.toml・secrets/・環境変数 APP__…）を使う（確認スクリプトのような分離はしない）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

MODE=votes
ASSUME_YES=0
for arg in "$@"; do
    case "$arg" in
        --votes) MODE=votes ;;
        --all) MODE=all ;;
        --yes) ASSUME_YES=1 ;;
        -h | --help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "不明な引数: $arg（使い方: scripts/db_reset.sh [--votes|--all] [--yes]）" >&2
            exit 2
            ;;
    esac
done

fail() {
    echo "エラー: $1" >&2
    exit 1
}

# 実際の設定を使う（cfg_init の分離をしない）。
export CFG_USE_REAL=1
source "$SCRIPT_DIR/lib/common.sh"
cfg_build
APP_ENV="$(cfg_get app.env)" || fail "設定を読み込めません（上のメッセージを参照）"
APP_MODE="$(cfg_get app.mode)"

# --- 1. 本番では、何も削除しない ---
if [[ "$APP_ENV" == "production" ]]; then
    fail "app.env=production では、DB のリセットは実行できません（何も削除していません）。"
fi

# --- 2. memory モードは、消すものが DB にない ---
if [[ "$APP_MODE" == "memory" ]]; then
    echo "app.mode=memory: 保存先は api プロセスのメモリです。api を再起動するとリセットされます（DB のリセットは不要です）。"
    echo "（DB をリセットするなら、app.mode=db の設定で実行してください。scripts/dev_up.sh cassandra と同じにするには: APP__APP__MODE=db scripts/db_reset.sh ...）"
    exit 0
fi

# --- 3. api / sealer が動いていたら、止めるよう促して終了 ---
running="$({ pgrep -a -x api || true; pgrep -a -x sealer || true; } | sed 's/^/  /')"
if [[ -n "$running" ]]; then
    echo "エラー: api または sealer が動いています:" >&2
    echo "$running" >&2
    echo "動いている最中に消すと、sealer が持っているチェーンの状態と DB が食い違います。" >&2
    echo "先に止めてください（scripts/dev_down.sh、または各プロセスに SIGTERM）。何も削除していません。" >&2
    exit 1
fi

# --- 4. DB（docker compose）とキースペースの確認 ---
db_setup_vars
KS="$(cfg_get db.keyspace)"
db_check_docker
if [[ -z "$("${COMPOSE[@]}" ps -q "$DB_SERVICE" 2>/dev/null)" || -z "$("${COMPOSE[@]}" ps -q --status running "$DB_SERVICE" 2>/dev/null)" ]]; then
    # scripts/dev_down.sh は、DB のコンテナも止める（データは残る）。消す前に、件数を数えるので、起動して healthy を待つ。
    echo "DB のコンテナ（${DB_SERVICE}）が止まっているので、起動します（データは消えていません）。"
    db_ensure
fi
cql() { "${COMPOSE[@]}" exec -T "$DB_SERVICE" cqlsh --request-timeout=180 -e "$1"; }
keyspace_exists="$(cql "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = '${KS}'" 2>/dev/null | tr -d ' ' | grep -cx "${KS}" || true)"
if [[ "$keyspace_exists" != 1 ]]; then
    if [[ "$MODE" == all ]]; then
        echo "キースペース ${KS} が存在しません。スキーマから作り直します。"
    else
        fail "キースペース ${KS} が存在しません（何も削除していません）。"
    fi
fi

# table_count TABLE → 件数（テーブルが無ければ「なし」）
table_count() {
    local out
    out="$(cql "SELECT count(*) FROM ${KS}.$1" 2>/dev/null | grep -E '^\s*[0-9]+\s*$' | tr -d ' ' | head -1 || true)"
    echo "${out:-なし}"
}

VOTE_TABLES=(participation ballot_pool blocks anchors sealer_lease)
# election_state / election_audit（原則17）は投票データではなく選挙の状態なので、--votes では残す
# （closed のまま票だけ消しても、再投票はできない。状態を scheduled からやり直すには --all を使う）。
KEEP_TABLES=(credentials voter_roll voter_registry cluster_config signer_keys election_state election_audit)

echo "対象: キースペース ${KS}（${DB_BACKEND}。app.env=${APP_ENV}）"
if [[ "$MODE" == votes ]]; then
    echo "モード --votes: 投票に関するデータを削除します（選挙の定義と認証情報は残します）。"
    echo "  削除する件数:"
    for t in "${VOTE_TABLES[@]}"; do printf '    %-14s %s\n' "$t" "$(table_count "$t")"; done
    echo "  残す件数:"
    for t in "${KEEP_TABLES[@]}"; do printf '    %-14s %s\n' "$t" "$(table_count "$t")"; done
else
    echo "モード --all: キースペースを削除して、スキーマから作り直します（認証情報も消えます）。"
    echo "  削除する件数:"
    for t in "${VOTE_TABLES[@]}" "${KEEP_TABLES[@]}"; do printf '    %-14s %s\n' "$t" "$(table_count "$t")"; done
fi

# --- 5. 確認 ---
if [[ "$ASSUME_YES" != 1 ]]; then
    printf '本当に削除しますか？ 続けるには yes と入力してください: '
    answer=""
    read -r answer || true
    if [[ "$answer" != yes ]]; then
        echo "中止しました（何も削除していません）。" >&2
        exit 1
    fi
fi

# --- 6. 実行 ---
if [[ "$MODE" == votes ]]; then
    for t in "${VOTE_TABLES[@]}"; do
        cql "TRUNCATE ${KS}.${t}" >/dev/null
    done
    echo "削除しました: ${VOTE_TABLES[*]}（選挙の定義と認証情報は残しています）。"
    echo "次に sealer を起動すると、ジェネシスから始まります。"
else
    cql "DROP KEYSPACE IF EXISTS ${KS}" >/dev/null
    SCHEMA_KEYSPACE="$KS" "${COMPOSE[@]}" run --rm "$SCHEMA_SERVICE" >/dev/null 2>&1 \
        || fail "スキーマの投入に失敗しました（キースペース ${KS}）"
    echo "キースペース ${KS} を削除して、スキーマから作り直しました（認証情報は空です。credgen で再登録してください）。"
fi
