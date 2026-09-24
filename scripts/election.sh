#!/usr/bin/env bash
# 選挙状態（scheduled → open → closing → closed）の管理操作（原則17・18。ADR 0019）。
# 公開用のポート（api.port）ではなく、管理用リスナー（admin.bind。既定 127.0.0.1:18081）に curl する。
#
#   scripts/election.sh status
#   scripts/election.sh schedule --opens-at <RFC3339> --closes-at <RFC3339>
#   scripts/election.sh open --now [--yes]
#   scripts/election.sh close --now [--yes]
#
#   schedule は、状態が scheduled のときだけ実行できる。
#   open --now / close --now は確認を求める（--yes で省略）。close --now の後の締切の手続き
#   （待ち時間・全シャードのフラッシュ・最終アンカー・closed への遷移）は、アンカーのリースを持つ
#   sealer が自動で行う（memory モードでは、api 内蔵のスケジューラが行う）。
#   status は、状態・期間・残り時間・シャードごとの未封印件数・最後のブロック・監査ログの直近 5 件を表示する。
#
# 接続先は、設定の admin.bind（環境変数 APP_CONFIG_DIR / APP_SECRETS_DIR / APP__… は手元の実際の設定を使う。
# scripts/check/*.sh のような分離はしない）。トークンは、api と同じ場所（環境変数 APP__ADMIN__TOKEN、
# 無ければ secrets/admin_token）から読む。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

USAGE="使い方: scripts/election.sh status | schedule --opens-at <RFC3339> --closes-at <RFC3339> | open --now [--yes] | close --now [--yes]"

fail() {
    echo "エラー: $1" >&2
    exit 1
}

CFG_USE_REAL=1
source "$SCRIPT_DIR/lib/common.sh"
cfg_build

CMD="${1:-}"
[[ -n "$CMD" ]] || fail "$USAGE"
shift || true

OPENS_AT=""
CLOSES_AT=""
ASSUME_YES=0
while (($# > 0)); do
    case "$1" in
        --opens-at)
            OPENS_AT="${2:?--opens-at には値が必要です}"
            shift
            ;;
        --closes-at)
            CLOSES_AT="${2:?--closes-at には値が必要です}"
            shift
            ;;
        --now) ;; # open --now / close --now の見た目のための引数（値は使わない）
        --yes) ASSUME_YES=1 ;;
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        *)
            fail "不明な引数: $1（$USAGE）"
            ;;
    esac
    shift
done

BIND="$(cfg_get admin.bind)" || fail "設定を読み込めません（上のメッセージを参照）"
BASE="http://${BIND}"
TOKEN="$(admin_token)"
[[ -n "$TOKEN" ]] || fail "admin.token が未設定です（環境変数 APP__ADMIN__TOKEN か secrets/admin_token で渡してください。scripts/dev_up.sh は開発用の固定値を自動で使う）"

# GNU date（-d）と BSD/macOS date（-v ... -j -f）の両方に対応する。
fmt_time() {
    local secs="$1"
    date -u -d "@${secs}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
        || date -u -r "${secs}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
        || echo "${secs}"
}

# JSON から 1 つの値を取り出す（トップレベルの文字列・数値・null だけを想定した簡易な抜き出し）。
json_get() {
    local body="$1" key="$2"
    sed -n "s/.*\"${key}\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}[,}].*/\1/p" <<<"$body" | head -1
}

admin_request() {
    local method="$1" path="$2" data="${3:-}"
    request "$method" "$path" "$TOKEN" "$data"
}

