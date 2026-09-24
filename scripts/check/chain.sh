#!/usr/bin/env bash
# chain スイート: ハッシュチェーン全般（対応表: docs/testing.md）。
#   1. verifier demo（正常チェーンの検証・改ざん検出。オフライン）
#   2. in-process sealer（memory）: 封印トリガー（count/time）・verify・tamper 検出・SIGTERM ではフラッシュしない
#   3. 「データに更新がない場合は、ブロックチェーンに何も追加しない」（memory）
#   4. DB 永続化・復旧: スキーマ投入・infra-scylla の統合テスト・再起動をまたいだ保持・クラッシュからの復旧
#   5. 複数 sealer のリース引き継ぎ・分岐なし・アンカー
#   6. verifier tally（集計）
#   7. ブロックチェーンのビューア API（ページ送り・Cache-Control・reveal_ballots）
#   8. 封印ルール（原則9。dev の設定 interval=10 秒・min=10）: 9 票は 20 秒待っても封印されない → 10 票目で
#      すぐに封印 → 5 票 → close --now → trigger=close で 5 件のブロック
# 4・5・6 は DB（Cassandra/ScyllaDB）を使う。専用のキースペースを使い、共用の vote には触れない。
# 環境変数（APP__DB__BACKEND / DB_PORT / KEEP_KEYSPACE / STOP_DB）は scripts/lib/common.sh を参照。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."
source scripts/lib/common.sh

echo "== 0. 前提の確認と共通ビルド"
db_setup_vars
db_check_docker
db_static_checks
echo "DB_BACKEND=${DB_BACKEND}、静的検査（共通 CQL・ヒープ設定）: OK"
cargo build -q -p api --features dev-tools -p sealer -p verifier -p seedgen
echo "共通ビルド: OK（api[dev-tools]・sealer・verifier・seedgen）"

# ===========================================================================
# 1. verifier demo（旧 check_step1.sh）
# ===========================================================================
check_demo() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    out="$(./target/debug/verifier demo)"
    echo "$out"
    # 表: ジェネシス + 3 ブロック（100/100/50 件）
    for row in '0 \| +0 \|' '1 \| +100 \|' '2 \| +100 \|' '3 \| +50 \|'; do
        grep -Eq "^ +${row}" <<<"$out" || fail "表に行 '${row}' がありません"
    done
    grep -q '検証 OK' <<<"$out" || fail "「検証 OK」が出力されていません"
    grep -q '改ざん検出' <<<"$out" || fail "「改ざん検出」が出力されていません"
    grep -q 'MerkleRootMismatch' <<<"$out" || fail "エラー種別が出力されていません"
    grep -q '改ざんした票: 包含証明 失敗' <<<"$out" || fail "包含証明の失敗が出力されていません"
    set +e
    ./target/debug/verifier unknown >/dev/null 2>&1
    code=$?
    set -e
    [[ "$code" -eq 2 ]] || fail "未知のサブコマンドが exit 2 になりません (exit=${code})"
    echo "OK: chain#1 verifier demo"
)

# ===========================================================================
# 2. in-process sealer（memory。旧 check_step4.sh）
# ===========================================================================
check_inprocess_sealer() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18801}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    export APP__SESSION__SECRET="chain2-check-secret-0123456789abcdef"
    export APP__SHARD__COUNT=1
    export APP__SEAL__MAX_BALLOTS=100
    export APP__SEAL__INTERVAL_SECS=10
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL=10
    # 開始時刻を過去にして、起動直後に自動で open にする（原則17・18。このスイートは封印を見るので、
    # 投票の受付期間そのものは確認しない）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,tower_http=warn"

    LOG="$(mktemp)"
    CODES="$(mktemp)"
    cleanup() {
        common_cleanup
        rm -rf "${SEED_TMP:-}"
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

    # inject N: N 番目の有権者（voter-N）がログインして、自分の最初の投票用紙に投票し、HTTP ステータスを出力する
    inject() {
        local n="$1" token
        token="$(login "voter-${n}")"
        seed_vote "$n" 1 "$token"
    }
    export -f inject

    VERIFY_OUT=""
    VERIFY_RC=0
    run_verify() {
        set +e
        VERIFY_OUT="$(./target/debug/verifier verify --api "$BASE" "$@" 2>&1)"
        VERIFY_RC=$?
        set -e
    }

    echo "== 0. 静的検査（dev-tools 有効）"
    cargo clippy -q --workspace --all-targets --features api/dev-tools -- -D warnings
    cargo test -q -p infra-memory --features dev-tools >/dev/null
    cargo test -q -p api --features dev-tools >/dev/null

    echo "== 1. 起動"
    SEED_TMP="$(mktemp -d)"
    seed_generate "$SEED_TMP" 600
    spawn api "$LOG" env "APP__API__PORT=${PORT}" ./target/debug/api
    wait_healthz "$BASE" 10 api || fail "api が応答しません"
    wait_election_open "$BASE" 5 || fail "自動で open になりませんでした"

    request GET /api/v1/chains/0/head
    expect_status 200 "起動直後の head"
    [[ "$BODY" == *'"height":0'* ]] || fail "起動直後はジェネシス（高さ 0）のはずです: ${BODY}"
    PUBLIC_KEY="$(sed -n 's/.*"signer_public_key":"\([0-9a-f]*\)".*/\1/p' <<<"$BODY")"
    [[ ${#PUBLIC_KEY} -eq 64 ]] || fail "公開鍵を取得できません: ${BODY}"
    echo "ジェネシス: OK"

    echo "== 2. 250 人分の投票を投入"
    seq 1 250 | xargs -P 25 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$(wc -l <"$CODES" | tr -d ' ')" == 250 ]] || fail "投票の応答が 250 件ではありません"
    [[ "$({ grep -vc '^201$' "$CODES" || true; })" == 0 ]] || fail "201 以外の応答があります"
    INJECTED_AT="$(now_ms)"
    echo "投入完了: 250 件すべて 201"

    echo "== 3. 2 秒以内に height=1, 2（各 100 件, trigger=count）"
    wait_for_log 'shard=0 height=1 count=100 trigger=count' 2000 "$LOG" || fail "height=1 が 2 秒以内に封印されません"
    wait_for_log 'shard=0 height=2 count=100 trigger=count' 2000 "$LOG" || fail "height=2 が 2 秒以内に封印されません"
    echo "height=1,2: OK"

    echo "== 4. 12 秒以内に height=3（50 件（最小件数 10 以上）, trigger=time）"
    elapsed_since_injection() { echo $(($(now_ms) - INJECTED_AT)); }
    wait_for_log 'shard=0 height=3 count=50 trigger=time' $((12000 - $(elapsed_since_injection))) "$LOG" \
        || fail "height=3 が投入完了から 12 秒以内に封印されません"
    echo "height=3: OK（投入完了から $(elapsed_since_injection) ms）"

    echo "== 5. さらに 25 秒待っても新しいブロックができない"
    sleep 25
    if grep -Eq 'shard=0 height=4 ' "$LOG"; then fail "25 秒待つ間に新しいブロックができました"; fi
    request GET /api/v1/chains/0/head
    [[ "$BODY" == *'"height":3'* ]] || fail "head の高さが 3 のままではありません: ${BODY}"
    echo "新しいブロックなし: OK"

    echo "== 6. verify が OK"
    run_verify --public-key "$PUBLIC_KEY"
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 0 ]] || fail "verify が exit ${VERIFY_RC} でした"
    [[ "$VERIFY_OUT" == *'検証 OK'* ]] || fail "「検証 OK」が出力されていません"
    [[ "$VERIFY_OUT" == *'blocks=4 ballots=250'* ]] || fail "ブロック数・票数が想定と異なります（4 ブロック・250 票）"

    echo "== 7. /debug/tamper の後は verify が失敗"
    request POST /debug/tamper
    [[ "$STATUS" == 200 ]] || fail "/debug/tamper が ${STATUS} でした: ${BODY}"
    run_verify --public-key "$PUBLIC_KEY"
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 3 ]] || fail "改ざん後の verify が exit 3 ではありません (exit=${VERIFY_RC})"
    [[ "$VERIFY_OUT" == *'検証 NG'* ]] || fail "「検証 NG」が出力されていません"

    echo "== 8. 30 票を投票して SIGTERM → フラッシュしない（原則9。残りの封印は締切の手続きの中でだけ）"
    seq 501 530 | xargs -P 30 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$({ grep -c '^201$' "$CODES" || true; })" == 30 ]] || fail "30 票が受理されていません"
    # 30 件 >= 最小件数 10 だが、前回の封印（height=3）から 10 秒経つ前に止める。
    graceful_stop api
    if grep -Eq 'shard=0 height=4 ' "$LOG"; then fail "SIGTERM で停止したときに、ブロックが作られました"; fi
    grep -aq '未封印の票はフラッシュせずに残します' "$LOG" || fail "停止時に、フラッシュしない旨のログがありません"
    echo "SIGTERM: フラッシュしない（ブロックは増えない）: OK"

    # 秘密投票: ログに投票者 ID が出ていないこと（原則1）
    if grep -Eq 'voter-[0-9]' "$LOG"; then
        fail "サーバログに投票者 ID が出力されています"
    fi
    echo "OK: chain#2 in-process sealer"
)

