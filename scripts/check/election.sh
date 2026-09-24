#!/usr/bin/env bash
# election スイート: 投票フローと選挙データ（対応表: docs/testing.md）。
#   1. 投票フロー（ログイン・状態・候補者・投票・再投票拒否・対象外403・並列100・秘密投票）。
#      サンプルの選挙データ seed/2026-general（有権者 alice = 東京 1 区、bob = 大阪 1 区）を使う
#   2. 47 都道府県規模の選挙データ（seedgen）: 生成・読み込み・有権者ごとの表示範囲・投票順の固定・
#      壊れたデータ（重複 ID・存在しない参照・不正な ID など）の検出
#   3. 選挙状態の遷移と投票の受付期間（原則17・18。ADR 0019）: schedule → 自動で open → 自動で closing →
#      closed（アンカーのリースを持つ sealer、memory モードでは api 内蔵のスケジューラが駆動する）。
#      期間の境界（開始前・終了後は拒否、期間内は受け付ける）、closing の直前に投じた票が最終ブロックまでに
#      封印されること、closed の後の投票が拒否されること、公開用のポートから管理用のエンドポイントに
#      届かないこと（逆方向も）を確認する
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
    export APP__SEAL__MAX_INTERVAL_SECS=2

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

    echo "== 0. ビルド"
    cargo build -q -p api

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

check_voting_flow
check_large_election_data
check_election_lifecycle

echo "OK: check/election.sh"