print_status() {
    admin_request GET /admin/v1/election
    expect_status 200 "GET /admin/v1/election"
    local phase opens_at closes_at now closing_started_at
    phase="$(json_get "$BODY" phase)"
    opens_at="$(json_get "$BODY" opens_at)"
    closes_at="$(json_get "$BODY" closes_at)"
    now="$(json_get "$BODY" now)"
    closing_started_at="$(json_get "$BODY" closing_started_at)"

    echo "状態: ${phase}"
    if [[ -n "$opens_at" && "$opens_at" != null ]]; then
        echo "開始: ${opens_at}（$(fmt_time "$opens_at")）"
    else
        echo "開始: 未設定"
    fi
    if [[ -n "$closes_at" && "$closes_at" != null ]]; then
        echo "終了: ${closes_at}（$(fmt_time "$closes_at")）"
    else
        echo "終了: 未設定"
    fi
    if [[ "$phase" == scheduled && -n "$opens_at" && "$opens_at" != null ]]; then
        echo "残り時間（開始まで）: $((opens_at - now)) 秒"
    elif [[ "$phase" == open && -n "$closes_at" && "$closes_at" != null ]]; then
        echo "残り時間（終了まで）: $((closes_at - now)) 秒"
    elif [[ "$phase" == closing && -n "$closing_started_at" && "$closing_started_at" != null ]]; then
        echo "締切の手続きの開始: $(fmt_time "$closing_started_at")（$((now - closing_started_at)) 秒前）"
    fi

    echo "シャードごとの未封印: $(sed -n 's/.*"pending_by_shard":\[\([^]]*\)\].*/\1/p' <<<"$BODY")"
    echo "最後のブロック（シャードごと）:"
    python3 - "$BODY" <<'PY' 2>/dev/null || echo "  （表示できません。生の応答: 下記）"
import json, sys
data = json.loads(sys.argv[1])
for h in data.get("heads", []):
    print(f"  shard={h['shard']} height={h.get('height')} block_hash={h.get('block_hash')}")
print("直近の監査ログ:")
for e in data.get("recent_audit", []):
    print(f"  {e['at_unix_secs']}: {e['from']} -> {e['to']} ({e['actor']})")
PY
    if ! command -v python3 >/dev/null 2>&1; then
        echo "（heads・recent_audit の整形表示には python3 が必要です。生の応答:）"
        echo "$BODY"
    fi
}

confirm_or_exit() {
    local action="$1"
    [[ "$ASSUME_YES" == 1 ]] && return 0
    printf '%s。本当に実行しますか？ 続けるには yes と入力してください: ' "$action"
    local answer=""
    read -r answer || true
    [[ "$answer" == yes ]] || fail "中止しました（何も変更していません）。"
}

case "$CMD" in
    status)
        print_status
        ;;
    schedule)
        [[ -n "$OPENS_AT" && -n "$CLOSES_AT" ]] || fail "schedule には --opens-at と --closes-at が両方必要です（$USAGE）"
        admin_request POST /admin/v1/election/schedule \
            "{\"opens_at\":\"${OPENS_AT}\",\"closes_at\":\"${CLOSES_AT}\"}"
        if [[ "$STATUS" == 200 ]]; then
            echo "設定しました（scheduled の間だけ実行できます）。"
            print_status
        else
            fail "schedule に失敗しました（status=${STATUS}）: ${BODY}"
        fi
        ;;
    open)
        confirm_or_exit "投票の受付を開始します（scheduled -> open）"
        admin_request POST /admin/v1/election/open
        if [[ "$STATUS" == 200 ]]; then
            echo "開始しました。"
            print_status
        else
            fail "open に失敗しました（status=${STATUS}）: ${BODY}"
        fi
        ;;
    close)
        confirm_or_exit "投票の受付を締め切ります（open -> closing。締切の手続きは自動で進みます）"
        admin_request POST /admin/v1/election/close
        if [[ "$STATUS" == 200 ]]; then
            echo "締切の手続きを開始しました（closing）。closed になるまで、しばらくお待ちください（scripts/election.sh status で確認できます）。"
            print_status
        else
            fail "close に失敗しました（status=${STATUS}）: ${BODY}"
        fi
        ;;
    *)
        fail "不明なコマンド: ${CMD}（$USAGE）"
        ;;
esac
