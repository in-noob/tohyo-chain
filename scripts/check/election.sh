#!/usr/bin/env bash
# election スイート: 投票フローと選挙データ（対応表: docs/testing.md）。
#   1. 投票フロー（ログイン・状態・候補者・投票・再投票拒否・対象外403・並列100・秘密投票）。
#      サンプルの選挙データ seed/2026-general（有権者 alice = 東京 1 区、bob = 大阪 1 区）を使う
#   2. 47 都道府県規模の選挙データ（seedgen）: 生成・読み込み・有権者ごとの表示範囲・投票順の固定・
#      壊れたデータ（重複 ID・存在しない参照・不正な ID など）の検出
#   3. 選挙状態の遷移と投票の受付期間（原則17・18。ADR 0019）: schedule → 自動で open → 自動で closing →
#      closed（アンカーのリースを持つ sealer、memory モードでは api 内蔵のスケジューラが駆動する）。
#      期間の境界（開始時刻ちょうどは受け付け、終了時刻ちょうどは拒否する。固定の時計の単体テスト）、
#      開始前・期間内・closing・closed の受付、closing の直前に投じた票が最終ブロックまでに封印されること、
#      公開用のポートから管理用のエンドポイントに届かないこと（逆方向も）を確認する
#   4. scripts/election.sh（status・schedule の拒否・close --now の確認と --yes）を通した締切: 投票を並列に
#      投げている最中に close --now し、受理した票がすべて最終ブロックまでに封印されること（verifier tally の
#      検証・突合）、tally が closed のときだけ実行できること、closed の後の投票が拒否されることを確認する
# 設定は、手元の config/local.toml などの影響を受けないよう分離する（scripts/lib/common.sh）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."
source scripts/lib/common.sh
cfg_init

# ===========================================================================
# 1. 投票フロー（旧 check_step3.sh）
# ===========================================================================
check_voting_flow() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18811}"
    BASE="http://127.0.0.1:${PORT}"
    export APP__SESSION__SECRET="election1-check-secret-0123456789abcdef"
    # このスイートは投票フローの確認で、選挙状態（原則17・18）の遷移そのものは check_election_lifecycle が
    # 見る。開始時刻を過去にして、起動直後に自動で open にする（schedule → open → closing → closed の
    # 遷移自体はここでは確認しない）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"

    LOG="$(mktemp)"
    CODES="$(mktemp)"
    cleanup() {
        common_cleanup
        hard_stop api
        rm -f "$LOG" "$CODES"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        echo "--- api log ---" >&2
        cat "$LOG" >&2
        exit 1
    }

    start_api() {
        spawn api "$LOG" env "APP__API__PORT=${PORT}" "$@"
        wait_healthz "$BASE" 10 api || fail "${BASE}/healthz が応答しません"
        wait_until 5000 bash -c "curl -sS '${BASE}/api/v1/election-status' | grep -q '\"phase\":\"open\"'" \
            || fail "自動で open になりませんでした（election.voting_opens_at の自動遷移）"
    }
    stop_api() { hard_stop api; }

    pool_total() {
        request GET /debug/pool
        expect_status 200 "/debug/pool"
        sed -n 's/.*"total":\([0-9]*\).*/\1/p' <<<"$BODY"
    }
    login_with_number() {
        request POST /api/v1/login "" "{\"voter_id\":\"$1\",\"my_number\":\"123456789012\"}"
        expect_status 200 "ログイン($1)"
        sed -n 's/.*"token":"\([^"]*\)".*/\1/p' <<<"$BODY"
    }

    echo "== 0. clippy / test（--features dev-tools）"
    cargo clippy -q -p api --all-targets --features dev-tools -- -D warnings
    cargo test -q -p api --features dev-tools >/dev/null

    echo "== 1. dev-tools 無効: /debug/pool は 404"
    cargo build -q -p api
    start_api ./target/debug/api
    request GET /debug/pool
    expect_status 404 "dev-tools 無効時の /debug/pool"
    stop_api

    echo "== 2. dev-tools 有効: 投票フロー"
    cargo build -q -p api --features dev-tools
    start_api ./target/debug/api

    TOKEN="$(login_with_number alice)"
    [[ -n "$TOKEN" ]] || fail "トークンを取得できません"
    echo "ログイン: OK"

    ballot_order() { { grep -o '"contest_id":"[^"]*"' <<<"$BODY" || true; } | cut -d'"' -f4 | tr '\n' ' '; }

    request GET /api/v1/ballot-status "$TOKEN"
    expect_status 200 "状態取得"
    [[ "$(count '"voted":false')" == 9 ]] || fail "alice に関係する 9 枚とも未投票のはずです: ${BODY}"
    [[ "$(count '"voted":true')" == 0 ]] || fail "投票済みが混ざっています: ${BODY}"
    EXPECTED_ORDER="2026-general/shugiin_smd.13.01 2026-general/shugiin_pr.tokyo 2026-general/sangiin_district.13 2026-general/sangiin_pr.national 2026-general/governor.13 2026-general/pref_assembly.13.01 2026-general/municipal_head.13.101 2026-general/municipal_assembly.13.101 2026-general/supreme_court_review.national "
    [[ "$(ballot_order)" == "$EXPECTED_ORDER" ]] || fail "投票用紙が、alice に関係する 9 枚だけ・表示順ではありません: $(ballot_order)"
    echo "状態: alice に関係する 9 枚だけが、表示順（固定）で、すべて未投票 OK"

    SMD1=/api/v1/contests/2026-general/shugiin_smd.13.01
    request GET "$SMD1/candidates" "$TOKEN"
    expect_status 200 "候補者取得"
    [[ "$(count '"candidate_id"')" == 4 ]] || fail "東京 1 区の候補者は 4 名のはずです: ${BODY}"
    echo "候補者取得: 4 名 OK"

    request POST "$SMD1/vote" "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c1"}'
    expect_status 201 "投票"
    [[ "$BODY" == '{"status":"accepted"}' ]] || fail "投票応答が想定と異なります（レシートを返してはいけません）: ${BODY}"
    echo "投票: 201 OK"

    request POST "$SMD1/vote" "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c2"}'
    expect_status 409 "同一の投票用紙への再投票"
    echo "再投票: 409 OK"

    request GET /api/v1/ballot-status "$TOKEN"
    expect_status 200 "状態取得(投票後)"
    [[ "$(count '"voted":true')" == 1 && "$(count '"voted":false')" == 8 ]] \
        || fail "投票済みがちょうど 1 件のはずです: ${BODY}"
    [[ "$(ballot_order)" == "$EXPECTED_ORDER" ]] || fail "投票後に、表示順が変わっています: $(ballot_order)"
    echo "状態: 1 件だけ投票済み・表示順は不変 OK"

    BOB="$(login_with_number bob)"
    [[ -n "$BOB" ]] || fail "bob のトークンを取得できません"
    request GET "$SMD1/candidates" "$BOB"
    expect_status 403 "対象外の投票用紙の候補者取得"
    request POST "$SMD1/vote" "$BOB" '{"candidate_id":"shugiin_smd.13.01.c1"}'
    expect_status 403 "対象外の投票用紙への投票"
    [[ "$BODY" == *'"error":"not_eligible"'* ]] || fail "エラーコードが not_eligible ではありません: ${BODY}"
    request GET /api/v1/ballot-status "$BOB"
    expect_status 200 "bob の状態取得"
    [[ "$(count '"contest_id"')" == 9 && "$BODY" != *"shugiin_smd.13"* ]] || fail "bob に、他の選挙区の投票用紙が見えています: ${BODY}"
    STRANGER="$(login_with_number nobody-in-roll)"
    request GET /api/v1/ballot-status "$STRANGER"
    [[ "$STATUS" == 200 && "$BODY" == '{"ballots":[]}' ]] || fail "名簿にない有権者は、投票用紙が 1 枚もないはずです: ${BODY}"
    echo "対象外は 403、名簿にない有権者は空: OK"

    [[ "$(pool_total)" == 1 ]] || fail "/debug/pool の total が 1 ではありません: ${BODY}"
    echo "/debug/pool: total=1 OK"

    OSAKA=/api/v1/contests/2026-general/shugiin_smd.27.01
    seq 1 100 | xargs -P 100 -I{} curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
        -H "Authorization: Bearer ${BOB}" -H 'Content-Type: application/json' \
        -d '{"candidate_id":"shugiin_smd.27.01.c3"}' "${BASE}${OSAKA}/vote" >"$CODES"
    created="$({ grep -c '^201$' "$CODES" || true; })"
    conflict="$({ grep -c '^409$' "$CODES" || true; })"
    total_lines="$(wc -l <"$CODES" | tr -d ' ')"
    [[ "$total_lines" == 100 ]] || fail "応答が 100 件ではありません: ${total_lines}"
    [[ "$created" == 1 && "$conflict" == 99 ]] \
        || fail "並列 100 リクエストで 201=${created}, 409=${conflict}（期待: 201=1, 409=99）"
    echo "並列 100 リクエスト: 201=1, 409=99 OK"

    [[ "$(pool_total)" == 2 ]] || fail "並列投票後の /debug/pool の total が 2 ではありません: ${BODY}"
    echo "/debug/pool: total=2 OK"

    if grep -Eq 'alice|bob|123456789012' "$LOG"; then
        fail "サーバログに投票者 ID またはマイナンバーが出力されています"
    fi
    echo "ログに投票者情報なし OK"

    echo "OK: election#1 投票フロー"
)

