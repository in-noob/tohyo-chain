#!/usr/bin/env bash
# auth スイート: ID・パスワードの事前登録（credgen）と、DB 認証（auth.mode=db）と、DB のリセット
# （scripts/db_reset.sh）と、確認用のサンプルデータ（scripts/sample_data.sh）（対応表: docs/testing.md）。
# auth#1
#   1. credgen で 100 人分を登録 → CSV の件数・列・権限（0600）・文字（0/O/1/I/l なし）を確認。DB にはハッシュ（Argon2id）だけ
#   2. 正しい ID とパスワードでログインでき、ログイン ID とは別の内部 voter_id で、DB の名簿の投票用紙に投票できる
#   3. パスワード違い・存在しない ID・形式が不正な ID が、同じステータス・同じメッセージで失敗する（応答時間も同程度）
#   4. output_file_enabled=false で警告が出て、--confirm-no-output がなければ何も処理しない。2 回目の実行はスキップ、
#      --reissue で再発行（古いパスワードは使えなくなり、投票済みの記録は保たれる）
#   5. 投票してから db_reset --votes --yes: 投票データが 0 件になり、認証情報は残り、同じ ID でまたログインして投票できる
#      （api / sealer が動いている間は拒否。確認なしでは何も消えない）
#   6. db_reset --all --yes の後は、ログインできない
#   7. app.env=production では、db_reset が拒否される。memory モードでは、再起動でリセットされると表示して終了する
# auth#2（scripts/sample_data.sh。ADR 0023）
#   0. app.env=production では拒否、memory モードでは db モードでの実行を案内して終了、不正な --phase は終了コード 2
#   1. --phase before / open / closed のそれぞれで実行する（選挙のルールも phase ごとに変える）。CSV の権限 0600・ヘッダ・
#      14 行（P13 は 2 行）・作成不可の行（before は P05〜P09、closed は allow_blank=false の P07）。要約表にパスワードを出さない
#   2. api・sealer を起動し直して、CSV の各行で、実際にログイン → 再投票 → 投票 を試し、期待結果の列と一致することを確認する
#   3. open: api が動いている間は、DB も CSV も変えずに拒否する。closed: verifier verify が OK・revote_key は破棄済み
# 専用のキースペースを使う。共用の vote には触れない。
# 環境変数（APP__DB__BACKEND / DB_PORT / KEEP_KEYSPACE / STOP_DB）は scripts/lib/common.sh を参照。
set -euo pipefail

# --help: 先頭のコメント（使い方）を表示して終わる（何も起動・変更しない。scripts/check/docs.sh#8 が確認する）。
[[ "${1:-}" == -h || "${1:-}" == --help ]] && { sed -n '2,/^[^#]/{/^#/s/^# \{0,1\}//p}' "$0"; exit 0; }

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."
source scripts/lib/common.sh

