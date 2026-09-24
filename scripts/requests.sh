#!/usr/bin/env bash
# scripts/requests.http と同等の curl スクリプト（動作確認用のデモ。検証は scripts/check/election.sh）。
#
#   APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api --features dev-tools   # 別ターミナル（api.port の既定は 18080）
#   scripts/requests.sh
#
# 環境変数: BASE_URL（既定 http://localhost:18080）, VOTER_ID（既定 demo-<乱数>）
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:18080}"
VOTER_ID="${VOTER_ID:-demo-$RANDOM}"

# call TITLE METHOD PATH [TOKEN] [JSON]
call() {
    local title="$1" method="$2" path="$3" token="${4:-}" data="${5:-}"
    local args=(-sS -w '\nHTTP %{http_code}\n' -X "$method" "${BASE_URL}${path}")
    [[ -n "$token" ]] && args+=(-H "Authorization: Bearer ${token}")
    [[ -n "$data" ]] && args+=(-H 'Content-Type: application/json' -d "$data")
    echo "### ${title}"
    echo "${method} ${path}"
    curl "${args[@]}"
    echo
}

call "ヘルスチェック" GET /healthz

echo "### ログイン（voter_id=${VOTER_ID}）"
login_json="$(curl -sS -X POST "${BASE_URL}/api/v1/login" \
    -H 'Content-Type: application/json' \
    -d "{\"voter_id\":\"${VOTER_ID}\",\"my_number\":\"123456789012\"}")"
echo "${login_json}"
TOKEN="$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' <<<"${login_json}")"
[[ -n "${TOKEN}" ]] || { echo "トークンを取得できませんでした" >&2; exit 1; }
echo

# 有権者は、サンプルの選挙データ seed/2026-general の alice（東京 1 区）の想定。
# 投票用紙の ID は `{election_id}/{district_id}`、候補者の ID は `{district_id}.c{連番}`。
SMD=/api/v1/contests/2026-general/shugiin_smd.13.01
call "自分に関係する投票用紙（表示順・固定）" GET /api/v1/ballot-status "${TOKEN}"
call "候補者取得（東京 1 区）" GET "${SMD}/candidates" "${TOKEN}"
call "投票 → 201" POST "${SMD}/vote" "${TOKEN}" '{"candidate_id":"shugiin_smd.13.01.c1"}'
call "再投票 → 409" POST "${SMD}/vote" "${TOKEN}" '{"candidate_id":"shugiin_smd.13.01.c2"}'
call "次の投票用紙（比例東京）へ投票 → 201" POST /api/v1/contests/2026-general/shugiin_pr.tokyo/vote "${TOKEN}" '{"candidate_id":"shugiin_pr.tokyo.c3"}'
call "候補者がその投票用紙にいない → 422" POST "${SMD}/vote" "${TOKEN}" '{"candidate_id":"shugiin_pr.tokyo.c1"}'
call "対象外の投票用紙（大阪 1 区）→ 403" GET /api/v1/contests/2026-general/shugiin_smd.27.01/candidates "${TOKEN}"
call "存在しない投票用紙 → 404" GET /api/v1/contests/2026-general/shugiin_smd.99.99/candidates "${TOKEN}"
call "トークンなし → 401" GET /api/v1/ballot-status
call "状態取得（投票後）" GET /api/v1/ballot-status "${TOKEN}"
call "シャードごとの未封印件数（dev-tools 有効時のみ）" GET /debug/pool