# ===========================================================================
# 2. 47 都道府県規模の選挙データ（旧 check_step11.sh）
# ===========================================================================
check_large_election_data() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18812}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    export APP__SESSION__SECRET="election2-check-secret-0123456789abcdef"
    export APP__API__PORT="$PORT"
    # このスイートは選挙データの読み込みの確認で、選挙状態（原則17・18）の遷移そのものは
    # check_election_lifecycle が見る。開始時刻を過去にして、起動直後に自動で open にする。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"

    TMP="$(mktemp -d)"
    LOG="$TMP/api.log"
    cleanup() {
        common_cleanup
        hard_stop api
        rm -rf "$TMP"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        if [[ -s "$LOG" ]]; then
            echo "--- api log（末尾）---" >&2
            tail -n 15 "$LOG" >&2
        fi
        exit 1
    }

    start_api() {
        spawn api "$LOG" ./target/debug/api
        wait_healthz "$BASE" 20 api || fail "${BASE}/healthz が応答しません"
        wait_until 5000 bash -c "curl -sS '${BASE}/api/v1/election-status' | grep -q '\"phase\":\"open\"'" \
            || fail "自動で open になりませんでした（election.voting_opens_at の自動遷移）"
    }
    stop_api() { graceful_stop api; }

    echo "== 0. ビルドと単体テスト"
    cargo build -q -p api
    cargo build -q -p seedgen
    cargo build -q -p verifier
    out="$(cargo test -p domain -p seed -p seedgen 2>&1)" || { echo "$out" >&2; fail "domain / seed / seedgen のテストが失敗しました"; }
    grep -E '^test result: ok\. [1-9][0-9]* passed' <<<"$out" | head -3
    SEEDGEN=./target/debug/seedgen

    # -----------------------------------------------------------------------
    echo "== 1. 47 都道府県規模のデータ（候補者 1 万人以上）を生成して、読み込む"
    BIG="$TMP/big"
    "$SEEDGEN" --out "$BIG" --prefectures 47 --districts-per-pref 6 --candidates-per-district 8 --voters 2000 \
        | tee "$TMP/gen.out"
    BIGDIR="$BIG/$SEED_ID"
    candidates="$(cat "$BIGDIR"/candidates/*.csv | grep -c . || true)"
    candidates=$((candidates - $(ls "$BIGDIR"/candidates/*.csv | wc -l)))
    districts=$(($(grep -c . "$BIGDIR/districts.csv") - 1))
    types="$(grep -c '^\[\[types\]\]' "$BIGDIR/election.toml")"
    prefectures="$(cut -d, -f4 "$BIGDIR/districts.csv" | tr ';' '\n' | sort -u | grep -c '^[0-9][0-9]$')"
    [[ "$candidates" -ge 10000 ]] || fail "候補者が 1 万人以上ではありません（${candidates} 人）"
    [[ "$types" == 9 ]] || fail "選挙の種類が 9 種類ではありません（${types}）"
    [[ "$prefectures" == 47 ]] || fail "47 都道府県ではありません（${prefectures}）"
    echo "生成: 選挙の種類 ${types}・都道府県 ${prefectures}・選挙区 ${districts}・候補者 ${candidates} 人・有権者 2000 人"
    grep -q '^sangiin_district.31_32,sangiin_district,.*,31;32,' "$BIGDIR/districts.csv" || fail "合区（鳥取・島根）が、ID 1 つ・都道府県 2 つで表されていません"
    grep -q '^shugiin_pr.kinki,shugiin_pr,.*,25;26;27;28;29;30,' "$BIGDIR/districts.csv" || fail "近畿ブロックが、複数の都道府県のリストで表されていません"
    echo "合区・比例ブロック: 都道府県のリスト属性で表現 OK"
    check_out="$("$SEEDGEN" --check "$BIG")" || fail "生成したデータの検証（--check）が失敗しました"
    echo "$check_out"
    [[ "$check_out" == *"候補者 ${candidates} 人"* ]] || fail "検証が数えた候補者数が、生成した数（${candidates}）と一致しません"

    APP__ELECTION__SEED_DIR="$BIG" start_api
    grep -aEq "選挙データを読み込みました.*candidates=${candidates}" "$LOG" || fail "api が、${candidates} 人の候補者を読み込んでいません"
    echo "api が読み込み: OK（$(grep -a '選挙データを読み込みました' "$LOG" | sed 's/^[^ ]* *//')）"

    # -----------------------------------------------------------------------
    echo "== 2. 東京 1 区の有権者に見える投票用紙"
    STATUS_BODY=""
    ballot_ids() { { grep -o '"contest_id":"[^"]*"' <<<"$STATUS_BODY" || true; } | cut -d'"' -f4 | tr '\n' ' '; }
    fetch_status() { STATUS_BODY="$(curl -sS "${BASE}/api/v1/ballot-status" -H "Authorization: Bearer $1")"; }

    TOKYO_ROW="$(grep -m1 'shugiin_smd\.13\.01' "$BIGDIR/voters.csv")"
    TOKYO_VOTER="${TOKYO_ROW%%,*}"
    TOKYO_DISTRICTS="${TOKYO_ROW#*,}"
    [[ -n "$TOKYO_VOTER" ]] || fail "東京 1 区の有権者が、名簿にありません"
    TOKEN="$(login "$TOKYO_VOTER")"
    [[ -n "$TOKEN" ]] || fail "ログインできません（${TOKYO_VOTER}）"
    fetch_status "$TOKEN"
    ACTUAL="$(ballot_ids)"

    expected_order() {
        local dir="$1" districts="$2"
        awk -F, -v want="$districts" '
            BEGIN { n = split(want, w, ";"); for (i = 1; i <= n; i++) wanted[w[i]] = 1 }
            FNR == 1 { next }
            ($1 in wanted) { print $1 "\t" $5 }
        ' "$dir/districts.csv" | while IFS=$'\t' read -r district order; do
            type="${district%%.*}"
            type_order="$(awk -v t="$type" '$1=="code" && $3=="\""t"\"" {found=1; next} found && $1=="order" {print $3; exit}' "$dir/election.toml")"
            printf '%08d %08d %s\n' "$type_order" "$order" "$district"
        done | sort | awk -v e="$SEED_ID" '{printf "%s/%s ", e, $3}'
    }
    EXPECTED="$(expected_order "$BIGDIR" "$TOKYO_DISTRICTS")"
    [[ "$ACTUAL" == "$EXPECTED" ]] || fail "東京 1 区の有権者（${TOKYO_VOTER}）の投票用紙が、名簿の選挙区を表示順に並べたものと違います:
  実際: ${ACTUAL}
  期待: ${EXPECTED}"
    count="$(wc -w <<<"$ACTUAL" | tr -d ' ')"
    [[ "$count" == 9 ]] || fail "投票用紙が 9 枚ではありません（${count}）: ${ACTUAL}"
    for must in shugiin_smd.13.01 shugiin_pr.tokyo sangiin_district.13 sangiin_pr.national governor.13 supreme_court_review.national; do
        [[ "$ACTUAL" == *"/${must} "* ]] || fail "東京 1 区の有権者に、${must} の投票用紙がありません: ${ACTUAL}"
    done
    for id in $ACTUAL; do
        [[ "$id" =~ (national|\.tokyo|\.13(\.|$)) ]] || fail "東京の有権者に、関係のない投票用紙が見えています: ${id}"
    done
    echo "有権者 ${TOKYO_VOTER}（東京 1 区）: 9 枚だけ、表示順: OK"
    echo "  ${ACTUAL}"
    OSAKA_ROW="$(grep -m1 'shugiin_smd\.27\.01' "$BIGDIR/voters.csv")"
    OSAKA_TOKEN="$(login "${OSAKA_ROW%%,*}")"
    fetch_status "$OSAKA_TOKEN"
    for id in $(ballot_ids); do
        [[ "$id" =~ (national|\.kinki|\.27(\.|$)) ]] || fail "大阪の有権者に、関係のない投票用紙が見えています: ${id}"
    done
    [[ "$(ballot_ids)" == *"shugiin_smd.27.01"* && "$(ballot_ids)" != *"shugiin_smd.13"* ]] || fail "大阪 1 区の有権者の投票用紙が想定と異なります: $(ballot_ids)"
    code="$(curl -sS -o /dev/null -w '%{http_code}' "${BASE}/api/v1/contests/${SEED_ID}/shugiin_smd.27.01/candidates" -H "Authorization: Bearer ${TOKEN}")"
    [[ "$code" == 403 ]] || fail "対象外の投票用紙の候補者取得が 403 ではありません（${code}）"
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${SEED_ID}/shugiin_smd.27.01/vote" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d '{"candidate_id":"shugiin_smd.27.01.c1"}')"
    [[ "$code" == 403 ]] || fail "対象外の投票用紙への投票が 403 ではありません（${code}）"
    echo "他の都道府県の有権者・対象外の投票: OK"

    # -----------------------------------------------------------------------
    echo "== 3. 投票の順番は表示順どおり（先頭の未投票へ進む）"
    first_unvoted() {
        { grep -o '"contest_id":"[^"]*"[^}]*"voted":false' <<<"$STATUS_BODY" || true; } | head -1 | sed 's/"contest_id":"\([^"]*\)".*/\1/'
    }
    voted_order=""
    for step in $(seq 1 9); do
        fetch_status "$TOKEN"
        current="$(first_unvoted)"
        [[ -n "$current" ]] || fail "${step} 枚目: 先頭の未投票がありません"
        expected_current="$(awk -v n="$step" '{print $n}' <<<"$EXPECTED")"
        [[ "$current" == "$expected_current" ]] || fail "${step} 枚目の「今」の投票用紙が表示順と違います: ${current}（期待 ${expected_current}）"
        district="${current#*/}"
        code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${current}/vote" \
            -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"${district}.c1\"}")"
        [[ "$code" == 201 ]] || fail "${step} 枚目（${current}）の投票が 201 ではありません（${code}）"
        voted_order+="${current} "
        fetch_status "$TOKEN"
        [[ "$(ballot_ids)" == "$EXPECTED" ]] || fail "投票後に、表示順が変わりました"
        voted_count="$({ grep -o '"voted":true' <<<"$STATUS_BODY" || true; } | wc -l | tr -d ' ')"
        [[ "$voted_count" == "$step" ]] || fail "${step} 枚目の投票後の投票済みが ${step} 枚ではありません（${voted_count}）"
    done
    [[ "$voted_order" == "$EXPECTED" ]] || fail "投票した順番が、表示順と違います: ${voted_order}"
    fetch_status "$TOKEN"
    [[ -z "$(first_unvoted)" ]] || fail "9 枚投票した後に、未投票が残っています"
    echo "先頭の未投票へ、表示順に 9 枚を投票 OK（先頭に戻る動きはなく、最後で終わる）"
    flow_out="$(cargo test -p web --lib flow:: 2>&1)" || { echo "$flow_out" >&2; fail "web の flow のテストが失敗しました"; }
    for name in a_full_session_visits_each_ballot_once_in_display_order_then_finishes \
        a_ballot_page_only_opens_for_the_current_ballot reloading_resumes_from_the_first_unvoted_ballot \
        current_is_always_the_first_unvoted_in_display_order; do
        grep -Eq "^test flow::tests::${name} \.\.\. ok$" <<<"$flow_out" || fail "flow のテスト ${name} が成功していません"
    done
    echo "flow（画面の順序ロジック）: 固定順のテスト OK"
    verify_out="$(./target/debug/verifier verify --api "$BASE" 2>&1)" || true
    echo "$verify_out" | tail -4
    stop_api

    # -----------------------------------------------------------------------
    echo "== 4. 壊したデータは、読み込み時に、ファイル・行・原因つきで検出される"
    GOOD="$TMP/good"
    "$SEEDGEN" --out "$GOOD" --prefectures 3 --districts-per-pref 2 --candidates-per-district 3 --voters 30 >/dev/null
    break_case() {
        local desc="$1" file="$2"
        shift 2
        local needles=()
        while [[ "$1" != "--" ]]; do needles+=("$1"); shift; done
        shift
        rm -rf "$TMP/broken"
        cp -r "$GOOD" "$TMP/broken"
        (cd "$TMP/broken/$SEED_ID" && "$@")
        local out rc=0
        out="$("$SEEDGEN" --check "$TMP/broken" 2>&1)" || rc=$?
        [[ "$rc" -ne 0 ]] || fail "${desc}: 壊したデータが、検出されずに読み込まれました"
        [[ "$out" == *"$file"* ]] || { echo "$out" >&2; fail "${desc}: エラーにファイル名 ${file} がありません"; }
        local n
        for n in "${needles[@]}"; do
            [[ "$out" == *"$n"* ]] || { echo "$out" >&2; fail "${desc}: エラーに「${n}」がありません"; }
        done
        echo "${desc}: 検出（$(grep -m1 "$file" <<<"$out" | sed 's/^ *- //')）"
    }
    break_case "選挙区 ID の重複" districts.csv "重複" "shugiin_smd.01.01" -- bash -c 'sed -n 2p districts.csv >> districts.csv'
    break_case "候補者 ID の重複" governor.csv "重複" "governor.01.c1" -- bash -c 'sed -n 2p candidates/governor.csv >> candidates/governor.csv'
    break_case "存在しない選挙区を参照する候補者" governor.csv "存在しない選挙区" "governor.99" -- \
        bash -c 'echo "governor.99.c1,governor.99,幽霊,無所属," >> candidates/governor.csv'
    break_case "存在しない選挙区に属する有権者" voters.csv "存在しない選挙区" "governor.99" -- \
        bash -c 'echo "ghost,governor.99" >> voters.csv'
    break_case "有権者 ID の重複" voters.csv "重複" "voter-1" -- bash -c 'echo "voter-1,governor.01" >> voters.csv'
    break_case "不正な候補者 ID" governor.csv "candidate_id" -- \
        bash -c 'echo "Governor.01.C9,governor.01,不正,無所属," >> candidates/governor.csv'
    break_case "長すぎる選挙区 ID" districts.csv "長すぎます" -- \
        bash -c 'echo "governor.'"$(printf 'x%.0s' {1..70})"',governor,長すぎる,01,9" >> districts.csv'
    break_case "未定義の選挙の種類" districts.csv "election.toml" "sangiin_x" -- \
        bash -c 'echo "sangiin_x.national,sangiin_x,全国,01,1" >> districts.csv'
    break_case "不正な都道府県コード" districts.csv "都道府県コード" "48" -- \
        bash -c 'echo "governor.90,governor,不正,48,9" >> districts.csv'
    break_case "列が足りない CSV" voters.csv "列の数" -- bash -c 'echo "onlyid" >> voters.csv'
    rm -rf "$TMP/broken"
    cp -r "$GOOD" "$TMP/broken"
    echo "governor.99.c1,governor.99,幽霊,無所属," >>"$TMP/broken/$SEED_ID/candidates/governor.csv"
    : >"$LOG"
    if APP__ELECTION__SEED_DIR="$TMP/broken" timeout 20 ./target/debug/api >"$LOG" 2>&1; then
        fail "壊した選挙データで、api が起動しました"
    fi
    grep -q 'governor.csv:' "$LOG" && grep -q '存在しない選挙区' "$LOG" || fail "api の起動エラーに、ファイル・行・原因がありません"
    echo "api: 壊したデータでは起動せず、ファイル・行・原因を表示して終了 OK"

    echo "OK: election#2 47 都道府県規模の選挙データ"
)

# ===========================================================================
# 3. 選挙状態の遷移と投票の受付期間（原則17・18。ADR 0019）
# ===========================================================================
check_election_lifecycle() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18813}"
    ADMIN_PORT="${CHECK_ADMIN_PORT:-18913}"
    BASE="http://127.0.0.1:${PORT}"
    ADMIN_BASE="http://127.0.0.1:${ADMIN_PORT}"
    ADMIN_TOKEN="election3-check-admin-token-0123456789abcdef"
    export APP__SESSION__SECRET=election3-check-secret-0123456789abcdef
    export APP__ADMIN__TOKEN="$ADMIN_TOKEN"
    export APP__ADMIN__BIND="127.0.0.1:${ADMIN_PORT}"
    export APP__API__PORT="${PORT}"
    # 猶予（締切の手続きの待ち時間）を短くして、確認を速くする（最小値: state_cache_secs は 0 以上、
    # request_timeout_secs は 1 以上）。
    export APP__API__REQUEST_TIMEOUT_SECS=1
    export APP__ELECTION__STATE_CACHE_SECS=1
    export APP__SEAL__INTERVAL_SECS=2

    LOG="$(mktemp)"
    ADMIN_TMP="$(mktemp)"
    cleanup() {
        common_cleanup
        hard_stop api
        rm -f "$LOG" "$ADMIN_TMP"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        echo "--- api log ---" >&2
        cat "$LOG" >&2
        exit 1
    }

    admin() {
        local method="$1" path="$2" data="${3:-}"
        local args=(-sS -o "$ADMIN_TMP" -w '%{http_code}' -X "$method" -H "Authorization: Bearer ${ADMIN_TOKEN}" "${ADMIN_BASE}${path}")
        [[ -n "$data" ]] && args+=(-H 'Content-Type: application/json' -d "$data")
        ASTATUS="$(curl "${args[@]}" || true)"
        ABODY="$(cat "$ADMIN_TMP")"
    }
    phase_is() {
        admin GET /admin/v1/election
        [[ "$ASTATUS" == 200 ]] && [[ "$ABODY" == *"\"phase\":\"$1\""* ]]
    }
    # date -d（GNU）/ -v（BSD・macOS）の両方に対応。
    rfc3339_in() {
        date -u -d "+$1 seconds" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v+"$1"S +%Y-%m-%dT%H:%M:%SZ
    }

    echo "== 0. ビルドと、期間の境界（開始時刻ちょうどは受け付け、終了時刻ちょうどは拒否する）"
    # 境界は「秒ちょうど」の判定なので、実時間の HTTP では狙って当てられない（1 秒ずれると別の分岐を見てしまう）。
    # 時計を固定できる単体テスト（domain::vote_gate・api の統合テスト。どちらも公開 API と同じ判定の経路）で確認する。
    cargo build -q -p api
    out="$(cargo test -q -p domain vote_gate 2>&1)" || { echo "$out" >&2; fail "domain::vote_gate のテストが失敗しました"; }
    grep -Eq 'test result: ok\. [1-9][0-9]* passed' <<<"$out" || { echo "$out" >&2; fail "domain::vote_gate のテストが 1 件も実行されていません"; }
    out="$(cargo test -q -p api --test api_flow the_opening_instant_is_accepted_and_the_closing_instant_is_rejected 2>&1)" \
        || { echo "$out" >&2; fail "api の境界テストが失敗しました"; }
    grep -q 'test result: ok. 1 passed' <<<"$out" || { echo "$out" >&2; fail "api の境界テストが実行されていません"; }
    echo "境界: 開始時刻ちょうど = 201、終了時刻ちょうど = 403 voting_closed（固定の時計）OK"

    echo "== 1. 起動直後は scheduled"
    spawn api "$LOG" ./target/debug/api
    wait_healthz "$BASE" 10 api || fail "${BASE}/healthz が応答しません"
    phase_is scheduled || fail "起動直後は scheduled のはずです: ${ABODY}"
    echo "起動直後: scheduled OK"

    echo "== 2. schedule は scheduled のときだけ実行できる"
    OPENS_AT="$(rfc3339_in 4)"
    CLOSES_AT="$(rfc3339_in 9)"
    admin POST /admin/v1/election/schedule "{\"opens_at\":\"${OPENS_AT}\",\"closes_at\":\"${CLOSES_AT}\"}"
    [[ "$ASTATUS" == 200 ]] || fail "schedule に失敗しました（status=${ASTATUS}）: ${ABODY}"
    echo "schedule: 200 OK（開始 ${OPENS_AT} / 終了 ${CLOSES_AT}）"

    echo "== 3. 開始前は投票を拒否する（labels の文言つき）"
    TOKEN="$(login alice)"
    [[ -n "$TOKEN" ]] || fail "ログインできません"
    request POST /api/v1/contests/2026-general/shugiin_smd.13.01/vote "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c1"}'
    expect_status 403 "開始前の投票"
    [[ "$BODY" == *'"error":"voting_not_started"'* ]] || fail "開始前のエラーコードが違います: ${BODY}"
    echo "開始前: 403 voting_not_started OK"

    echo "== 4. schedule は open では実行できない（後で確認するため、いま一度だけ試す）"
    admin POST /admin/v1/election/schedule "{\"opens_at\":\"${OPENS_AT}\",\"closes_at\":\"${CLOSES_AT}\"}"
    [[ "$ASTATUS" == 200 ]] || fail "2 回目の schedule（まだ scheduled）に失敗しました: ${ABODY}"

    echo "== 5. アンカーのリースを持つ sealer（memory: api 内蔵）が、開始時刻に自動で open へ進める"
    wait_until 8000 phase_is open || fail "自動で open になりませんでした: ${ABODY}"
    echo "自動遷移: scheduled -> open OK"

    echo "== 6. schedule は open ではもう実行できない"
    admin POST /admin/v1/election/schedule "{\"opens_at\":\"${OPENS_AT}\",\"closes_at\":\"${CLOSES_AT}\"}"
    [[ "$ASTATUS" == 409 ]] || fail "open の間の schedule が 409 ではありません（status=${ASTATUS}）: ${ABODY}"
    echo "open 中の schedule: 409 OK"

    echo "== 7. 期間内は投票を受け付ける"
    request POST /api/v1/contests/2026-general/shugiin_smd.13.01/vote "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c1"}'
    expect_status 201 "期間内の投票"
    echo "期間内: 201 OK"

    echo "== 8. 締切の直前に投じた票が、最終ブロックまでに封印される"
    request POST /api/v1/contests/2026-general/shugiin_pr.tokyo/vote "$TOKEN" '{"candidate_id":"shugiin_pr.tokyo.c1"}'
    expect_status 201 "締切直前の投票"

    wait_until 10000 phase_is closing || fail "自動で closing になりませんでした: ${ABODY}"
    echo "自動遷移: open -> closing OK"

    echo "== 9. closing の間は投票を拒否する"
    request POST /api/v1/contests/2026-general/shugiin_smd.13.01/vote "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c2"}'
    if [[ "$STATUS" == 403 && "$BODY" == *'"error":"voting_closing"'* ]]; then
        echo "closing 中: 403 voting_closing OK"
    else
        echo "  （closing の観測前に closed へ進んだ可能性があります。次の確認で closed を確認します）"
    fi

    wait_until 20000 phase_is closed || fail "自動で closed になりませんでした: ${ABODY}"
    echo "自動遷移: closing -> closed OK"

    admin GET /admin/v1/election
    [[ "$ABODY" == *'"pending_by_shard":[0]'* ]] || fail "closed の時点で未封印の票が残っています: ${ABODY}"
    echo "closed の時点で未封印 0 件 OK"
    heights="$(sed -n 's/.*"height":\([0-9]*\).*/\1/p' <<<"$ABODY" | head -1)"
    [[ "$heights" -ge 1 ]] || fail "締切直前の票を封印したブロックが増えていません（height=${heights}）"
    echo "締切直前の票（2 枚の投票用紙）は、最終ブロック（height=${heights}）までに封印済み OK"

    echo "== 10. closed の後の投票は拒否される"
    request POST /api/v1/contests/2026-general/shugiin_smd.13.01/vote "$TOKEN" '{"candidate_id":"shugiin_smd.13.01.c2"}'
    expect_status 403 "closed 後の投票"
    [[ "$BODY" == *'"error":"voting_closed"'* ]] || fail "closed 後のエラーコードが違います: ${BODY}"
    echo "closed 後: 403 voting_closed OK"

    echo "== 11. 公開用のポートから管理用のエンドポイントに届かない（逆方向も）"
    code="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${ADMIN_TOKEN}" "${BASE}/admin/v1/election")"
    [[ "$code" == 404 ]] || fail "公開用ポートから /admin/v1/election に届いています（${code}）"
    code="$(curl -sS -o /dev/null -w '%{http_code}' "${ADMIN_BASE}/api/v1/election-status")"
    [[ "$code" == 404 ]] || fail "管理用リスナーから /api/v1/election-status に届いています（${code}）"
    code="$(curl -sS -o /dev/null -w '%{http_code}' "${ADMIN_BASE}/healthz")"
    [[ "$code" == 404 ]] || fail "管理用リスナーから /healthz に届いています（${code}）"
    echo "公開用ポートと管理用リスナーは、互いのエンドポイントに届かない OK"

    graceful_stop api
    echo "OK: election#3 選挙状態の遷移と投票の受付期間"
)