check_credgen_and_db_auth() (
    set -euo pipefail
    db_setup_vars
    ks_init auth1

    PORT="${CHECK_API_PORT:-18821}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE

    TMP="$(mktemp -d)"
    API_LOG="$TMP/api.log"
    SEALER_LOG="$TMP/sealer.log"

    export APP__APP__MODE=db
    export APP__AUTH__MODE=db
    export APP__DB__NODES="127.0.0.1:${DB_PORT}"
    export APP__SHARD__COUNT=1
    # 少ない票（6 票）を時間で封印させて、db_reset の前に「ブロックがある」状態を作るため、最小件数を 1 にする。
    export APP__SEAL__INTERVAL_SECS=10
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL=1
    export APP__SEALER__LEASE_TTL_SECS=6
    export APP__SEALER__SIGNING_SEED="0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c"
    export APP__SESSION__SECRET="auth1-check-secret-0123456789abcdef"
    export APP__API__PORT="$PORT"
    # Argon2 は、確認を速くするため、小さめのパラメータ（本番の既定は memory_kib=19456）。
    export APP__AUTH__ARGON2__MEMORY_KIB=4096
    export APP__AUTH__ARGON2__ITERATIONS=2
    export APP__CREDENTIALS__PASSWORD_LENGTH=12
    export APP__CREDENTIALS__LOGIN_ID_LENGTH=10
    export APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true
    export APP__CREDENTIALS__OUTPUT_PATH="$TMP/credentials.csv"
    # 開始時刻を過去にして、アンカーのリースを持つ sealer が起動直後に自動で open にする（原則17・18。
    # このスイートは認証を見るので、投票の受付期間そのものは確認しない）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,sealer=debug,tower_http=warn"

    cleanup() {
        common_cleanup
        hard_stop api
        hard_stop sealer
        rm -rf "$TMP"
        ks_drop
        db_stop
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        for f in "$API_LOG" "$SEALER_LOG"; do
            if [[ -s "$f" ]]; then
                echo "--- $(basename "$f")（末尾）---" >&2
                tail -n 12 "$f" >&2
            fi
        done
        exit 1
    }

    now_ns() { date +%s%N; }

    start_services() {
        spawn sealer "$SEALER_LOG" env APP__SEALER__ID=auth1-sealer ./target/debug/sealer
        spawn api "$API_LOG" ./target/debug/api
        for _ in $(seq 1 100); do
            alive api || fail "api が起動直後に終了しました"
            alive sealer || fail "sealer が起動直後に終了しました"
            curl -fsS -o /dev/null "${BASE}/healthz" 2>/dev/null && break
            sleep 0.2
        done
        for _ in $(seq 1 200); do
            [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/0/head")" == 200 ]] && break
            sleep 0.2
        done
        wait_election_open "$BASE" 20 || fail "sealer がチェーンを用意しない、または自動で open になりません"
    }
    stop_procs() {
        graceful_stop api
        graceful_stop sealer
    }

    # login_raw JSON → ステータスを STATUS、本文を BODY に入れる（ID とパスワードでのログインは common.sh の db_login、
    # トークンの取り出しは token_of）
    login_raw() { request POST /api/v1/login "" "$1"; }

    # CSV の n 行目（ヘッダを除く）の列（1: login_id, 2: password）
    csv_field() { sed -n "$(($2 + 1))p" "$1" | cut -d, -f"$3"; }

    count() { # count TABLE → 件数
        ks_cql "SELECT count(*) FROM ${KS}.$1" 2>/dev/null | grep -E '^\s*[0-9]+\s*$' | tr -d ' ' | head -1
    }
    expect_count() { # expect_count TABLE N 説明
        local n
        n="$(count "$1")"
        [[ "$n" == "$2" ]] || fail "$3: ${1} が ${n} 件です（期待 $2）"
    }

    echo "== 0. 前提と準備"
    db_check_docker
    echo "DB_BACKEND=${DB_BACKEND}"
    cargo build -q -p api -p sealer -p credgen
    db_ensure
    ks_create
    seed_generate "$TMP/seed" 100
    echo "専用キースペース: ${KS}、有権者 100 人（voter-1〜voter-100）"

    # -----------------------------------------------------------------------
    echo "== 1. credgen で 100 人分を登録する"
    out="$(./target/debug/credgen 2>&1)" || { echo "$out" >&2; fail "credgen が失敗しました"; }
    echo "$out"
    CSV="$APP__CREDENTIALS__OUTPUT_PATH"
    [[ "$(($(wc -l <"$CSV") - 1))" == 100 ]] || fail "CSV の行数が 100（ヘッダ除く）ではありません: $(wc -l <"$CSV")"
    [[ "$(stat -c %a "$CSV")" == 600 ]] || fail "CSV の権限が 0600 ではありません: $(stat -c %a "$CSV")"
    [[ "$(head -1 "$CSV")" == "login_id,password,都道府県,選挙区" ]] || fail "CSV のヘッダが想定と異なります: $(head -1 "$CSV")"
    bad="$(tail -n +2 "$CSV" | cut -d, -f1 | { grep -Ecv '^[ABCDEFGHJKLMNPQRSTUVWXYZ23456789]{10}$' || true; })"
    [[ "$bad" == 0 ]] || fail "形式の不正なログイン ID が ${bad} 件あります"
    bad="$(tail -n +2 "$CSV" | cut -d, -f2 | { grep -Ecv '^[ABCDEFGHJKLMNPQRSTUVWXYZ23456789]{12}$' || true; })"
    [[ "$bad" == 0 ]] || fail "形式の不正なパスワードが ${bad} 件あります"
    [[ "$(tail -n +2 "$CSV" | cut -d, -f1 | sort -u | wc -l | tr -d ' ')" == 100 ]] || fail "ログイン ID が重複しています"
    [[ "$(tail -n +2 "$CSV" | cut -d, -f3,4 | { grep -c , || true; })" == 100 ]] || fail "都道府県・選挙区の列が空の行があります"
    echo "CSV: 100 行・権限 0600・ログイン ID 10 文字 / パスワード 12 文字（0/O/1/I/l なし）・重複なし: OK"
    sed -n 2p "$CSV" | cut -d, -f3,4 | sed 's/^/  郵送用の列の例（都道府県,選挙区）: /'
    git check-ignore -q secrets/credentials.csv || fail "既定の出力先 secrets/credentials.csv が .gitignore に登録されていません"

    expect_count credentials 100 "登録後"
    expect_count voter_roll 100 "登録後"
    expect_count voter_registry 100 "登録後"
    hashes="$(ks_cql "SELECT password_hash FROM ${KS}.credentials" 2>/dev/null | grep -c '\$argon2id\$' || true)"
    [[ "$hashes" == 100 ]] || fail "password_hash が Argon2id の PHC 文字列の行が ${hashes} 件です（期待 100）"
    dump="$(ks_cql "SELECT * FROM ${KS}.credentials" 2>/dev/null; ks_cql "SELECT * FROM ${KS}.voter_registry" 2>/dev/null; ks_cql "SELECT * FROM ${KS}.voter_roll" 2>/dev/null)"
    for n in 1 2 3 4 5 50 100; do
        pw="$(csv_field "$CSV" "$n" 2)"
        if grep -qF "$pw" <<<"$dump"; then fail "平文のパスワードが DB に保存されています（CSV の ${n} 行目）"; fi
    done
    voter_ids="$(ks_cql "SELECT voter_id FROM ${KS}.credentials" 2>/dev/null | { grep -Eo '^\s*[0-9a-f]{32}\s*$' || true; } | tr -d ' ')"
    [[ "$(wc -l <<<"$voter_ids" | tr -d ' ')" == 100 && "$(sort -u <<<"$voter_ids" | wc -l | tr -d ' ')" == 100 ]] \
        || fail "内部の voter_id が、100 個の別々の 32 桁の 16 進数ではありません"
    first_login="$(csv_field "$CSV" 1 1)"
    grep -qF "$first_login" <<<"$voter_ids" && fail "voter_id が、ログイン ID と同じです"
    echo "DB: credentials/voter_roll/voter_registry が各 100 件・ハッシュは Argon2id・平文なし・voter_id は別の乱数: OK"

    # -----------------------------------------------------------------------
    echo "== 2. 正しい ID とパスワードでログインし、投票する"
    start_services
    LOGIN1="$(csv_field "$CSV" 1 1)"
    PASS1="$(csv_field "$CSV" 1 2)"
    db_login "$LOGIN1" "$PASS1"
    [[ "$STATUS" == 200 ]] || fail "正しい ID とパスワードでログインできません（${STATUS}: ${BODY}）"
    TOKEN="$(token_of)"
    [[ -n "$TOKEN" ]] || fail "トークンを取得できません"
    login_raw "{\"voter_id\":\" $(tr 'A-Z' 'a-z' <<<"${LOGIN1:0:5}")-$(tr 'A-Z' 'a-z' <<<"${LOGIN1:5}") \",\"password\":\"$(tr 'A-Z' 'a-z' <<<"$PASS1")\"}"
    [[ "$STATUS" == 200 ]] || fail "小文字・区切りつきの入力（旧名 voter_id）でログインできません（${STATUS}）"
    ballots="$(curl -sS "${BASE}/api/v1/ballot-status" -H "Authorization: Bearer ${TOKEN}")"
    [[ "$({ grep -o '"contest_id"' <<<"$ballots" || true; } | wc -l | tr -d ' ')" == 9 ]] || fail "DB の名簿から、9 枚の投票用紙が見えません: ${ballots}"
    CONTEST1="$(grep -o '"contest_id":"[^"]*"' <<<"$ballots" | head -1 | cut -d'"' -f4)"
    DISTRICT1="${CONTEST1#*/}"
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${CONTEST1}/vote" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"${DISTRICT1}.c1\"}")"
    [[ "$code" == 201 ]] || fail "投票が 201 ではありません（${code}）"
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${CONTEST1}/vote" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"${DISTRICT1}.c2\"}")"
    [[ "$code" == 409 ]] || fail "再投票が 409 ではありません（${code}）"
    echo "ログイン OK（大文字小文字・区切りの揺れと旧名 voter_id も可）・DB の名簿で 9 枚・投票 201・再投票 409: OK"

    # -----------------------------------------------------------------------
    echo "== 3. パスワード違い・存在しない ID・形式が不正な ID は、同じステータスと同じメッセージで失敗する"
    db_login "$LOGIN1" "WRONGPASSW0RD"
    WRONG_STATUS="$STATUS"
    WRONG_BODY="$BODY"
    db_login "ZZZZZZZZZZ" "$PASS1"
    UNKNOWN_STATUS="$STATUS"
    UNKNOWN_BODY="$BODY"
    db_login "bad id!" "$PASS1"
    MALFORMED_STATUS="$STATUS"
    MALFORMED_BODY="$BODY"
    login_raw "{\"login_id\":\"$LOGIN1\"}"
    NOPASS_STATUS="$STATUS"
    NOPASS_BODY="$BODY"
    for pair in "unknown:$UNKNOWN_STATUS:$UNKNOWN_BODY" "malformed:$MALFORMED_STATUS:$MALFORMED_BODY" "nopassword:$NOPASS_STATUS:$NOPASS_BODY"; do
        IFS=: read -r name status body <<<"$pair"
        [[ "$status" == 401 && "$WRONG_STATUS" == 401 ]] || fail "${name}: ステータスが 401 ではありません（${status} / パスワード違い ${WRONG_STATUS}）"
        [[ "$body" == "$WRONG_BODY" ]] || fail "${name}: 本文が、パスワード違いと違います:
  ${name}: ${body}
  パスワード違い: ${WRONG_BODY}"
    done
    [[ "$WRONG_BODY" == *'"error":"unauthorized"'* && "$WRONG_BODY" == *'"message":"'* ]] || fail "エラー応答の形式が想定と異なります: ${WRONG_BODY}"
    [[ "$WRONG_BODY" != *"$LOGIN1"* && "$WRONG_BODY" != *"ZZZZZZZZZZ"* ]] || fail "エラー応答に、入力した ID が含まれています"
    echo "401・本文（${WRONG_BODY}）が、4 種類の失敗で同一: OK"
    measure_ms() { # measure_ms LOGIN PASSWORD N → 平均ミリ秒
        local t0 t1 i
        t0="$(now_ns)"
        for i in $(seq 1 "$3"); do db_login "$1" "$2"; done
        t1="$(now_ns)"
        echo $(((t1 - t0) / $3 / 1000000))
    }
    db_login "$LOGIN1" "WRONGPASSW0RD" # ウォームアップ
    wrong_ms="$(measure_ms "$LOGIN1" "WRONGPASSW0RD" 10)"
    unknown_ms="$(measure_ms "ZZZZZZZZZZ" "WRONGPASSW0RD" 10)"
    echo "応答時間の平均: パスワード違い ${wrong_ms} ms / 存在しない ID ${unknown_ms} ms"
    [[ "$wrong_ms" -ge 3 && "$unknown_ms" -ge 3 ]] || fail "応答が速すぎます（Argon2 の照合が行われていない?）"
    if [[ "$unknown_ms" -lt $((wrong_ms / 2)) || "$unknown_ms" -gt $((wrong_ms * 2)) ]]; then
        fail "存在しない ID の応答時間（${unknown_ms} ms）が、パスワード違い（${wrong_ms} ms）と、2 倍以上違います"
    fi
    if grep -aqF "$PASS1" "$API_LOG" || grep -aq '123456789012' "$API_LOG" || grep -aqF "$LOGIN1" "$API_LOG"; then
        fail "api のログに、パスワード・マイナンバー・ログイン ID が出力されています"
    fi
    echo "応答時間が同程度・ログにパスワード/マイナンバー/ログイン ID なし: OK"

    # -----------------------------------------------------------------------
    echo "== 4. 出力しない設定の警告・2 回目の実行（スキップ）・再発行"
    before_hashes="$(ks_cql "SELECT login_id, password_hash FROM ${KS}.credentials" 2>/dev/null | sort | md5sum)"
    set +e
    out="$(APP__CREDENTIALS__OUTPUT_FILE_ENABLED=false ./target/debug/credgen 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -eq 2 ]] || fail "output_file_enabled=false で --confirm-no-output なしが、終了コード 2 ではありません（${rc}）"
    [[ "$out" == *"平文のパスワードは二度と取り出せません"* ]] || fail "警告に「平文のパスワードは二度と取り出せません」がありません: ${out}"
    [[ "$out" == *"何も処理していません"* ]] || fail "何も処理していない旨がありません: ${out}"
    after_hashes="$(ks_cql "SELECT login_id, password_hash FROM ${KS}.credentials" 2>/dev/null | sort | md5sum)"
    [[ "$before_hashes" == "$after_hashes" ]] || fail "確認なしなのに、DB が変わりました"
    echo "output_file_enabled=false: 警告を表示し、--confirm-no-output なしでは何も処理しない（終了コード 2）: OK"
    out="$(APP__CREDENTIALS__OUTPUT_FILE_ENABLED=false ./target/debug/credgen --confirm-no-output 2>&1)" || { echo "$out" >&2; fail "--confirm-no-output で失敗しました"; }
    [[ "$out" == *"平文のパスワードは二度と取り出せません"* && "$out" == *"スキップ（登録済み）100"* ]] || fail "2 回目の実行（スキップ）の出力が想定と異なります: ${out}"
    after_hashes="$(ks_cql "SELECT login_id, password_hash FROM ${KS}.credentials" 2>/dev/null | sort | md5sum)"
    [[ "$before_hashes" == "$after_hashes" ]] || fail "スキップのはずが、認証情報が変わりました"
    echo "2 回目の実行: 100 人すべてスキップ（認証情報は不変）: OK"
    set +e
    out="$(./target/debug/credgen 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"上書きしません"* ]] || fail "出力先が存在するとき、上書きせずに失敗するはずです（rc=${rc}）: ${out}"
    [[ "$(($(wc -l <"$CSV") - 1))" == 100 ]] || fail "既存の CSV が壊れました"
    echo "出力先がすでにあれば、上書きしない: OK"

    REISSUE_CSV="$TMP/credentials-reissue.csv"
    out="$(APP__CREDENTIALS__OUTPUT_PATH="$REISSUE_CSV" ./target/debug/credgen --reissue 2>&1)" || { echo "$out" >&2; fail "--reissue が失敗しました"; }
    [[ "$out" == *"再発行 100"* ]] || fail "再発行が 100 人ではありません: ${out}"
    [[ "$(($(wc -l <"$REISSUE_CSV") - 1))" == 100 && "$(stat -c %a "$REISSUE_CSV")" == 600 ]] || fail "再発行の CSV の件数・権限が想定と異なります"
    expect_count credentials 100 "再発行後（古い認証情報は削除される）"
    db_login "$LOGIN1" "$PASS1"
    [[ "$STATUS" == 401 ]] || fail "再発行の後も、古い ID とパスワードでログインできます（${STATUS}）"
    NEW_LOGIN="$(csv_field "$REISSUE_CSV" 1 1)"
    NEW_PASS="$(csv_field "$REISSUE_CSV" 1 2)"
    [[ "$NEW_LOGIN" != "$LOGIN1" ]] || fail "再発行されたログイン ID が、古いものと同じです"
    db_login "$NEW_LOGIN" "$NEW_PASS"
    [[ "$STATUS" == 200 ]] || fail "再発行された ID とパスワードでログインできません（${STATUS}）"
    TOKEN="$(token_of)"
    ballots="$(curl -sS "${BASE}/api/v1/ballot-status" -H "Authorization: Bearer ${TOKEN}")"
    [[ "$ballots" == *'"voted":true'* ]] || fail "再発行で、投票済みの記録（内部の voter_id）が保たれていません: ${ballots}"
    echo "再発行: 100 人・古い ID/パスワードは使えない・新しいもので同じ有権者にログイン（投票済みが保たれる）: OK"

    # -----------------------------------------------------------------------
    echo "== 5. db_reset --votes（api / sealer が動いている間は拒否。確認なしでは消えない）"
    set +e
    out="$(./scripts/db_reset.sh --votes --yes 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"api または sealer が動いています"* && "$out" == *"止めて"* ]] || fail "api / sealer が動いているのに、db_reset が拒否しません（rc=${rc}）: ${out}"
    expect_count participation 1 "動作中の拒否の後"
    echo "api / sealer 稼働中: 拒否（何も削除しない）: OK"
    for n in 2 3 4 5 6; do
        db_login "$(csv_field "$REISSUE_CSV" "$n" 1)" "$(csv_field "$REISSUE_CSV" "$n" 2)"
        [[ "$STATUS" == 200 ]] || fail "ログインできません（再発行 ${n} 行目）"
        tok="$(token_of)"
        b="$(curl -sS "${BASE}/api/v1/ballot-status" -H "Authorization: Bearer ${tok}")"
        c="$(grep -o '"contest_id":"[^"]*"' <<<"$b" | head -1 | cut -d'"' -f4)"
        code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${c}/vote" \
            -H "Authorization: Bearer ${tok}" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"${c#*/}.c1\"}")"
        [[ "$code" == 201 ]] || fail "投票が 201 ではありません（${code}）"
    done
    sleep 12 # 間隔（10 秒）が経ち、1 件以上あるので封印される
    stop_procs
    [[ "$(count participation)" -ge 6 && "$(count ballot_pool)" != "" ]] || fail "投票データがありません"
    blocks_before="$(count blocks)"
    [[ "$blocks_before" -ge 2 ]] || fail "封印されたブロックがありません（blocks=${blocks_before}）"
    echo "リセット前: participation=$(count participation) blocks=${blocks_before} credentials=$(count credentials)"
    set +e
    out="$(echo no | ./scripts/db_reset.sh --votes 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"中止しました"* && "$out" == *"削除する件数"* ]] || fail "確認で no と答えても、中止しません（rc=${rc}）: ${out}"
    [[ "$out" == *"participation"* ]] || fail "削除する件数が表示されていません"
    expect_count blocks "$blocks_before" "確認で中止した後"
    echo "件数を表示して確認を求め、no では何も消えない: OK"
    out="$(./scripts/db_reset.sh --votes --yes 2>&1)" || { echo "$out" >&2; fail "db_reset --votes --yes が失敗しました"; }
    echo "$out" | tail -3
    for t in participation slot_state ballot_pool blocks anchors sealer_lease; do
        expect_count "$t" 0 "db_reset --votes の後"
    done
    expect_count credentials 100 "db_reset --votes の後（認証情報は残る）"
    expect_count voter_roll 100 "db_reset --votes の後（名簿は残る）"
    expect_count voter_registry 100 "db_reset --votes の後"
    [[ "$(count cluster_config)" -ge 1 && "$(count signer_keys)" -ge 1 ]] || fail "選挙の定義（cluster_config・signer_keys）が消えています"
    echo "db_reset --votes: 投票データが 0 件、認証情報と選挙の定義は残る: OK"

    start_services
    db_login "$NEW_LOGIN" "$NEW_PASS"
    [[ "$STATUS" == 200 ]] || fail "リセット後に、同じ ID とパスワードでログインできません（${STATUS}）"
    TOKEN="$(token_of)"
    ballots="$(curl -sS "${BASE}/api/v1/ballot-status" -H "Authorization: Bearer ${TOKEN}")"
    [[ "$ballots" != *'"voted":true'* ]] || fail "リセット後も、投票済みのままです"
    c="$(grep -o '"contest_id":"[^"]*"' <<<"$ballots" | head -1 | cut -d'"' -f4)"
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/contests/${c}/vote" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"${c#*/}.c1\"}")"
    [[ "$code" == 201 ]] || fail "リセット後の投票が 201 ではありません（${code}）"
    [[ "$(curl -sS "${BASE}/api/v1/chains/0/head" | sed -n 's/.*"height":\([0-9]*\).*/\1/p')" == 0 ]] || fail "リセット後のチェーンが、ジェネシス（高さ 0）から始まっていません"
    echo "リセット後: 同じ ID でログイン・未投票に戻っている・投票 201・チェーンはジェネシスから: OK"
    stop_procs

    # -----------------------------------------------------------------------
    echo "== 6. db_reset --all の後は、ログインできない"
    out="$(./scripts/db_reset.sh --all --yes 2>&1)" || { echo "$out" >&2; fail "db_reset --all --yes が失敗しました"; }
    echo "$out" | tail -2
    expect_count credentials 0 "db_reset --all の後"
    expect_count voter_roll 0 "db_reset --all の後"
    start_services
    db_login "$NEW_LOGIN" "$NEW_PASS"
    [[ "$STATUS" == 401 ]] || fail "db_reset --all の後も、ログインできます（${STATUS}）"
    stop_procs
    echo "db_reset --all: 認証情報が空になり、ログインできない: OK"

    # -----------------------------------------------------------------------
    echo "== 7. app.env=production では拒否・memory モードは再起動でリセット"
    rm -f "$CSV"
    ./target/debug/credgen >/dev/null 2>&1 || fail "認証情報の再登録に失敗しました"
    expect_count credentials 100 "再登録後"
    mkdir -p "$TMP/config-prod"
    printf '[app]\nmode = "db"\n' >"$TMP/config-prod/production.toml"
    set +e
    out="$(APP_CONFIG_DIR="$TMP/config-prod" APP__APP__ENV=production ./scripts/db_reset.sh --all --yes 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"production"* && "$out" == *"何も削除していません"* ]] || fail "app.env=production で、db_reset が拒否されません（rc=${rc}）: ${out}"
    expect_count credentials 100 "production の拒否の後（何も消えていない）"
    echo "app.env=production: 拒否（何も削除しない）: OK"
    out="$(APP__APP__MODE=memory APP__AUTH__MODE=stub ./scripts/db_reset.sh 2>&1)" || { echo "$out" >&2; fail "memory モードの db_reset が失敗しました"; }
    [[ "$out" == *"再起動するとリセットされます"* ]] || fail "memory モードの案内がありません: ${out}"
    echo "memory モード: 「再起動するとリセットされます」と表示して終了: OK"
    set +e
    out="$(APP__APP__MODE=memory APP__AUTH__MODE=stub ./target/debug/credgen 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"app.mode=db が必要"* ]] || fail "memory モードの credgen は、拒否されるはずです（rc=${rc}）"
    echo "memory モードの credgen: 拒否: OK"

    echo "OK: auth#1 credgen・DB 認証・db_reset（DB_BACKEND=${DB_BACKEND}）"
)

# -----------------------------------------------------------------------------
# 2. 確認用のサンプルデータ（scripts/sample_data.sh。ADR 0023）: 3 つの phase のそれぞれで実行し、出力した CSV の各行について、
#    実際にログイン・再投票・投票を試して、期待結果の列と一致することを確認する（CSV がそのままテスト仕様）。
# -----------------------------------------------------------------------------
check_sample_data() (
    set -euo pipefail
    db_setup_vars
    ks_init auth2

    PORT="${CHECK_API_PORT_SAMPLE:-18824}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    TMP="$(mktemp -d)"
    API_LOG="$TMP/api.log"
    SEALER_LOG="$TMP/sealer.log"
    OUT="$TMP/sample"

    export APP__APP__MODE=db
    export APP__AUTH__MODE=db
    export APP__DB__NODES="127.0.0.1:${DB_PORT}"
    export APP__API__PORT="$PORT"
    export APP__ADMIN__BIND="127.0.0.1:$((PORT + 1))"
    export APP__SAMPLE__OUTPUT_DIR="$OUT"
    # 再投票の鍵（secrets/revote_key）は、この確認専用の secrets ディレクトリに作られ、締切の手続きで破棄される。
    export APP_SECRETS_DIR="$TMP/secrets"
    export APP__SESSION__SECRET="auth2-check-secret-0123456789abcdef"
    export APP__SEALER__SIGNING_SEED="0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d"
    export APP__ADMIN__TOKEN="auth2-check-admin-token-0123456789abcdef"
    export APP__SEALER__LEASE_TTL_SECS=6
    # 投票期間中の事前の投票（16 票）を、10 秒で封印させる（最小件数は既定の 10 件）。締切の手続きの待ち時間も短くする。
    export APP__SEAL__INTERVAL_SECS=10
    export APP__ELECTION__STATE_CACHE_SECS=1
    export APP__API__REQUEST_TIMEOUT_SECS=3
    export APP__AUTH__ARGON2__MEMORY_KIB=4096
    export RUST_LOG="info,tower_http=warn"
    BLANK="$(cfg_get labels.blank_name)"

    cleanup() {
        common_cleanup
        hard_stop api
        hard_stop sealer
        rm -rf "$TMP"
        ks_drop
        db_stop
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        for f in "$API_LOG" "$SEALER_LOG" "$OUT/logs/api.log" "$OUT/logs/sealer.log"; do
            if [[ -s "$f" ]]; then
                echo "--- ${f}（末尾）---" >&2
                tail -n 12 "$f" >&2
            fi
        done
        exit 1
    }

    start_services() {
        spawn sealer "$SEALER_LOG" env APP__SEALER__ID=auth2-sealer APP__ELECTION__SEED_DIR="$OUT/seed" ./target/debug/sealer
        spawn api "$API_LOG" env APP__ELECTION__SEED_DIR="$OUT/seed" ./target/debug/api
        wait_healthz "$BASE" 30 api || fail "api が起動しません"
        chain_ready() { [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/0/head")" == 200 ]]; }
        wait_until 60000 chain_ready || fail "sealer がチェーンを用意しません"
    }
    stop_services() {
        graceful_stop api
        graceful_stop sealer
    }

    # run_case CELL KIND TOKEN: 期待結果のセル（「<操作> → <結果>」）の操作を実際に行い、結果（HTTP ステータスとエラーの種類）を比べる。
    #   操作: 「K枚目」（BALLOTS の表示順で K 番目）か「対象外の投票用紙（<contest_id>）」に、「候補者N」か白票で、投票（vote）／やり直し（revote。
    #         revote には、その投票用紙の ballots_cast を入れる）
    #   結果: 「成功（201）」か「拒否（<status> <error>）」
    run_case() {
        local cell="$1" kind="$2" token="$3" op res contest cast=0 candidate code err=""
        op="${cell%% → *}"
        res="${cell##* → }"
        if [[ "$op" =~ ^([0-9]+)枚目 ]]; then
            contest="$(ballot_field "${BASH_REMATCH[1]}" contest_id)"
            cast="$(ballot_field "${BASH_REMATCH[1]}" ballots_cast)"
        elif [[ "$op" =~ ^対象外の投票用紙（([^）]+)） ]]; then
            contest="${BASH_REMATCH[1]}"
        else
            fail "${ROW_ID}: 操作を読めません: ${op}"
        fi
        [[ -n "$contest" ]] || fail "${ROW_ID}: 対象の投票用紙がありません: ${op}"
        if [[ "$op" == *"${BLANK}で"* ]]; then
            candidate=blank
        elif [[ "$op" =~ 候補者([0-9]+) ]]; then
            candidate="${contest#*/}.c${BASH_REMATCH[1]}"
        else
            fail "${ROW_ID}: 投票先を読めません: ${op}"
        fi
        [[ "$res" =~ （([0-9]{3})(\ ([a-z_]+))?）$ ]] || fail "${ROW_ID}: 結果を読めません: ${res}"
        code="${BASH_REMATCH[1]}"
        err="${BASH_REMATCH[3]}"
        if [[ "$kind" == revote ]]; then
            cast_vote "$token" "$contest" "$candidate" "$cast"
        else
            cast_vote "$token" "$contest" "$candidate"
        fi
        [[ "$STATUS" == "$code" ]] || fail "${ROW_ID}: ${cell}: 実際は ${STATUS}（${BODY}）"
        [[ -z "$err" || "$BODY" == *"\"error\":\"${err}\""* ]] || fail "${ROW_ID}: ${cell}: エラーの種類が違います（${BODY}）"
    }

    # verify_csv PHASE: CSV の形式と、各行の期待結果を確かめる。
    verify_csv() {
        local phase="$1" csv="$OUT/credentials_patterns.csv" line tried=0 skipped=0 token n
        [[ -f "$csv" ]] || fail "${csv} がありません"
        [[ "$(stat -c %a "$csv")" == 600 ]] || fail "CSV の権限が 0600 ではありません: $(stat -c %a "$csv")"
        [[ "$(head -1 "$csv")" == "pattern_id,pattern_name,login_id,password,都道府県,選挙区,投票用紙の数,期待結果_ログイン,期待結果_投票,期待結果_再投票,備考" ]] \
            || fail "CSV のヘッダが想定と異なります: $(head -1 "$csv")"
        [[ "$(tail -n +2 "$csv" | cut -d, -f1 | paste -sd' ')" == "P01 P02 P03 P04 P05 P06 P07 P08 P09 P10 P11 P12 P13 P13" ]] \
            || fail "パターンの行が想定と異なります: $(tail -n +2 "$csv" | cut -d, -f1 | paste -sd' ')"
        [[ "$(awk -F, '{print NF}' "$csv" | sort -u)" == 11 ]] || fail "列の数が 11 ではない行があります"
        while IFS= read -r line; do
            IFS=, read -r ROW_ID name login pw _ dist count e_login e_vote e_revote _ <<<"$line"
            ROW_ID="${ROW_ID}（${name}）"
            if [[ "$e_login" == 作成不可* ]]; then
                [[ -z "$login" && -z "$pw" && "$e_vote" == "$e_login" && "$e_revote" == "$e_login" ]] \
                    || fail "${ROW_ID}: 作成不可の行に、ID・パスワードか別の期待結果があります"
                skipped=$((skipped + 1))
                continue
            fi
            [[ "$e_login" =~ （([0-9]{3}) ]] || fail "${ROW_ID}: 期待結果_ログインを読めません: ${e_login}"
            db_login "$login" "$pw"
            [[ "$STATUS" == "${BASH_REMATCH[1]}" ]] || fail "${ROW_ID}: ログイン: 期待 ${e_login}、実際は ${STATUS}"
            tried=$((tried + 1))
            if [[ "$e_vote" == —* ]]; then
                [[ "$STATUS" != 200 && "$e_revote" == —* ]] || fail "${ROW_ID}: ログインできる行の投票が「—」です"
                continue
            fi
            token="$(token_of)"
            ballots_of "$token"
            n="$(ballot_count)"
            [[ "$n" == "$count" ]] || fail "${ROW_ID}: 投票用紙の数が ${n} です（CSV は ${count}）"
            [[ "$ROW_ID" != P03* || "$dist" == *合区* ]] || fail "${ROW_ID}: 合区の選挙区がありません: ${dist}"
            # 再投票を先に試す（投票の確認で、未投票の投票用紙が投票済みに変わるため）。
            run_case "$e_revote" revote "$token"
            ballots_of "$token"
            run_case "$e_vote" vote "$token"
        done < <(tail -n +2 "$csv")
        echo "  ${phase}: ${tried} 行を試して期待結果どおり・作成不可 ${skipped} 行"
        SKIPPED="$skipped"
    }

    echo "== 0. 準備"
    db_check_docker
    cargo build -q -p api -p sealer -p credgen -p seedgen -p verifier
    db_ensure
    echo "専用キースペース: ${KS}"

    echo "== 1. app.env=production では拒否・memory モードは db モードでの実行を案内する"
    mkdir -p "$TMP/config-prod"
    printf '[app]\nmode = "db"\n' >"$TMP/config-prod/production.toml"
    set +e
    out="$(APP_CONFIG_DIR="$TMP/config-prod" APP__APP__ENV=production ./scripts/sample_data.sh --yes 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"production"* && "$out" == *"何も変更していません"* ]] || fail "app.env=production で拒否されません（rc=${rc}）: ${out}"
    set +e
    out="$(APP__APP__MODE=memory APP__AUTH__MODE=stub ./scripts/sample_data.sh --yes 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -ne 0 && "$out" == *"APP__APP__MODE=db scripts/sample_data.sh"* ]] || fail "memory モードで、db モードでの実行が案内されません（rc=${rc}）: ${out}"
    [[ ! -e "$OUT" ]] || fail "拒否したのに、出力先が作られています"
    set +e
    out="$(./scripts/sample_data.sh --phase later 2>&1)"
    rc=$?
    set -e
    [[ "$rc" -eq 2 && "$out" == *"before / open / closed"* ]] || fail "不正な --phase が、終了コード 2 で拒否されません（rc=${rc}）: ${out}"
    echo "production: 拒否・memory: db モードの案内・不正な --phase: 終了コード 2: OK"
    git check-ignore -q out/sample/credentials_patterns.csv || fail "既定の出力先 out/sample/credentials_patterns.csv が .gitignore に登録されていません"

    # phase ごとに、選挙のルールも変えて、期待結果の計算（allow_revote / allow_blank / max_revotes）を確かめる。
    for spec in "before:scheduled:false:true:5:5" "open:open:true:true:2:0" "closed:closed:true:false:1:1"; do
        IFS=: read -r phase want_state revote blank max want_skipped <<<"$spec"
        echo "== 2-${phase}. sample_data.sh --phase ${phase}（allow_revote=${revote} allow_blank=${blank} max_revotes=${max}）"
        export APP__VOTE__ALLOW_REVOTE="$revote" APP__VOTE__ALLOW_BLANK="$blank" APP__VOTE__MAX_REVOTES="$max"
        out="$(./scripts/sample_data.sh --phase "$phase" --yes 2>&1)" || { echo "$out" >&2; fail "sample_data.sh --phase ${phase} が失敗しました"; }
        [[ "$out" == *"選挙状態: ${want_state}"* ]] || fail "要約に「選挙状態: ${want_state}」がありません: ${out}"
        [[ "$(grep -cE '^P[0-9]{2} ' <<<"$out")" == 14 ]] || fail "要約表に、14 行（P13 は 2 行）がありません: ${out}"
        if grep -Fq "$(sed -n 2p "$OUT/credentials_patterns.csv" | cut -d, -f4)" <<<"$out"; then fail "要約表にパスワードが表示されています"; fi
        [[ ! -e "$OUT/.credgen.csv" && ! -e "$OUT/.credgen-reissue.csv" ]] || fail "credgen の途中の CSV（平文のパスワード）が残っています"
        pgrep -x api >/dev/null && fail "sample_data.sh の終了後も api が動いています"
        start_services
        request GET /api/v1/election-status
        [[ "$BODY" == *"\"phase\":\"${want_state}\""* ]] || fail "再起動後の選挙状態が ${want_state} ではありません: ${BODY}"
        verify_csv "$phase"
        [[ "$SKIPPED" == "$want_skipped" ]] || fail "作成不可の行が ${SKIPPED} 行です（期待 ${want_skipped}）"
        if [[ "$phase" == before ]]; then
            [[ "$(grep -c '作成不可（開始前のため）' "$OUT/credentials_patterns.csv")" == 5 ]] || fail "開始前に、P05〜P09 が「作成不可（開始前のため）」になっていません"
        elif [[ "$phase" == open ]]; then
            # 動いている api があると、DB を消さずに拒否する（CSV もそのまま）。
            before_csv="$(md5sum <"$OUT/credentials_patterns.csv")"
            set +e
            out="$(./scripts/sample_data.sh --phase open --yes 2>&1)"
            rc=$?
            set -e
            [[ "$rc" -ne 0 && "$out" == *"dev_down.sh"* ]] || fail "api が動いているのに、拒否されません（rc=${rc}）: ${out}"
            [[ "$(md5sum <"$OUT/credentials_patterns.csv")" == "$before_csv" ]] || fail "拒否したのに、CSV が変わりました"
            echo "  api の稼働中は拒否（DB・CSV はそのまま）: OK"
        else
            ./target/debug/verifier verify --api "$BASE" >"$TMP/verify.out" 2>&1 || { cat "$TMP/verify.out" >&2; fail "締切後のチェーンの検証が失敗しました"; }
            [[ ! -e "$APP_SECRETS_DIR/revote_key" ]] || fail "締切後も revote_key が残っています"
            echo "  締切後: verifier verify OK・revote_key は破棄済み: OK"
        fi
        stop_services
    done
    echo "OK: auth#2 確認用のサンプルデータ（DB_BACKEND=${DB_BACKEND}）"
)

check_credgen_and_db_auth
check_sample_data

echo "OK: check/auth.sh"