# ===========================================================================
# 3. 「データに更新がない場合は、ブロックチェーンに何も追加しない」（memory。旧 check_step10.sh）
# ===========================================================================
check_no_append_without_change() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18802}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    export APP__SESSION__SECRET="chain3-check-secret-0123456789abcdef"
    export APP__SHARD__COUNT=1
    export APP__SEAL__MAX_BALLOTS=100
    export APP__SEAL__INTERVAL_SECS=10
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL=10
    export APP__API__PORT="$PORT"
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    # sealer=debug: アンカーを作らなかったときの skip のログを検査する。
    export RUST_LOG="info,sealer=debug,tower_http=warn"

    LOG="$(mktemp)"
    cleanup() {
        common_cleanup
        rm -rf "${SEED_TMP:-}"
        hard_stop api
        rm -f "$LOG"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        if [[ -s "$LOG" ]]; then
            echo "--- api log（末尾）---" >&2
            tail -n 30 "$LOG" >&2
        fi
        exit 1
    }

    # start_api: 新しいプロセスとして起動する（memory モードなので、前回までの状態は引き継がない。
    # ログも、そのプロセス分だけを見るよう、毎回作り直す）。
    start_api() {
        spawn api "$LOG" ./target/debug/api
        wait_healthz "$BASE" 10 api || fail "${BASE}/healthz が応答しません"
        wait_election_open "$BASE" 5 || fail "自動で open になりませんでした"
    }
    stop_api() { graceful_stop api; }

    chain_height() {
        curl -sS "${BASE}/api/v1/chains/0/head" | sed -n 's/.*"height":\([0-9]*\).*/\1/p'
    }
    anchor_seq() {
        local code
        code="$(curl -sS -o "$TMP_BODY" -w '%{http_code}' "${BASE}/api/v1/anchors/latest")"
        if [[ "$code" == 404 ]]; then
            echo 0
        elif [[ "$code" == 200 ]]; then
            sed -n 's/.*"seq":\([0-9]*\).*/\1/p' "$TMP_BODY"
        else
            fail "GET /api/v1/anchors/latest が ${code} です"
        fi
    }
    log_count() { { grep -ac -- "$1" "$LOG" || true; }; }
    vote_once() {
        local token code
        token="$(login "voter-$1")"
        [[ -n "$token" ]] || fail "ログインできません"
        code="$(seed_vote "$1" 1 "$token")"
        [[ "$code" == 201 ]] || fail "投票が 201 ではありません（${code}）"
    }
    expect_state() {
        local h s
        h="$(chain_height)"
        s="$(anchor_seq)"
        [[ "$h" == "$1" && "$s" == "$2" ]] || fail "$3: ブロックの高さ=${h}（期待 $1）、アンカーの seq=${s}（期待 $2）"
    }

    TMP_BODY="$(mktemp)"
    SEED_TMP="$(mktemp -d)"
    seed_generate "$SEED_TMP" 15

    echo "== 1. 投票 0 件で 60 秒待つ: ブロックもアンカーも増えない"
    start_api
    expect_state 0 0 "起動直後（ジェネシスだけ。アンカーは無い）"
    for i in 1 2 3 4 5 6; do
        sleep 10
        expect_state 0 0 "投票 0 件で $((i * 10)) 秒後"
    done
    [[ "$(log_count 'ブロックを封印しました')" == 0 ]] || fail "投票 0 件なのにブロックが封印されました"
    [[ "$(log_count 'アンカーを作成しました')" == 0 ]] || fail "投票 0 件なのにアンカーが作成されました"
    skips_idle="$(log_count 'アンカーの作成を skip しました')"
    [[ "$skips_idle" -ge 4 ]] || fail "DEBUG ログに skip が出ていません（${skips_idle} 件。60 秒で 5〜6 回のはず）"
    echo "60 秒間、ブロック 0・アンカー 0（skip のログ ${skips_idle} 件）: OK"

    echo "== 2. 10 票（最小件数）投票する: ブロックが 1 つ、アンカーが 1 つ増える"
    for n in $(seq 1 10); do vote_once "$n"; done
    deadline=$((SECONDS + 40))
    until [[ "$(chain_height)" == 1 && "$(anchor_seq)" == 1 ]]; do
        ((SECONDS < deadline)) || fail "10 票の投票から 40 秒以内に、ブロック 1・アンカー 1 になりません（高さ=$(chain_height) seq=$(anchor_seq)）"
        sleep 1
    done
    expect_state 1 1 "10 票の投票後"
    echo "ブロックが 1 つ（高さ 1）、アンカーが 1 つ（seq=1）に増えた: OK"

    echo "== 3. さらに 60 秒待つ: 増えない。verify が OK"
    skips_before="$(log_count 'アンカーの作成を skip しました')"
    for i in 1 2 3 4 5 6; do
        sleep 10
        expect_state 1 1 "10 票の後、さらに $((i * 10)) 秒後"
    done
    [[ "$(log_count 'ブロックを封印しました')" == 1 ]] || fail "封印のログが 1 件ではありません"
    [[ "$(log_count 'アンカーを作成しました')" == 1 ]] || fail "アンカー作成のログが 1 件ではありません"
    skips_after="$(log_count 'アンカーの作成を skip しました')"
    [[ "$((skips_after - skips_before))" -ge 4 ]] || fail "増えない期間に skip のログが出ていません（${skips_before} → ${skips_after}）"
    echo "さらに 60 秒間、増えない（skip のログ +$((skips_after - skips_before)) 件）: OK"

    set +e
    VERIFY_OUT="$(./target/debug/verifier verify --api "$BASE" 2>&1)"
    VERIFY_RC=$?
    set -e
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 0 && "$VERIFY_OUT" == *'1 シャード, 2 ブロック, 10 票'* && "$VERIFY_OUT" == *'アンカー: seq=1'* ]] \
        || fail "verify が OK ではないか、内容が想定と異なります（1 シャード・2 ブロック（ジェネシス含む）・10 票・アンカー seq=1）(exit=${VERIFY_RC})"

    echo "== 4. 変化なしで SIGTERM: 最終アンカーは、最新の状態を指していることを確認するだけ（作らない）"
    stop_api
    grep -aq '最終アンカー: 最後のアンカーが最新の状態を指しています（新しいアンカーは作りません）' "$LOG" \
        || fail "最終アンカーの確認のログがありません"
    [[ "$(log_count 'アンカーを作成しました')" == 1 ]] || fail "変化がないのに、停止時にアンカーが作られました"
    [[ "$(log_count 'ブロックを封印しました')" == 1 ]] || fail "変化がないのに、停止時にブロックが作られました"
    echo "停止時: 最終アンカーは確認のみ（アンカー 1・ブロック 1 のまま）: OK"

    echo "== 5. 封印される前に SIGTERM: フラッシュしない（原則9）。ブロックもアンカーも作らない"
    start_api
    vote_once 11
    stop_api
    [[ "$(log_count 'ブロックを封印しました')" == 0 ]] || fail "停止時にブロックが作られました（SIGTERM ではフラッシュしない）"
    [[ "$(log_count 'アンカーを作成しました')" == 0 ]] || fail "ブロックが増えていないのに、停止時にアンカーが作られました"
    echo "停止時: フラッシュせず、ブロック 0・アンカー 0: OK"

    echo "OK: chain#3 no-append-without-change"
)