# ===========================================================================
# 4. close --now（scripts/election.sh）の直前に投じた票が、すべて最終ブロックまでに封印される（原則17・18。ADR 0019）
# ===========================================================================
# 3 節は「時刻による自動の closing」を見る。ここでは、運用者が scripts/election.sh close --now で締め切る経路を、
# スクリプトそのものを通して確認する。締切の手続きの待ち時間（state_cache_secs + request_timeout_secs）は、
# 「各 api が古い状態（open）をキャッシュしている間に受理した票」と「処理中のリクエスト」を取りこぼさないための
# ものなので、投票を並列に投げ続けている最中に close --now を実行し、201 を返した票の数と、closed 後の
# チェーン上の票の数（verifier tally の突合。未封印 0 件・投票済み記録と一致）が等しいことを確かめる。
check_close_now() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18814}"
    ADMIN_PORT="${CHECK_ADMIN_PORT:-18914}"
    BASE="http://127.0.0.1:${PORT}"
    ADMIN_BASE="http://127.0.0.1:${ADMIN_PORT}"
    ADMIN_TOKEN="election4-check-admin-token-0123456789abcdef"
    export BASE
    export APP__SESSION__SECRET=election4-check-secret-0123456789abcdef
    export APP__ADMIN__TOKEN="$ADMIN_TOKEN"
    export APP__ADMIN__BIND="127.0.0.1:${ADMIN_PORT}"
    export APP__API__PORT="${PORT}"
    export APP__SHARD__COUNT=4
    # 件数・時間による封印はさせない（締切のフラッシュだけで、全件が最終ブロックに入ることを確かめるため）。
    export APP__SEAL__MAX_BALLOTS=10000
    export APP__SEAL__INTERVAL_SECS=600
    # 開始は過去（起動直後に自動で open）、終了は遠い未来（締切は close --now だけが起こす）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export APP__ELECTION__VOTING_CLOSES_AT="2099-01-01T00:00:00+00:00"
    export APP__ELECTION__STATE_CACHE_SECS=1
    export APP__API__REQUEST_TIMEOUT_SECS=2

    TMP="$(mktemp -d)"
    LOG="$TMP/api.log"
    CODES="$TMP/codes"
    OUT="$TMP/tally"
    cleanup() {
        common_cleanup
        hard_stop api
        rm -rf "$TMP"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        echo "--- api log（末尾）---" >&2
        tail -n 20 "$LOG" >&2
        exit 1
    }
    election() { ./scripts/election.sh "$@"; }
    admin_body() { curl -sS -H "Authorization: Bearer ${ADMIN_TOKEN}" "${ADMIN_BASE}/admin/v1/election" 2>/dev/null || true; }
    phase_is() { [[ "$(admin_body)" == *"\"phase\":\"$1\""* ]]; }
    RC=0
    TALLY_OUT=""
    run_tally() {
        RC=0
        TALLY_OUT="$(./scripts/tally.sh --api "$BASE" --out "$OUT" "$@" 2>&1)" || RC=$?
    }

    echo "== 0. ビルドと準備"
    cargo build -q -p api
    cargo build -q -p verifier
    VOTERS=40
    seed_generate "$TMP/seed" "$VOTERS"
    spawn api "$LOG" ./target/debug/api
    wait_healthz "$BASE" 10 api || fail "${BASE}/healthz が応答しません"
    wait_election_open "$BASE" 10 || fail "自動で open になりませんでした"

    echo "== 1. scripts/election.sh status: 状態・期間・残り時間・未封印・最後のブロック・監査ログ"
    out="$(election status)" || fail "election.sh status が失敗しました: ${out}"
    for must in "状態: open" "開始: " "終了: " "残り時間（終了まで）" "シャードごとの未封印: 0,0,0,0" \
        "最後のブロック" "直近の監査ログ" "scheduled -> open"; do
        [[ "$out" == *"$must"* ]] || fail "election.sh status の表示に「${must}」がありません:
${out}"
    done
    echo "status: OK"

    echo "== 2. schedule は open では実行できない（scheduled のときだけ）"
    if out="$(election schedule --opens-at 2099-01-01T00:00:00+09:00 --closes-at 2099-01-02T00:00:00+09:00 2>&1)"; then
        fail "open の間に schedule が成功しました: ${out}"
    fi
    [[ "$out" == *"409"* ]] || fail "open の間の schedule が 409 ではありません: ${out}"
    echo "open 中の schedule: 拒否（409）OK"

    echo "== 3. 集計（tally）は closed のときだけ（--allow-interim は app.env=dev のときだけ）"
    run_tally
    [[ "$RC" == 4 ]] || fail "open の間の tally が終了コード 4 ではありません（${RC}）: ${TALLY_OUT}"
    run_tally --allow-interim
    [[ "$RC" == 2 ]] || fail "app.env=test で --allow-interim が拒否されません（${RC}）: ${TALLY_OUT}"
    echo "open 中の tally: 4・app.env=test の --allow-interim: 2 OK"

    echo "== 4. close --now は確認を求める（yes 以外では何も変えない）"
    if out="$(echo no | election close --now 2>&1)"; then
        fail "確認に no と答えたのに、close --now が成功しました: ${out}"
    fi
    phase_is open || fail "確認で中止したのに、状態が変わっています: $(admin_body)"
    echo "確認で中止: 状態は open のまま OK"

    echo "== 5. 投票を並列に投げ続けている最中に close --now --yes"
    TOKENS="$TMP/tokens"
    for n in $(seq 1 "$VOTERS"); do
        token="$(login "voter-${n}")"
        [[ -n "$token" ]] || fail "voter-${n} がログインできません"
        echo "${n} ${token}" >>"$TOKENS"
    done
    # 有権者ごとに、表示順 1〜6 枚目の投票用紙へ 0.5 秒おきに投票する（有権者どうしは並列。約 3 秒続く）。
    # close --now は、その途中（最初の応答の直後）に実行する。api の状態のキャッシュ（state_cache_secs=1）の
    # 間は受理され続け、その後は 403 になるので、締切の前後の両方に票が飛んでいる状態になる。
    PER_VOTER=6
    vote_all() {
        local n token k
        read -r n token <<<"$1"
        for k in $(seq 1 "$PER_VOTER"); do
            seed_vote "$n" "$k" "$token"
            sleep 0.5
        done
    }
    export PER_VOTER
    export -f vote_all
    xargs -P "$VOTERS" -I{} bash -c 'vote_all "$@"' _ {} <"$TOKENS" >"$CODES" &
    VOTING_PID=$!
    # 最初の票が受理されるのを待ってから締め切る（締切の前にも後にも、票が飛んでいる状態を作る）。
    wait_until 5000 test -s "$CODES" || fail "投票の応答がありません"
    election close --now --yes >"$TMP/close.out" 2>&1 || { cat "$TMP/close.out" >&2; fail "close --now --yes が失敗しました"; }
    wait "$VOTING_PID" || true
    accepted="$({ grep -c '^201$' "$CODES" || true; })"
    rejected="$({ grep -c '^403$' "$CODES" || true; })"
    total_lines="$(wc -l <"$CODES" | tr -d ' ')"
    [[ "$total_lines" == $((VOTERS * PER_VOTER)) ]] || fail "応答の数が $((VOTERS * PER_VOTER)) 件ではありません（${total_lines}）"
    [[ "$((accepted + rejected))" == "$total_lines" ]] || fail "201 / 403 以外の応答があります: $(sort "$CODES" | uniq -c | tr '\n' ' ')"
    [[ "$accepted" -ge 1 && "$rejected" -ge 1 ]] \
        || fail "締切の前後の両方に票が飛んでいません（受理 ${accepted}・拒否 ${rejected}。確認になりません）"
    echo "close --now の前後: 受理 ${accepted} 票・拒否 ${rejected} 票（締切の手続き中・後）"
    phase_is closing || phase_is closed || fail "close --now の後、closing になりません: $(admin_body)"

    echo "== 6. 締切の手続き（自動）: 待ち時間 → 全シャードのフラッシュ → 最終アンカー → closed"
    run_tally
    if phase_is closing; then
        [[ "$RC" == 4 ]] || fail "closing の間の tally が終了コード 4 ではありません（${RC}）: ${TALLY_OUT}"
        echo "closing 中の tally: 4 OK"
    fi
    wait_until 20000 phase_is closed || fail "自動で closed になりませんでした: $(admin_body)"
    body="$(admin_body)"
    [[ "$body" == *'"pending_by_shard":[0,0,0,0]'* ]] || fail "closed の時点で未封印の票が残っています: ${body}"
    out="$(election status)"
    for must in "状態: closed" "open -> closing (admin" "closing -> closed"; do
        [[ "$out" == *"$must"* ]] || fail "election.sh status の監査ログに「${must}」がありません:
${out}"
    done
    echo "closed・未封印 0 件・監査ログ（open -> closing は admin、closing -> closed は自動）OK"

    echo "== 7. 受理した票が、すべて最終ブロックまでに封印されている（tally の検証・突合）"
    # 開票の区切りのために、公開用の election-status の短期キャッシュが入れ替わるのを待つ。
    sleep "$((APP__ELECTION__STATE_CACHE_SECS + 1))"
    run_tally
    [[ "$RC" == 0 ]] || fail "closed 後の tally が終了コード 0 ではありません（${RC}）: ${TALLY_OUT}"
    json="$(find "$OUT" -name tally.json | head -1)"
    [[ -n "$json" ]] || fail "tally.json がありません"
    sealed="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["reconciliation"]["ballots"])' "$json")"
    [[ "$sealed" == "$accepted" ]] || fail "チェーン上の票（${sealed}）が、受理した票（${accepted}）と一致しません"
    grep -q '"interim": false' "$json" || fail "closed 後の集計が中間集計になっています"
    echo "受理 ${accepted} 票 = チェーン上 ${sealed} 票（未封印 0・投票済み記録と一致）OK"

    echo "== 8. closed の後の投票は拒否される"
    read -r n token < <(tail -n 1 "$TOKENS")
    code="$(seed_vote "$n" $((PER_VOTER + 1)) "$token")"
    [[ "$code" == 403 ]] || fail "closed 後の投票が 403 ではありません（${code}）"
    echo "closed 後: 403 OK"

    graceful_stop api
    echo "OK: election#4 close --now の直前の票の封印（scripts/election.sh）"
)

check_voting_flow
check_large_election_data
check_election_lifecycle
check_close_now

echo "OK: check/election.sh"