# ===========================================================================
# 4. DB 永続化・復旧（DB。旧 check_step6.sh）
# ===========================================================================
check_db_persistence() (
    set -euo pipefail
    ALICE_NO=1001
    BOB_NO=1002
    SEED_TMP="$(mktemp -d)"
    seed_generate "$SEED_TMP" 1100
    ks_init chain4

    PORT="${CHECK_API_PORT:-18803}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    export APP__SESSION__SECRET="chain4-check-secret-0123456789abcdef"
    export APP__APP__MODE=db
    export APP__SEALER__LEASE_TTL_SECS=6
    export APP__DB__NODES="127.0.0.1:${DB_PORT}"
    export APP__SHARD__COUNT=1
    export APP__SEAL__MAX_BALLOTS=100
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL=10
    export APP__SEALER__SIGNING_SEED="0707070707070707070707070707070707070707070707070707070707070707"
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,sealer=debug,tower_http=warn"

    LOG="$(mktemp)"
    CODES="$(mktemp)"
    SEALER_SEQ=0
    cleanup() {
        common_cleanup
        hard_stop api
        hard_stop sealer
        rm -f "$LOG" "$CODES"
        rm -rf "$SEED_TMP"
        ks_drop
        db_stop
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        echo "--- api log（末尾）---" >&2
        tail -n 40 "$LOG" >&2
        exit 1
    }

    inject() {
        local n="$1" token
        token="$(login "voter-${n}")"
        seed_vote "$n" 1 "$token"
    }
    export -f inject

    VERIFY_OUT=""
    VERIFY_RC=0
    run_verify() {
        set +e
        VERIFY_OUT="$(./target/debug/verifier verify --api "$BASE" "$@" 2>&1)"
        VERIFY_RC=$?
        set -e
    }

    # start_api INTERVAL_SECS LABEL: sealer（独立プロセス）と api を起動する。
    start_api() {
        echo "=== 起動: $2（seal.interval_secs=$1）" >>"$LOG"
        SEALER_SEQ=$((SEALER_SEQ + 1))
        # sealer と api で、同じログファイルを共有し、再起動をまたいで追記する（起動 1〜3 を通して、
        # 累積したログに対して grep する。fail() がそのまま末尾を表示できるよう、1 つのファイルにまとめる）。
        spawn_append sealer "$LOG" env "APP__SEAL__INTERVAL_SECS=$1" "APP__SEALER__ID=chain4-sealer-${SEALER_SEQ}" ./target/debug/sealer
        spawn_append api "$LOG" env "APP__API__PORT=${PORT}" ./target/debug/api
        wait_healthz "$BASE" 20 api || fail "${BASE}/healthz が応答しません（$2）"
        # sealer がシャード 0 のリースを取り、チェーン（ジェネシス）が読めるようになるまで待つ
        # （前回の sealer がクラッシュした場合は、そのリースの期限切れまで待つ）。
        local ready=0
        for _ in $(seq 1 200); do
            alive sealer || fail "sealer が終了しました（$2）"
            if [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/0/head")" == 200 ]]; then
                ready=1
                break
            fi
            sleep 0.2
        done
        [[ "$ready" == 1 ]] || fail "sealer がチェーンを用意しません（$2）"
        wait_election_open "$BASE" 15 || fail "自動で open になりませんでした（$2）"
    }
    stop_api_gracefully() {
        graceful_stop api
        graceful_stop sealer
    }
    crash_api() {
        hard_stop api
        hard_stop sealer
    }
    field() { sed -n "s/.*\"$1\":\\(\"\\?\\)\\([^\",}]*\\)\\1.*/\\2/p" <<<"$BODY"; }

    echo "== 1. DB の起動と、専用キースペース（${KS}）へのスキーマ投入"
    db_ensure
    ks_create
    echo "スキーマ投入: OK（2 回流しても成功）"

    echo "== 2. infra-scylla の統合テスト"
    it_out="$(cargo test -p infra-scylla -- --ignored --test-threads=4 2>&1)" \
        || { echo "$it_out" >&2; fail "infra-scylla の統合テストが失敗しました"; }
    grep -E '^test result: .* [1-9][0-9]* passed' <<<"$it_out" | head -1

    # -----------------------------------------------------------------------
    echo "== 3. 起動 1（封印間隔 600 秒）: DB 固有の並列書き込み（LWT）と、SIGTERM ではフラッシュしないこと"
    # ログイン・状態一覧・候補者一覧の詳細な検証は、保存先に依存しないアプリケーション層のロジックであり、
    # election.sh（旧 check_step3）ですでに検証済みなので、ここでは行わない（docs/testing.md の「除外した項目」を参照）。
    # ここで見るのは、DB 固有の並列書き込み（Cassandra の LWT）が排他制御として機能すること。
    start_api 600 "起動 1"
    request GET /api/v1/chains/0/head
    expect_status 200 "起動直後の head"
    [[ "$BODY" == *'"height":0'* ]] || fail "起動直後はジェネシス（高さ 0）のはずです: ${BODY}"
    PUBLIC_KEY="$(sed -n 's/.*"signer_public_key":"\([0-9a-f]*\)".*/\1/p' <<<"$BODY")"
    [[ ${#PUBLIC_KEY} -eq 64 ]] || fail "公開鍵を取得できません: ${BODY}"

    ALICE="$(login "voter-${ALICE_NO}")"
    [[ -n "$ALICE" ]] || fail "トークンを取得できません"
    ALICE_DISTRICT="$(seed_district "$ALICE_NO" 1)"
    request POST "/api/v1/contests/${SEED_ID}/${ALICE_DISTRICT}/vote" "$ALICE" "{\"candidate_id\":\"${ALICE_DISTRICT}.c1\"}"
    expect_status 201 "投票"
    request POST "/api/v1/contests/${SEED_ID}/${ALICE_DISTRICT}/vote" "$ALICE" "{\"candidate_id\":\"${ALICE_DISTRICT}.c2\"}"
    expect_status 409 "同一の投票用紙への再投票"
    [[ "$(pool_total)" == 1 ]] || fail "/debug/pool の total が 1 ではありません: ${BODY}"
    echo "投票 201 / 再投票 409 / プール 1 件: OK"

    BOB="$(login "voter-${BOB_NO}")"
    BOB_DISTRICT="$(seed_district "$BOB_NO" 1)"
    seq 1 100 | xargs -P 100 -I{} curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
        -H "Authorization: Bearer ${BOB}" -H 'Content-Type: application/json' \
        -d "{\"candidate_id\":\"${BOB_DISTRICT}.c3\"}" "${BASE}/api/v1/contests/${SEED_ID}/${BOB_DISTRICT}/vote" >"$CODES"
    created="$({ grep -c '^201$' "$CODES" || true; })"
    conflict="$({ grep -c '^409$' "$CODES" || true; })"
    [[ "$(wc -l <"$CODES" | tr -d ' ')" == 100 ]] || fail "応答が 100 件ではありません"
    [[ "$created" == 1 && "$conflict" == 99 ]] \
        || fail "並列 100 で 201=${created}, 409=${conflict}（期待: 201=1, 409=99。その他: $(sort "$CODES" | uniq -c | tr '\n' ' ')）"
    [[ "$(pool_total)" == 2 ]] || fail "並列投票後の /debug/pool の total が 2 ではありません: ${BODY}"
    echo "並列 100 リクエスト（DB の LWT）: 201=1, 409=99, プール 2 件: OK"

    echo "-- SIGTERM（api → sealer）: sealer はフラッシュせずに（原則9）、リースを解放して終了する"
    stop_api_gracefully
    if grep -aq 'ブロックを封印しました' "$LOG"; then fail "SIGTERM で停止したときに、ブロックが作られました"; fi
    grep -aq 'リースを解放しました' "$LOG" || fail "SIGTERM 後に、リースを解放していません"
    echo "SIGTERM: フラッシュしない（ブロック 0）・リースを解放: OK"

    head_json() {
        request GET /api/v1/chains/0/head
        expect_status 200 "head"
    }

    # -----------------------------------------------------------------------
    echo "== 4. 起動 2（封印間隔 10 秒・最小 10 件）: 再起動後の保持と、投票開始（DB）からの経過時間"
    start_api 10 "起動 2（再起動）"
    head_json
    [[ "$(field height)" == 0 ]] || fail "SIGTERM で止めたので、ブロックは無いはずです: ${BODY}"
    [[ "$(field signer_public_key)" == "$PUBLIC_KEY" ]] || fail "再起動で署名の公開鍵が変わりました"
    [[ "$(pool_total)" == 2 ]] || fail "SIGTERM 前の未封印の 2 票が、DB に保持されていません: ${BODY}"

    ALICE="$(login "voter-${ALICE_NO}")"
    request GET /api/v1/ballot-status "$ALICE"
    [[ "$(count '"voted":true')" == 1 && "$(count '"voted":false')" == 8 ]] \
        || fail "再起動後に投票状況が保持されていません: ${BODY}"
    request POST "/api/v1/contests/${SEED_ID}/${ALICE_DISTRICT}/vote" "$ALICE" "{\"candidate_id\":\"${ALICE_DISTRICT}.c4\"}"
    expect_status 409 "再起動後の再投票"
    echo "再起動後: 未封印の 2 票・投票状況・公開鍵の保持 / 再投票 409: OK"

    # 経過時間の起点は投票開始（起動 1 の時刻。DB の選挙状態に記録）なので、もう 10 秒以上経っている。
    # 最小件数（10）に達した時点で、すぐに時間で封印される（2 + 8 = 10 票）。
    for n in $(seq 1 8); do
        [[ "$(inject "$n")" == 201 ]] || fail "投票が 201 ではありません（voter-${n}）"
    done
    wait_for_log 'shard=0 height=1 count=10 trigger=time' 3000 "$LOG" \
        || fail "10 件目で、すぐに（投票開始から 10 秒以上経っているので）封印されません"
    echo "height=1（10 件, trigger=time。起点は DB の投票開始時刻）: OK"
    head_json
    HEAD1_HASH="$(field block_hash)"

    # 件数による封印: 100 件に達したら、すぐに 100 件で封印する。
    seq 101 200 | xargs -P 25 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$(wc -l <"$CODES" | tr -d ' ')" == 100 && "$({ grep -vc '^201$' "$CODES" || true; })" == 0 ]] \
        || fail "100 票がすべて 201 ではありません"
    wait_for_log 'shard=0 height=2 count=100 trigger=count' 3000 "$LOG" || fail "height=2 が 100 件で封印されません"
    echo "height=2（100 件, trigger=count）: OK"

    sleep 25
    if grep -Eq 'shard=0 height=3 ' "$LOG"; then fail "25 秒待つ間に新しいブロックができました（未封印 0 件）"; fi
    head_json
    [[ "$(field height)" == 2 ]] || fail "head の高さが 2 のままではありません: ${BODY}"
    HEAD2_HASH="$(field block_hash)"
    echo "25 秒待っても新しいブロックなし: OK"

    run_verify --public-key "$PUBLIC_KEY"
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 0 && "$VERIFY_OUT" == *'blocks=3 ballots=110'* ]] \
        || fail "verify が OK ではないか、ブロック数・票数が想定と異なります（3 ブロック・110 票）(exit=${VERIFY_RC})"

    # -----------------------------------------------------------------------
    echo "== 5. 起動 2 → クラッシュ（SIGKILL）→ 起動 3: プールの票の保持と復旧"
    # 5 票（最小件数 10 未満なので、時間では封印されない）を入れて、クラッシュさせる。
    seq 1011 1015 | xargs -P 5 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$({ grep -c '^201$' "$CODES" || true; })" == 5 ]] || fail "5 票が受理されていません"
    [[ "$(pool_total)" == 5 ]] || fail "クラッシュ前のプールが 5 件ではありません: ${BODY}"

    # 秘密投票: DB 上の票の書き込み時刻（WRITETIME）が、分に丸められていること。
    wt="$(ks_cql "SELECT WRITETIME(contest_id) FROM ${KS}.ballot_pool WHERE shard = 0" 2>/dev/null \
        | grep -E '^\s*[0-9]+\s*$' | tr -d ' ' || true)"
    [[ "$(wc -l <<<"$wt" | tr -d ' ')" == 5 ]] || fail "DB から WRITETIME を 5 件取得できません: ${wt}"
    while read -r t; do
        ((t % 60000000 == 0)) || fail "票の書き込み時刻が分に丸められていません: ${t}"
    done <<<"$wt"
    echo "票の書き込み時刻（WRITETIME）: すべて分に丸められている OK"

    crash_api
    echo "-- api と sealer を SIGKILL しました（フラッシュ・リース解放なし。プールに 5 票が残っているはず）"

    start_api 10 "起動 3（クラッシュ後）"
    [[ "$(pool_total)" == 5 ]] || fail "クラッシュ後にプールの 5 票が保持されていません: ${BODY}"
    ALICE="$(login "voter-${ALICE_NO}")"
    request POST "/api/v1/contests/${SEED_ID}/${ALICE_DISTRICT}/vote" "$ALICE" "{\"candidate_id\":\"${ALICE_DISTRICT}.c1\"}"
    expect_status 409 "クラッシュ後の再投票（LWT の保持）"
    CRASH1="$(login voter-1011)"
    CRASH1_DISTRICT="$(seed_district 1011 1)"
    request POST "/api/v1/contests/${SEED_ID}/${CRASH1_DISTRICT}/vote" "$CRASH1" "{\"candidate_id\":\"${CRASH1_DISTRICT}.c1\"}"
    expect_status 409 "クラッシュ前に投票した人の再投票"
    head_json
    [[ "$(field height)" == 2 && "$(field block_hash)" == "$HEAD2_HASH" ]] \
        || fail "クラッシュ後にチェーンの先頭が変わっています: ${BODY}"
    request GET /api/v1/chains/0/blocks/1
    [[ "$(field block_hash)" == "$HEAD1_HASH" ]] || fail "再起動をまたいで既存のブロックのハッシュが変わりました"
    echo "クラッシュ後: プール 5 票・投票状況・チェーンが保持されている OK"

    # さらに 5 票で最小件数（10）に達する。クラッシュした sealer のリース（TTL 6 秒）の期限切れ → 引き継ぎ →
    # 前回の封印（引き継いだ sealer は、先頭ブロックの分の最後の秒とみなす。最大 59 秒遅い側）から 10 秒、を待つ。
    seq 1016 1020 | xargs -P 5 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$({ grep -c '^201$' "$CODES" || true; })" == 5 ]] || fail "追加の 5 票が受理されていません"
    wait_for_log 'shard=0 height=3 count=10 trigger=time' 90000 "$LOG" || fail "クラッシュ後にプールの 10 票が封印されません"
    [[ "$(pool_total)" == 0 ]] || fail "封印後にプールが空になっていません: ${BODY}"
    run_verify --public-key "$PUBLIC_KEY"
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 0 && "$VERIFY_OUT" == *'blocks=4 ballots=120'* ]] \
        || fail "最終の verify が OK ではありません（4 ブロック・120 票のはず）(exit=${VERIFY_RC})"
    stop_api_gracefully

    # 完全修飾テーブル名を使っているので、Cassandra の「USE <keyspace> with prepared statements」の警告は出ないはず。
    if grep -Eqi 'USE <keyspace>|with prepared statements' "$LOG"; then
        fail "api / sealer のログに「USE <keyspace> with prepared statements」の警告が出ています"
    fi
    echo "ログに USE の警告なし OK"

    echo "OK: chain#4 DB 永続化・復旧（DB_BACKEND=${DB_BACKEND}）"
)

# ===========================================================================
# 5. 複数 sealer のリース引き継ぎ（DB。旧 check_step7.sh）
# ===========================================================================
check_multi_sealer() (
    set -euo pipefail
    SEED_TMP="$(mktemp -d)"
    seed_generate "$SEED_TMP" 1000
    ks_init chain5

    PORT="${CHECK_API_PORT:-18804}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE
    export APP__SESSION__SECRET="chain5-check-secret-0123456789abcdef"
    export APP__APP__MODE=db
    export APP__DB__NODES="127.0.0.1:${DB_PORT}"
    export APP__SHARD__COUNT=4
    export APP__SEAL__MAX_BALLOTS=100
    export APP__SEAL__INTERVAL_SECS=10
    # このスイートはリースの引き継ぎを見る。投入後の残り（シャードごとの端数）が最小件数未満で残ると
    # 「全票が封印される」を待てないので、最小件数を 1 にする（最小件数そのものは chain#8 が確認する）。
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL=1
    export APP__SEALER__LEASE_TTL_SECS=6
    export APP__SEALER__SIGNING_SEED="0808080808080808080808080808080808080808080808080808080808080808"
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,sealer=debug,tower_http=warn"

    CODES="$(mktemp)"
    cleanup() {
        common_cleanup
        hard_stop api
        hard_stop sealer-a
        hard_stop sealer-b
        rm -f "$CODES"
        rm -rf "$SEED_TMP"
        ks_drop
        db_stop
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        local name log
        for name in sealer-a sealer-b api; do
            log="$(log_of "$name")"
            if [[ -n "$log" && -s "$log" ]]; then
                echo "--- ${name} のログ（末尾 25 行）---" >&2
                tail -n 25 "$log" >&2
            fi
        done
        exit 1
    }

    inject() {
        local n="$1" token
        token="$(login "voter-${n}")"
        seed_vote "$n" $((n % 3 + 1)) "$token"
    }
    export -f inject

    pending_total() {
        request GET /api/v1/audit/counts
        [[ "$STATUS" == 200 ]] || return 1
        { grep -o '"pending":[0-9]*' <<<"$BODY" || true; } | cut -d: -f2 | paste -sd+ | bc_sum
    }
    participation_total() {
        request GET /api/v1/audit/counts
        [[ "$STATUS" == 200 ]] || return 1
        { grep -o '"participation":[0-9]*' <<<"$BODY" || true; } | cut -d: -f2 | paste -sd+ | bc_sum
    }
    leased_shards() {
        { grep -a 'リースを取得しました' "$1" | grep -ao 'shard=[0-9]*' | cut -d= -f2 || true; } | sort -un | tr '\n' ' '
    }

    echo "== 1. DB の起動と、専用キースペース（${KS}）へのスキーマ投入（冪等性は chain#4 で確認済みのため 1 回だけ）"
    db_ensure
    ks_create skip
    echo "スキーマ投入: OK"

    # -------------------------------------------------------------------
    echo "== 2. sealer を 2 プロセス起動 → 4 シャードのリースを取得"
    spawn sealer-a "$(mktemp)" env APP__SEALER__ID=sealer-a ./target/debug/sealer
    spawn sealer-b "$(mktemp)" env APP__SEALER__ID=sealer-b ./target/debug/sealer

    all_leased() {
        alive sealer-a && alive sealer-b || return 1
        local all
        all="$(echo "$(leased_shards "$(log_of sealer-a)") $(leased_shards "$(log_of sealer-b)")" | tr ' ' '\n' | grep -c '[0-9]' || true)"
        [[ "$all" -eq 4 ]]
    }
    wait_until 60000 all_leased || fail "4 シャードのリースが 60 秒以内に取得されません"
    SHARDS_A="$(leased_shards "$(log_of sealer-a)")"
    SHARDS_B="$(leased_shards "$(log_of sealer-b)")"
    echo "sealer-a: シャード ${SHARDS_A:-なし}/ sealer-b: シャード ${SHARDS_B:-なし}"
    dup="$(echo "$SHARDS_A $SHARDS_B" | tr ' ' '\n' | grep '[0-9]' | sort | uniq -d || true)"
    [[ -z "$dup" ]] || fail "同じシャードのリースを 2 つの sealer が取得しています: ${dup}"
    [[ -n "$SHARDS_A" && -n "$SHARDS_B" ]] || fail "リースが片方の sealer に偏っています（両方が担当を持つはず）"

    echo "== 3. api（app.mode=db）を起動 → 全シャードのチェーンが読める"
    spawn api "$(mktemp)" env "APP__API__PORT=${PORT}" ./target/debug/api
    api_ready() {
        alive api || fail "api が起動直後に終了しました"
        curl -fsS -o /dev/null "${BASE}/healthz" 2>/dev/null || return 1
        local s
        for s in 0 1 2 3; do
            [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/${s}/head")" == 200 ]] || return 1
        done
    }
    wait_until 30000 api_ready || fail "api が応答しない、または全シャードのチェーン（ジェネシス）が読めません"
    wait_election_open "$BASE" 15 || fail "自動で open になりませんでした"
    echo "api: OK（4 シャードのジェネシス）"

    # -------------------------------------------------------------------
    echo "== 4. 3 種類の投票用紙に 1000 票を投入"
    seq 1 1000 | xargs -P 50 -I{} bash -c 'inject {}' >"$CODES"
    [[ "$(wc -l <"$CODES" | tr -d ' ')" == 1000 ]] || fail "投票の応答が 1000 件ではありません"
    [[ "$({ grep -vc '^201$' "$CODES" || true; })" == 0 ]] || fail "201 以外の応答があります: $(sort "$CODES" | uniq -c | tr '\n' ' ')"
    echo "投入完了: 1000 件すべて 201"

    VICTIM_NAME="sealer-a"
    VICTIM_SHARDS="$SHARDS_A"
    request GET /debug/pool
    [[ "$STATUS" == 200 ]] || fail "/debug/pool が取得できません"
    victim_pending=0
    for s in $VICTIM_SHARDS; do
        n="$({ grep -o "\"shard\":${s},\"pending\":[0-9]*" <<<"$BODY" || true; } | cut -d: -f3)"
        victim_pending=$((victim_pending + ${n:-0}))
    done
    echo "kill 直前: 全体の未封印 $(pending_total) 件（うち ${VICTIM_NAME} のシャード [${VICTIM_SHARDS}] は ${victim_pending} 件）"
    [[ "$victim_pending" -gt 0 ]] || fail "kill 時点で ${VICTIM_NAME} のシャードに未封印の票がありません（引き継ぎを確認できません）"

    echo "== 5. ${VICTIM_NAME} を kill -9 → 残った sealer（sealer-b）がリースを引き継ぐ"
    hard_stop sealer-a
    KILLED_AT="$(now_ms)"
    took_over() {
        alive sealer-b || fail "生き残るはずの sealer が終了しました"
        local s
        for s in $VICTIM_SHARDS; do
            grep -aq "リースを取得しました shard=${s} " "$(log_of sealer-b)" || return 1
        done
    }
    wait_until 60000 took_over || fail "残った sealer が、落ちた sealer のシャード [${VICTIM_SHARDS}] のリースを 60 秒以内に引き継ぎません"
    echo "引き継ぎ: OK（kill から $(($(now_ms) - KILLED_AT)) ms で、シャード [${VICTIM_SHARDS}] を取得）"

    echo "== 6. 全票が封印されるまで待つ"
    all_sealed() {
        [[ "$(pending_total)" == 0 ]] && [[ "$(participation_total)" == 1000 ]]
    }
    wait_until 90000 all_sealed || fail "90 秒以内に全票が封印されません（未封印: $(pending_total) 件）"
    echo "全票封印: OK（kill から $(($(now_ms) - KILLED_AT)) ms）"
    took_seal=0
    for s in $VICTIM_SHARDS; do
        grep -aq "ブロックを封印しました shard=${s} " "$(log_of sealer-b)" && took_seal=1
    done
    [[ "$took_seal" -eq 1 ]] || fail "残った sealer が、引き継いだシャードの票を封印していません"
    echo "引き継いだシャードの封印: OK"

    # -------------------------------------------------------------------
    echo "== 7. 分岐なし: 同じ高さに 2 つのブロックが存在しない"
    seals="$(cat "$(log_of sealer-a)" "$(log_of sealer-b)" 2>/dev/null | { grep -ao 'ブロックを封印しました shard=[0-9]* height=[0-9]*' || true; } \
        | sed 's/ブロックを封印しました //' | sort)"
    dup_seals="$(uniq -d <<<"$seals")"
    [[ -z "$dup_seals" ]] || fail "同じ (shard, height) が複数回封印されています: ${dup_seals}"
    echo "封印ログ: $(wc -l <<<"$seals" | tr -d ' ') 件、同じ (shard, height) の重複なし"
    rows="$(ks_cql "SELECT shard, height FROM ${KS}.blocks" 2>/dev/null \
        | grep -E '^\s*[0-9]+\s*\|\s*[0-9]+\s*$' | tr -d ' ' || true)"
    [[ -n "$rows" ]] || fail "DB からブロックの一覧を取得できません"
    for s in 0 1 2 3; do
        count="$({ grep -c "^${s}|" <<<"$rows" || true; })"
        max="$({ grep "^${s}|" <<<"$rows" | cut -d'|' -f2 | sort -n | tail -1; } || true)"
        [[ "$count" -eq $((max + 1)) ]] || fail "シャード ${s}: ブロックが ${count} 件、最大の高さ ${max}（欠番または重複）"
        dups="$(grep "^${s}|" <<<"$rows" | sort | uniq -d)"
        [[ -z "$dups" ]] || fail "シャード ${s}: 同じ高さのブロックが複数あります: ${dups}"
    done
    echo "DB 上のブロック: 全シャードで高さが連続し、重複なし"

    echo "== 8. アンカー: 全票が封印された後のアンカーが作られるのを待つ"
    anchor_matches_heads() {
        request GET /api/v1/anchors/latest
        [[ "$STATUS" == 200 ]] || return 1
        local s a h
        for s in 0 1 2 3; do
            a="$({ grep -o "\"shard\":${s},\"height\":[0-9]*" <<<"$BODY" || true; } | cut -d: -f3)"
            h="$(curl -sS "${BASE}/api/v1/chains/${s}/head" | sed -n 's/.*"height":\([0-9]*\).*/\1/p')"
            [[ -n "$a" && "$a" == "$h" ]] || return 1
        done
    }
    wait_until 60000 anchor_matches_heads || fail "全シャードの最終の head を含むアンカーが 60 秒以内に作られません"
    echo "アンカー: OK（最新のアンカーが全シャードの最終の head を含む）"
    anchors_logged="$(cat "$(log_of sealer-a)" "$(log_of sealer-b)" 2>/dev/null | { grep -ac 'アンカーを作成しました' || true; })"
    [[ "$anchors_logged" -ge 1 ]] || fail "アンカーがログに出力されていません"

    # 変化がなければ、アンカーは作られない。
    request GET /api/v1/anchors/latest
    seq_before="$(sed -n 's/.*"seq":\([0-9]*\).*/\1/p' <<<"$BODY")"
    [[ -n "$seq_before" ]] || fail "最新のアンカーの seq を取得できません: ${BODY}"
    sleep 25
    request GET /api/v1/anchors/latest
    seq_after="$(sed -n 's/.*"seq":\([0-9]*\).*/\1/p' <<<"$BODY")"
    [[ "$seq_after" == "$seq_before" ]] || fail "変化がないのにアンカーが増えました（seq ${seq_before} → ${seq_after}）"
    echo "アンカー: 変化がない 25 秒間（間隔 10 秒）で増えない（seq=${seq_after}）: OK"

    echo "== 9. verify（全シャード・投票用紙別の突合・ballot_id の重複・アンカー）"
    request GET /api/v1/chains/0/head
    PUBLIC_KEY="$(sed -n 's/.*"signer_public_key":"\([0-9a-f]*\)".*/\1/p' <<<"$BODY")"
    [[ ${#PUBLIC_KEY} -eq 64 ]] || fail "公開鍵を取得できません: ${BODY}"
    set +e
    VERIFY_OUT="$(./target/debug/verifier verify --api "$BASE" --public-key "$PUBLIC_KEY" 2>&1)"
    VERIFY_RC=$?
    set -e
    echo "$VERIFY_OUT"
    [[ "$VERIFY_RC" -eq 0 ]] || fail "verify が exit ${VERIFY_RC} でした"
    [[ "$VERIFY_OUT" == *'検証 OK: 4 シャード'* ]] || fail "4 シャードすべてが検証されていません"
    [[ "$VERIFY_OUT" == *'1000 票'* ]] || fail "チェーン内の票数が 1000 ではありません"
    expected_ballots="$(for n in $(seq 1 1000); do seed_district "$n" $((n % 3 + 1)); done | sort -u | wc -l | tr -d ' ')"
    rows_ok="$({ grep -Ec 'contest=.* pending=0 OK' <<<"$VERIFY_OUT" || true; })"
    rows_ng="$({ grep -Ec 'contest=.* NG' <<<"$VERIFY_OUT" || true; })"
    [[ "$rows_ok" == "$expected_ballots" && "$rows_ng" == 0 ]] \
        || fail "投票用紙別の突合が想定と異なります（OK ${rows_ok} 行、NG ${rows_ng} 行。期待: OK ${expected_ballots} 行）"
    participation_sum="$({ grep -o 'participation=[0-9]*' <<<"$VERIFY_OUT" || true; } | cut -d= -f2 | paste -sd+ | bc_sum)"
    [[ "$participation_sum" == 1000 ]] || fail "participation の合計が 1000 ではありません: ${participation_sum}"
    [[ "$VERIFY_OUT" == *"${expected_ballots} 枚の"*"を突合"* ]] || fail "突合した投票用紙の枚数が ${expected_ballots} ではありません"
    [[ "$VERIFY_OUT" == *'ballot_id の重複: なし OK'* ]] || fail "ballot_id の重複の確認が OK ではありません"
    grep -Eq 'アンカー: seq=[0-9]+ shards=4 OK' <<<"$VERIFY_OUT" || fail "アンカーの検証が OK ではありません"

    echo "== 10. 正常停止（SIGTERM: api → sealer）: フラッシュせずに（原則9）リースを解放"
    graceful_stop api
    graceful_stop sealer-b
    released="$({ grep -ac 'リースを解放しました' "$(log_of sealer-b)" || true; })"
    [[ "$released" -ge 1 ]] || fail "正常停止時にリースが解放されていません"
    echo "正常停止: OK（リース解放 ${released} 件）"

    if cat "$(log_of sealer-a)" "$(log_of sealer-b)" 2>/dev/null | grep -aEq 'voter-[0-9]'; then
        fail "ログに投票者 ID が出力されています"
    fi
    if cat "$(log_of sealer-a)" "$(log_of sealer-b)" 2>/dev/null | grep -aEqi 'USE <keyspace>|with prepared statements'; then
        fail "ログに「USE <keyspace> with prepared statements」の警告が出ています"
    fi
    echo "OK: chain#5 複数 sealer のリース引き継ぎ（DB_BACKEND=${DB_BACKEND}）"
)

# ===========================================================================
# 6. verifier tally（DB。旧 check_step13.sh）
# ===========================================================================
check_tally() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18805}"
    ADMIN_PORT="${CHECK_ADMIN_PORT:-18905}"
    BASE="http://127.0.0.1:${PORT}"
    ADMIN_BASE="http://127.0.0.1:${ADMIN_PORT}"
    ADMIN_TOKEN="chain6-check-admin-token-0123456789abcdef"
    export BASE

    TMP="$(mktemp -d)"
    OUT="$TMP/out"
    mkdir -p "$OUT"
    API_LOG="$TMP/api.log"
    SEALER_LOG="$TMP/sealer.log"

    export APP__APP__MODE=db
    # db_setup_vars（cfg_init）が app.env=test にしているが、--allow-interim は app.env=dev のときだけ
    # 使えるので、この確認だけは dev にする（原則17。他のスイートの分離とは無関係にここで上書きする）。
    export APP__APP__ENV=dev
    export APP__DB__NODES="127.0.0.1:${DB_PORT}"
    export APP__SHARD__COUNT=2
    export APP__SEAL__MAX_BALLOTS=1000
    # 時間による封印はさせない（投入中に時間で封印されると、未封印の確認（3節）が不安定になる）。
    # 未封印 0 件は、closing の自動フラッシュ（4節。投票終了の手続き）で作る。
    export APP__SEAL__INTERVAL_SECS=600
    export APP__SEALER__LEASE_TTL_SECS=6
    export APP__SEALER__SIGNING_SEED="0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d"
    export APP__SESSION__SECRET="chain6-check-secret-0123456789abcdef"
    export APP__API__PORT="$PORT"
    export APP__ADMIN__BIND="127.0.0.1:${ADMIN_PORT}"
    export APP__ADMIN__TOKEN="$ADMIN_TOKEN"
    # 開始時刻を過去にして起動直後に自動で open にする。締切の手続きの猶予（== closing の間の長さ）は、
    # election-status の短期キャッシュ（state_cache_secs）が入れ替わるのを待ってから closing 中の
    # tally を試すだけの余裕（state_cache_secs の待ち + tally 呼び出し）を持たせる（原則17・18）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export APP__ELECTION__STATE_CACHE_SECS=2
    export APP__API__REQUEST_TIMEOUT_SECS=8
    export RUST_LOG="info,sealer=debug,tower_http=warn"

    ks_init chain6

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

    RC=0
    TALLY_OUT=""
    run_tally() {
        RC=0
        TALLY_OUT="$(./scripts/tally.sh --out "$OUT" "$@" 2>&1)" || RC=$?
    }
    # find は $OUT が無いと非 0 を返す（pipefail で拾われ、set -e で無言のまま落ちる）。
    # tally が一度も成功していない（$OUT がまだ無い）呼び出し方をするので、常に 0 件として扱う。
    out_dirs() { { find "$OUT" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '; } || true; }
    count() { ks_cql "SELECT count(*) FROM ${KS}.$1" 2>/dev/null | grep -E '^\s*[0-9]+\s*$' | tr -d ' ' | head -1; }
    admin_phase() { curl -sS -H "Authorization: Bearer ${ADMIN_TOKEN}" "${ADMIN_BASE}/admin/v1/election" 2>/dev/null; }
    close_election() {
        curl -sS -o /dev/null -X POST -H "Authorization: Bearer ${ADMIN_TOKEN}" "${ADMIN_BASE}/admin/v1/election/close"
    }

    echo "== 1. 前提と準備"
    db_ensure
    ks_create skip
    VOTERS=24
    seed_generate "$TMP/seed" "$VOTERS"
    SEED_DIR_ELECTION="$TMP/seed/$SEED_ID"
    echo "専用キースペース: ${KS}、有権者 ${VOTERS} 人（voter-1〜voter-${VOTERS}）"

    spawn sealer "$SEALER_LOG" env APP__SEALER__ID=chain6-sealer ./target/debug/sealer
    spawn api "$API_LOG" ./target/debug/api
    wait_healthz "$BASE" 20 api || fail "api が応答しません"
    for shard in 0 1; do
        for _ in $(seq 1 200); do
            [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/${shard}/head")" == 200 ]] && continue 2
            sleep 0.2
        done
        fail "sealer がシャード ${shard} のチェーンを用意しません"
    done
    wait_election_open "$BASE" 15 || fail "自動で open になりませんでした"

    echo "== 2. 決まった投票を投入する（有権者 n が、表示順 1 番目と 2 番目の投票用紙に、候補者 c(n%4+1) へ）"
    declare -A EXP_CAND=() EXP_DIST=() EXP_TYPE=() EXP_PREF=()
    TOTAL_VOTES=0
    for n in $(seq 1 "$VOTERS"); do
        token="$(login "voter-${n}")"
        [[ -n "$token" ]] || fail "voter-${n} がログインできません"
        for k in 1 2; do
            code="$(seed_vote "$n" "$k" "$token")"
            [[ "$code" == 201 ]] || fail "voter-${n} の ${k} 枚目の投票が 201 ではありません（${code}）"
            d="$(seed_district "$n" "$k")"
            EXP_CAND["${d}.c$((n % 4 + 1))"]=$((${EXP_CAND["${d}.c$((n % 4 + 1))"]:-0} + 1))
            EXP_DIST["$d"]=$((${EXP_DIST["$d"]:-0} + 1))
            EXP_TYPE["${d%%.*}"]=$((${EXP_TYPE["${d%%.*}"]:-0} + 1))
            prefs="$(awk -F, -v d="$d" '$1 == d { print $4 }' "$SEED_DIR_ELECTION/districts.csv")"
            [[ -n "$prefs" ]] || fail "districts.csv に ${d} がありません"
            if [[ "$prefs" == *";"* ]]; then key="$d"; else key="$prefs"; fi
            EXP_PREF["$key"]=$((${EXP_PREF["$key"]:-0} + 1))
            TOTAL_VOTES=$((TOTAL_VOTES + 1))
        done
    done
    [[ "$TOTAL_VOTES" == $((VOTERS * 2)) ]] || fail "投入した票数が想定と違います（${TOTAL_VOTES}）"
    [[ "$(count participation)" == "$TOTAL_VOTES" ]] || fail "participation が ${TOTAL_VOTES} 件ではありません"
    echo "投入: ${TOTAL_VOTES} 票（候補者別の期待値 ${#EXP_CAND[@]} 通り、選挙区 ${#EXP_DIST[@]} 枚）"

    echo "== 3. 未封印の票が残っていると、件数を表示して中止する（--allow-interim があっても）"
    run_tally
    [[ "$RC" == 4 ]] || fail "未封印の票があるのに、終了コード 4 ではありません（${RC}）: ${TALLY_OUT}"
    [[ "$TALLY_OUT" == *"未封印の票が ${TOTAL_VOTES} 件"* ]] || fail "未封印の件数が表示されていません: ${TALLY_OUT}"
    [[ "$TALLY_OUT" == *"投票終了の手続き"* && "$TALLY_OUT" == *"close --now"* ]] || fail "投票終了の手続きの案内がありません: ${TALLY_OUT}"
    [[ "$TALLY_OUT" == *"検証 OK"* ]] || fail "未封印の確認の前に、検証と突合が行われていません: ${TALLY_OUT}"
    [[ "$(out_dirs)" == 0 ]] || fail "中止したのに、出力ディレクトリができています"
    run_tally --allow-interim
    [[ "$RC" == 4 && "$TALLY_OUT" == *"未封印の票が"* ]] || fail "--allow-interim でも、未封印の票があれば中止するはずです（${RC}）"
    echo "未封印 ${TOTAL_VOTES} 件: 検証・突合の後に中止（終了コード 4）・--allow-interim でも中止・何も出力しない: OK"

    echo "== 4. close --now（open -> closing）: 全シャードが直ちにフラッシュされ、選挙状態が closed になる前は --allow-interim がないと拒否される"
    close_election
    for _ in $(seq 1 50); do
        [[ "$(admin_phase)" == *'"phase":"closing"'* ]] && break
        sleep 0.1
    done
    [[ "$(admin_phase)" == *'"phase":"closing"'* ]] || fail "close --now の後、closing になりません: $(admin_phase)"
    for _ in $(seq 1 50); do
        [[ "$(count ballot_pool)" == 0 ]] && break
        sleep 0.1
    done
    [[ "$(count ballot_pool)" == 0 ]] || fail "closing を検知した後も、未封印の票が残っています（$(count ballot_pool)）"
    echo "closing: 直ちに全シャードがフラッシュされる OK"

    # 公開用の election-status は election.state_cache_secs だけ短期キャッシュする（原則17・18）ので、
    # admin（キャッシュを経由しない）で closing を確認できても、tally（公開 API 経由）が見る状態は、
    # キャッシュが効いている間は open のままのことがある。キャッシュが確実に入れ替わるまで待つ。
    sleep "$((APP__ELECTION__STATE_CACHE_SECS + 1))"
    run_tally
    [[ "$RC" == 4 && "$TALLY_OUT" == *"--allow-interim"* && "$TALLY_OUT" == *"closing"* ]] \
        || fail "closing 中が、終了コード 4 で拒否されません（${RC}）: ${TALLY_OUT}"
    before="$(out_dirs)"
    run_tally --allow-interim
    [[ "$RC" == 0 && "$TALLY_OUT" == *"中間集計"* ]] || fail "--allow-interim で中間集計できません（${RC}）: ${TALLY_OUT}"
    [[ "$(out_dirs)" == $((before + 1)) ]] || fail "中間集計の出力ディレクトリができていません"
    echo "closing 中: 拒否（終了コード 4）・--allow-interim なら中間集計: OK"

    echo "== 5. 締切の手続きの猶予が経つと closed になる → 締切後の tally が、期待値と一致する"
    for _ in $(seq 1 100); do
        [[ "$(admin_phase)" == *'"phase":"closed"'* ]] && break
        sleep 0.2
    done
    [[ "$(admin_phase)" == *'"phase":"closed"'* ]] || fail "自動で closed になりませんでした: $(admin_phase)"
    echo "自動遷移: closing -> closed OK"

    run_tally
    [[ "$RC" == 0 ]] || fail "closed 後の tally が終了コード 0 ではありません（${RC}）: ${TALLY_OUT}"
    [[ "$TALLY_OUT" != *"中間集計"* ]] || fail "closed 後の集計が、中間集計と表示されています: ${TALLY_OUT}"
    echo "$TALLY_OUT" | sed -n '1,20p'
    DIR="$(find "$OUT" -mindepth 1 -maxdepth 1 -type d | sort | tail -1)"
    [[ "$(out_dirs)" -ge 1 && -n "$DIR" ]] || fail "出力ディレクトリができていません"
    for f in districts.csv candidates.csv prefectures.csv types.csv reconciliation.csv tally.json; do
        [[ -s "$DIR/$f" ]] || fail "$f がありません"
    done
    checked=0
    while IFS=, read -r _contest district _name rank cand _cname _party votes; do
        want="${EXP_CAND["$cand"]:-0}"
        [[ "$votes" == "$want" ]] || fail "候補者 ${cand} の得票が ${votes} です（期待 ${want}）"
        checked=$((checked + 1))
    done < <(tail -n +2 "$DIR/candidates.csv")
    [[ "$checked" -ge "${#EXP_CAND[@]}" ]] || fail "candidates.csv の行数が足りません（${checked}）"
    sum_votes="$(tail -n +2 "$DIR/candidates.csv" | awk -F, '{ s += $8 } END { print s + 0 }')"
    [[ "$sum_votes" == "$TOTAL_VOTES" ]] || fail "候補者別の得票の合計が ${sum_votes} です（期待 ${TOTAL_VOTES}）"
    bad_order="$(tail -n +2 "$DIR/candidates.csv" | awk -F, '
        { if ($2 == prev_d && $8 > prev_v) bad++
          prev_d = $2; prev_v = $8 }
        END { print bad + 0 }')"
    [[ "$bad_order" == 0 ]] || fail "candidates.csv が、選挙区内で得票の多い順に並んでいません"
    echo "候補者別の得票: ${checked} 行が期待値と一致・合計 ${sum_votes} 票・選挙区内は得票の多い順: OK"
    while IFS=, read -r _contest district _name _type _prefs valid blank total voted; do
        want="${EXP_DIST["$district"]:-0}"
        [[ "$blank" == 0 ]] || fail "${district}: 白票が ${blank} です（期待 0）"
        [[ "$total" == "$want" && "$valid" == "$want" && "$voted" == "$want" ]] \
            || fail "${district}: 有効票=${valid} 白票=${blank} 合計=${total} 投票済み者数=${voted}（期待 ${want}）"
    done < <(tail -n +2 "$DIR/districts.csv")
    echo "選挙区別: 有効票 = 合計 = 投票済み者数 = 期待値・白票 0: OK"
    while IFS=, read -r key _name _wide _n _valid _blank total _voted; do
        want="${EXP_PREF["$key"]:-0}"
        [[ "$total" == "$want" ]] || fail "都道府県別 ${key}: 合計 ${total}（期待 ${want}）"
    done < <(tail -n +2 "$DIR/prefectures.csv")
    while IFS=, read -r key _name _wide _n _valid _blank total voted; do
        want="${EXP_TYPE["$key"]:-0}"
        [[ "$total" == "$want" && "$voted" == "$want" ]] || fail "選挙の種類別 ${key}: 合計 ${total} 投票済み ${voted}（期待 ${want}）"
    done < <(tail -n +2 "$DIR/types.csv")
    echo "都道府県別（${#EXP_PREF[@]} 行）・選挙の種類別（${#EXP_TYPE[@]} 種類）: 期待値と一致 OK"
    [[ "$(tail -n +2 "$DIR/reconciliation.csv" | awk -F, '$4 != 0 || $5 != "true" || $2 != $3 { n++ } END { print n + 0 }')" == 0 ]] \
        || fail "reconciliation.csv に、不一致・未封印が残っています"
    grep -q '"interim": false' "$DIR/tally.json" || fail "tally.json の interim が false ではありません"
    grep -q '"duplicate_ballots": 0' "$DIR/tally.json" || fail "tally.json に突合の結果（重複 0）がありません"
    echo "突合の結果・JSON（interim=false・重複 0）: OK"

    echo "== 6. 封印済みのブロックを改ざんすると、集計が拒否される"
    row="$(ks_cql "SELECT shard, height, ballot_count FROM ${KS}.blocks" 2>/dev/null | awk -F'|' 'NF == 3 && $3 + 0 > 0 { gsub(/ /, "", $1); gsub(/ /, "", $2); print $1 " " $2; exit }')"
    [[ -n "$row" ]] || fail "改ざんできる（票のある）ブロックがありません"
    read -r t_shard t_height <<<"$row"
    tuple="$(ks_cql "SELECT ballots FROM ${KS}.blocks WHERE shard = ${t_shard} AND height = ${t_height}" 2>/dev/null \
        | grep -oE "\(0x[0-9a-f]+, '[^']+', '[^']+'\)" | head -1)"
    [[ -n "$tuple" ]] || fail "改ざん対象の票を取得できません"
    t_id="$(sed -E "s/^\(0x([0-9a-f]+), .*/\1/" <<<"$tuple")"
    t_contest="$(sed -E "s/^\(0x[0-9a-f]+, '([^']+)', '([^']+)'\)$/\1/" <<<"$tuple")"
    t_cand="$(sed -E "s/^\(0x[0-9a-f]+, '([^']+)', '([^']+)'\)$/\2/" <<<"$tuple")"
    if [[ "$t_cand" == *.c1 ]]; then new_cand="${t_cand%.c1}.c2"; else new_cand="${t_cand%.c*}.c1"; fi
    ks_cql "UPDATE ${KS}.blocks SET ballots[0] = (0x${t_id}, '${t_contest}', '${new_cand}') WHERE shard = ${t_shard} AND height = ${t_height}" >/dev/null 2>&1 \
        || fail "ブロックを改ざんできませんでした"
    before="$(out_dirs)"
    sleep 1
    run_tally
    [[ "$RC" == 3 ]] || fail "改ざんされたチェーンの集計が、終了コード 3 で拒否されません（${RC}）: ${TALLY_OUT}"
    [[ "$TALLY_OUT" == *"集計を中止しました"* && "$TALLY_OUT" == *"検証 NG"* ]] || fail "拒否の表示がありません: ${TALLY_OUT}"
    [[ "$(out_dirs)" == "$before" ]] || fail "拒否したのに、出力ディレクトリが増えています"
    echo "改ざん: 検証 NG → 集計を中止（終了コード 3）・何も出力しない: OK"

    echo "OK: chain#6 tally（DB_BACKEND=${DB_BACKEND}）"
)

# ===========================================================================
# 7. ブロックチェーンのビューア API（memory。旧 check_step14.sh）
# ===========================================================================
check_viewer_api() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18806}"
    BASE="http://127.0.0.1:${PORT}"
    export BASE

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
        [[ -s "$LOG" ]] && tail -n 20 "$LOG" >&2
        exit 1
    }

    close_at() {
        date -u -d "+$1 seconds" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v+"$1"S +%Y-%m-%dT%H:%M:%SZ
    }
    CLOSE_SECS=40

    export APP__SESSION__SECRET="chain7-check-secret-0123456789abcdef"
    export APP__SHARD__COUNT=1
    export APP__SEAL__MAX_BALLOTS=3
    export APP__SEAL__INTERVAL_SECS=10
    export APP__API__PORT="$PORT"
    export APP__CHAIN__REVEAL_BALLOTS=after_close
    # 選挙状態（原則17・18）の開始時刻は過去にして、起動直後に自動で open にする。このスイートが見る
    # 締切（voting_closes_at）は、chain.reveal_ballots=after_close の公開判定にだけ使う（別の機構）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,tower_http=warn"

    http() {
        STATUS="$(curl -sS -D "$TMP/headers" -o "$TMP/body" -w '%{http_code}' "${BASE}$1")"
        CC="$({ grep -i '^cache-control:' "$TMP/headers" || true; } | sed 's/^[^:]*: *//' | tr -d '\r')"
        BODY="$(cat "$TMP/body")"
    }
    heights_of() { { grep -o '"height":[0-9]*' <<<"$1" || true; } | cut -d: -f2 | paste -sd' ' -; }
    IMMUTABLE="public, max-age=31536000, immutable"

    echo "== 1. 準備と起動（締切は ${CLOSE_SECS} 秒後）"
    SEED_TMP="$TMP/seed"
    seed_generate "$SEED_TMP" 30
    CLOSE_EPOCH=$(($(date +%s) + CLOSE_SECS))
    export APP__ELECTION__VOTING_CLOSES_AT="$(close_at "$CLOSE_SECS")"
    spawn api "$LOG" ./target/debug/api
    wait_healthz "$BASE" 10 api || fail "api が応答しません"

    echo "== 2. 30 票を投入する（3 票ごとに封印: ジェネシス + 10 ブロック）"
    for n in $(seq 1 30); do
        token="$(login "voter-${n}")"
        [[ -n "$token" ]] || fail "voter-${n} がログインできません"
        code="$(seed_vote "$n" 1 "$token")"
        [[ "$code" == 201 ]] || fail "voter-${n} の投票が 201 ではありません（${code}）"
    done
    for _ in $(seq 1 100); do
        http "/api/v1/chains/0/blocks?limit=1"
        [[ "$(heights_of "$BODY")" == 10 ]] && break
        sleep 0.3
    done
    [[ "$(heights_of "$BODY")" == 10 ]] || fail "ブロックが高さ 10 まで封印されません（$(heights_of "$BODY")）"
    [[ "$(date +%s)" -lt "$CLOSE_EPOCH" ]] || fail "締切を過ぎてしまいました（マシンが遅い場合は CLOSE_SECS を延ばす）"
    echo "封印: 高さ 0〜10（11 ブロック）"

    echo "== 3. ブロックの一覧のページ送り（limit=4: 4 + 4 + 3）と Cache-Control"
    collected=""
    path="/api/v1/chains/0/blocks?limit=4"
    pages=0
    while :; do
        http "$path"
        [[ "$STATUS" == 200 ]] || fail "${path} が ${STATUS} です"
        if [[ "$pages" == 0 ]]; then
            [[ "$CC" == "no-cache" ]] || fail "先頭からのページの Cache-Control が no-cache ではありません（${CC}）"
        else
            [[ "$CC" == "$IMMUTABLE" ]] || fail "before_height 指定のページの Cache-Control が immutable ではありません（${CC}）"
        fi
        [[ "$BODY" != *candidate_id* && "$BODY" != *ballot_id* ]] || fail "ブロックの一覧に票の中身が含まれています"
        collected="${collected:+$collected }$(heights_of "$BODY")"
        pages=$((pages + 1))
        next="$(sed -n 's/.*"next_before_height":\([0-9]*\).*/\1/p' <<<"$BODY")"
        [[ -n "$next" ]] || break
        [[ "$pages" -lt 10 ]] || fail "ページ送りが終わりません"
        path="/api/v1/chains/0/blocks?before_height=${next}&limit=4"
    done
    [[ "$collected" == "10 9 8 7 6 5 4 3 2 1 0" ]] || fail "ページ送りの結果が「10 … 0」ではありません（${collected}）"
    [[ "$pages" == 3 ]] || fail "ページ数が 3 ではありません（${pages}）"
    echo "ページ送り: ${pages} ページで「${collected}」（欠落・重複なし）: OK"
    http "/api/v1/chains/0/blocks?before_height=0"
    [[ "$STATUS" == 200 && -z "$(heights_of "$BODY")" && "$CC" == "$IMMUTABLE" ]] || fail "before_height=0 が、空で immutable になりません（${CC}）"
    for bad in "limit=abc" "before_height=-1"; do
        http "/api/v1/chains/0/blocks?${bad}"
        [[ "$STATUS" == 400 ]] || fail "${bad} が 400 ではありません（${STATUS}）"
    done
    echo "before_height の境界（0）・不正な値（400）: OK"

    echo "== 4. 締切前: 詳細 API に票の中身が含まれない"
    http "/api/v1/chains/0/blocks/5"
    [[ "$STATUS" == 200 ]] || fail "blocks/5 が ${STATUS} です"
    [[ "$BODY" == *'"ballots_revealed":false'* ]] || fail "ballots_revealed が false ではありません"
    for secret in candidate_id ballot_id contest_id shugiin_smd; do
        [[ "$BODY" != *"$secret"* ]] || fail "締切前の blocks/5 に「${secret}」が含まれています: ${BODY}"
    done
    [[ "$BODY" == *'"ballot_count":3'* ]] || fail "締切前でも、票数（ballot_count）は返るはずです: ${BODY}"
    [[ "$CC" == "no-store" ]] || fail "締切前の blocks/5 の Cache-Control が no-store ではありません（${CC}）"
    echo "締切前: 票の中身なし・ヘッダーと票数は返る・Cache-Control: no-store: OK"
    set +e
    out="$(./target/debug/verifier verify --api "$BASE" 2>&1)"
    rc=$?
    set -e
    [[ "$rc" == 4 && "$out" == *"締切後"* ]] || fail "締切前の verifier verify が、終了コード 4 ではありません（rc=${rc}）: ${out}"
    echo "締切前の verifier verify: 「票が非公開のため、締切後に実行」で終了コード 4: OK"

    echo "== 5. 締切後: 詳細 API に票の中身と表示名が含まれ、immutable"
    while [[ "$(date +%s)" -le "$CLOSE_EPOCH" ]]; do sleep 1; done
    http "/api/v1/chains/0/blocks/5"
    [[ "$STATUS" == 200 && "$BODY" == *'"ballots_revealed":true'* ]] || fail "締切後も、票が公開されません: ${BODY}"
    [[ "$BODY" == *'"candidate_id":"shugiin_smd.'* && "$BODY" == *'"ballot_id":"'* ]] || fail "締切後の blocks/5 に票がありません: ${BODY}"
    [[ "$BODY" == *'"candidate_name":"'* && "$BODY" == *'"district_name":"'* ]] || fail "締切後の blocks/5 に、表示名がありません: ${BODY}"
    [[ "$CC" == "$IMMUTABLE" ]] || fail "締切後の blocks/5 の Cache-Control が immutable ではありません（${CC}）"
    echo "締切後: candidate_id・ballot_id・表示名あり・Cache-Control: ${CC}: OK"
    ./target/debug/verifier verify --api "$BASE" >"$TMP/verify.out" 2>&1 || fail "締切後の verifier verify が失敗しました: $(tail -3 "$TMP/verify.out")"
    grep -q '検証 OK' "$TMP/verify.out" || fail "verifier verify が「検証 OK」ではありません"
    echo "締切後の verifier verify: 検証 OK: OK"

    echo "== 6. シャードの一覧・アンカーの一覧・404"
    http "/api/v1/chains"
    [[ "$STATUS" == 200 && "$CC" == "no-cache" ]] || fail "/chains が 200 / no-cache ではありません（${STATUS} / ${CC}）"
    [[ "$BODY" == *'"shard":0'* && "$BODY" == *'"height":10'* ]] || fail "/chains に、シャード 0 と先頭（高さ 10）がありません: ${BODY}"
    for _ in $(seq 1 60); do
        http "/api/v1/anchors?limit=5"
        [[ "$BODY" == *'"seq":'* ]] && break
        sleep 1
    done
    [[ "$STATUS" == 200 && "$BODY" == *'"seq":'* ]] || fail "アンカーの一覧が取得できません（${STATUS}）: ${BODY}"
    a_height="$(grep -o '"height":[0-9]*' <<<"$BODY" | head -1 | cut -d: -f2)"
    a_hash="$(grep -o '"block_hash":"[0-9a-f]*"' <<<"$BODY" | head -1 | cut -d'"' -f4)"
    http "/api/v1/chains/0/blocks/${a_height}"
    [[ "$STATUS" == 200 && "$BODY" == *"\"block_hash\":\"${a_hash}\""* ]] || fail "アンカーが指すブロック（高さ ${a_height}）のハッシュが一致しません"
    echo "/chains・/anchors（最新のアンカーの先頭 = 高さ ${a_height} のブロックへたどれる）: OK"
    for path in "/api/v1/chains/9/blocks" "/api/v1/chains/0/blocks/99"; do
        http "$path"
        [[ "$STATUS" == 404 && "$CC" == "no-store" ]] || fail "${path} が 404 / no-store ではありません（${STATUS} / ${CC}）"
    done
    echo "存在しないシャード・高さ: 404 / Cache-Control: no-store: OK"

    echo "OK: chain#7 ブロックチェーンのビューア API"
)

# ===========================================================================
# 8. 封印ルール（原則9。memory。ADR 0020）
# ===========================================================================
# dev の設定（config/dev.toml: seal.interval_secs=10・seal.min_ballots_after_interval=10）で、
#   9 票 → 20 秒待ってもブロックができない → 10 票目 → すぐに封印される（trigger=time）→
#   5 票 → scripts/election.sh close --now → 締切の手続きの中で trigger=close の 5 件のブロック
# を確認する。設定は、手元の config/local.toml の影響を受けないよう、config/dev.toml だけを一時ディレクトリに写して使う。
check_seal_rules() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18807}"
    ADMIN_PORT="${CHECK_ADMIN_PORT:-18907}"
    BASE="http://127.0.0.1:${PORT}"
    ADMIN_BASE="http://127.0.0.1:${ADMIN_PORT}"
    ADMIN_TOKEN="chain8-check-admin-token-0123456789abcdef"
    export BASE

    TMP="$(mktemp -d)"
    LOG="$TMP/api.log"
    mkdir -p "$TMP/config"
    cp config/dev.toml "$TMP/config/dev.toml"
    export APP_CONFIG_DIR="$TMP/config"
    export APP__APP__ENV=dev
    export APP__SESSION__SECRET="chain8-check-secret-0123456789abcdef"
    export APP__API__PORT="$PORT"
    export APP__ADMIN__BIND="127.0.0.1:${ADMIN_PORT}"
    export APP__ADMIN__TOKEN="$ADMIN_TOKEN"
    # 締切の手続きの待ち時間（state_cache_secs + request_timeout_secs）を短くして、確認を速くする。
    export APP__ELECTION__STATE_CACHE_SECS=1
    export APP__API__REQUEST_TIMEOUT_SECS=1
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,tower_http=warn"

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
            tail -n 30 "$LOG" >&2
        fi
        exit 1
    }

    vote_once() {
        local token code
        token="$(login "voter-$1")"
        [[ -n "$token" ]] || fail "ログインできません（voter-$1）"
        code="$(seed_vote "$1" 1 "$token")"
        [[ "$code" == 201 ]] || fail "投票が 201 ではありません（voter-$1: ${code}）"
    }
    chain_height() {
        curl -sS "${BASE}/api/v1/chains/0/head" | sed -n 's/.*"height":\([0-9]*\).*/\1/p'
    }
    seal_count() { { grep -ac 'ブロックを封印しました' "$LOG" || true; }; }
    admin_phase() { curl -sS -H "Authorization: Bearer ${ADMIN_TOKEN}" "${ADMIN_BASE}/admin/v1/election" 2>/dev/null || true; }
    phase_is() { [[ "$(admin_phase)" == *"\"phase\":\"$1\""* ]]; }

    echo "== 1. dev の設定で起動（seal.interval_secs=10・seal.min_ballots_after_interval=10・shard.count=1）"
    shown="$(./target/debug/app-config show)"
    grep -q '^interval_secs = 10  # .*dev.toml' <<<"$shown" || fail "dev の設定で seal.interval_secs=10 になっていません:
${shown}"
    grep -q '^min_ballots_after_interval = 10  # .*dev.toml' <<<"$shown" || fail "dev の設定で seal.min_ballots_after_interval=10 になっていません"
    SEED_TMP="$TMP/seed"
    seed_generate "$SEED_TMP" 30
    spawn api "$LOG" ./target/debug/api
    wait_healthz "$BASE" 10 api || fail "api が応答しません"
    wait_election_open "$BASE" 5 || fail "自動で open になりませんでした"
    [[ "$(chain_height)" == 0 ]] || fail "起動直後はジェネシス（高さ 0）のはずです"

    echo "== 2. 9 票 → 20 秒待ってもブロックができない（最小件数 10 に届かない。窓もリセットしない）"
    for n in $(seq 1 9); do vote_once "$n"; done
    sleep 20
    [[ "$(chain_height)" == 0 && "$(seal_count)" == 0 ]] || fail "9 票で封印されました（高さ=$(chain_height)）"
    echo "9 票・20 秒: ブロックなし OK"

    echo "== 3. 10 票目 → すぐに（間隔は経っているので）全 10 件を封印する"
    voted_at="$(now_ms)"
    vote_once 10
    wait_for_log 'shard=0 height=1 count=10 trigger=time' 2000 "$LOG" || fail "10 票目の後、2 秒以内に封印されません"
    echo "10 票目: $(($(now_ms) - voted_at)) ms で height=1（10 件, trigger=time）OK"

    echo "== 4. 5 票 → close --now → 締切の手続きの中で trigger=close の 5 件のブロック"
    for n in $(seq 11 15); do vote_once "$n"; done
    [[ "$(chain_height)" == 1 ]] || fail "5 票で封印されました（高さ=$(chain_height)）"
    ./scripts/election.sh close --now --yes >"$TMP/close.out" 2>&1 || { cat "$TMP/close.out" >&2; fail "close --now が失敗しました"; }
    wait_for_log 'shard=0 height=2 count=5 trigger=close' 10000 "$LOG" || fail "close --now の後、trigger=close で 5 件のブロックができません"
    wait_until 10000 phase_is closed || fail "締切の手続きが終わり、closed になりません: $(admin_phase)"
    [[ "$(admin_phase)" == *'"pending_by_shard":[0]'* ]] || fail "closed の時点で未封印が残っています: $(admin_phase)"
    [[ "$(seal_count)" == 2 ]] || fail "封印の回数が 2 ではありません（$(seal_count)）"
    echo "close --now: height=2（5 件, trigger=close）→ closed・未封印 0 件 OK"

    graceful_stop api
    echo "OK: chain#8 封印ルール（原則9）"
)

check_demo
check_inprocess_sealer
check_no_append_without_change
check_db_persistence
check_multi_sealer
check_tally
check_viewer_api
check_seal_rules

echo "OK: check/chain.sh"
