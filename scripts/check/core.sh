#!/usr/bin/env bash
# core スイート: 起動・設定・封印ポリシーの単体テスト・性能計測ツール（対応表: docs/testing.md）。
#   1. api の起動と GET /healthz
#   2. domain::seal_policy の単体テスト
#   3. 設定ファイル一式の整合（default / dev / production.example / .gitignore / Trunk.toml）
#   4. 設定ファイルの値を書き換えると動作が変わる
#   5. 環境変数が設定ファイルより優先される
#   6. 秘密情報は secrets/・環境変数から読める。設定ファイルには書けない。どこにも漏れない
#   7. 不正な設定では、api/sealer/verifier/bench が理由つきで起動に失敗する
#   8. labels.* がビルド時に web へ渡る（web は app-config に依存しない）
#   9. 性能計測ツール一式（scripts/bench.sh と crates/bench）
# 設定は、手元の config/local.toml などの影響を受けないよう分離する（scripts/lib/common.sh）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."
source scripts/lib/common.sh

# ===========================================================================
# 1. api の起動と healthz（旧 check_step0.sh）
# ===========================================================================
check_healthz() (
    set -euo pipefail
    cfg_init
    PORT="${CHECK_API_PORT:-18831}"
    LOG="$(mktemp)"
    cleanup() {
        common_cleanup
        hard_stop api
        rm -f "$LOG"
    }
    trap cleanup EXIT
    fail() {
        echo "FAIL: $1" >&2
        cat "$LOG" >&2
        exit 1
    }

    cargo build -p api
    spawn api "$LOG" env "APP__API__PORT=${PORT}" "APP__SESSION__SECRET=core1-check-secret-0123456789abcdef" ./target/debug/api

    url="http://127.0.0.1:${PORT}/healthz"
    ready=0
    for _ in $(seq 1 50); do
        alive api || fail "api が起動直後に終了しました"
        if curl -fsS -o /dev/null "$url" 2>/dev/null; then
            ready=1
            break
        fi
        sleep 0.2
    done
    [[ "$ready" -eq 1 ]] || fail "${url} が応答しません"

    status="$(curl -sS -o /dev/null -w '%{http_code}' "$url")"
    body="$(curl -sS "$url")"
    [[ "$status" == "200" ]] || fail "status=${status} (期待: 200)"
    [[ "$body" == '{"status":"ok"}' ]] || fail "body=${body} (期待: {\"status\":\"ok\"})"

    sleep 0.2
    grep -q "healthz" "$LOG" || fail "リクエストログが出力されていません"

    echo "OK: core#1 healthz"
)

# ===========================================================================
# 2. domain::seal_policy の単体テスト（旧 check_step2.sh）
# ===========================================================================
check_seal_policy() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    # 依頼された必須ケース（テスト名）。消す・改名すると失敗する。
    # 原則9（ADR 0020）: 時刻は引数で渡し、実際には待たない。
    REQUIRED=(
        case_99_ballots_at_9m59s_waits_and_100_ballots_seal_at_any_time
        case_250_ballots_seal_100_100_then_the_remaining_50_at_10_minutes
        case_9_ballots_at_10_minutes_wait_then_the_10th_at_12_minutes_seals_all_at_once
        case_exactly_10_ballots_at_exactly_10_minutes_seals_all
        case_0_ballots_at_10_minutes_wait_without_a_block_or_a_window_reset
        case_close_seals_3_as_one_block
        case_close_with_0_ballots_does_nothing
        case_close_splits_250_into_100_100_50
        case_the_voting_start_is_the_origin_of_the_elapsed_time
    )
    if ! out="$(cargo test -p domain --lib seal_policy:: 2>&1)"; then
        echo "$out" >&2
        fail "seal_policy のテストが失敗しました"
    fi
    passed="$(grep -Ec '^test seal_policy::.* \.\.\. ok$' <<<"$out" || true)"
    failed="$(grep -Ec '^test seal_policy::.* \.\.\. FAILED$' <<<"$out" || true)"
    for name in "${REQUIRED[@]}"; do
        grep -Eq "^test seal_policy::tests::${name} \.\.\. ok$" <<<"$out" \
            || fail "必須テスト ${name} が成功していません（存在しない可能性があります）"
    done
    [[ "$failed" -eq 0 ]] || fail "失敗したテストが ${failed} 件あります"
    [[ "$passed" -ge "${#REQUIRED[@]}" ]] || fail "成功件数 ${passed} が必須ケース数 ${#REQUIRED[@]} 未満です"
    echo "seal_policy tests: ${passed} passed, ${failed} failed（必須 ${#REQUIRED[@]} 件を含む）"
    echo "OK: core#2 seal_policy"
)

# ===========================================================================
# 3〜8. 設定（旧 check_step9.sh。旧項目 7 は docs.sh へ）
# ===========================================================================
check_config() (
    set -euo pipefail
    PORT="${CHECK_API_PORT:-18832}"
    BASE="http://127.0.0.1:${PORT}"
    SECRET="core3-check-secret-0123456789abcdef"
    # 秘密情報が漏れないことの確認に使う、目印になる値。
    MARK_SECRET="MARKER-session-secret-do-not-leak-0123"
    MARK_SEED="$(printf 'ab%.0s' {1..32})"
    SEED_A="$(printf '09%.0s' {1..32})"
    SEED_B="$(printf '0a%.0s' {1..32})"

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
            tail -n 20 "$LOG" >&2
        fi
        exit 1
    }

    # 手元の設定（環境変数 APP__…）を引き継がずに、指定した設定ディレクトリで実行する。
    # with_config CONFIG_DIR SECRETS_DIR [NAME=VALUE...] -- COMMAND...
    with_config() {
        local config_dir="$1" secrets_dir="$2"
        shift 2
        local pairs=() unset_args=() name
        for name in $(compgen -e | grep '^APP__' || true); do
            unset_args+=(-u "$name")
        done
        while [[ "$1" != "--" ]]; do
            pairs+=("$1")
            shift
        done
        shift
        # WITH_TIMEOUT=秒 なら、その時間で打ち切る（起動に失敗するはずのコマンドが、成功して居座るのを防ぐ）。
        local limit=()
        if [[ -n "${WITH_TIMEOUT:-}" ]]; then limit=(timeout "$WITH_TIMEOUT"); fi
        # WITH_EXEC=1 なら、このシェルを置き換えて実行する（バックグラウンド起動で、$! を対象のプロセスの PID にするため）。
        if [[ "${WITH_EXEC:-}" == 1 ]]; then
            exec "${limit[@]}" env "${unset_args[@]}" APP_CONFIG_DIR="$config_dir" APP_SECRETS_DIR="$secrets_dir" "${pairs[@]}" "$@"
        fi
        "${limit[@]}" env "${unset_args[@]}" APP_CONFIG_DIR="$config_dir" APP_SECRETS_DIR="$secrets_dir" "${pairs[@]}" "$@"
    }
    new_dir() {
        local dir
        dir="$(mktemp -d "$TMP/d.XXXXXX")"
        echo "$dir"
    }

    EMPTY_SECRETS="$(new_dir)"
    seed_generate "$TMP/seed" 20
    SEED_DIR="$TMP/seed"

    echo "== 0. ビルド"
    cargo build -q -p app-config
    cargo build -q -p api
    cargo build -q -p sealer
    cargo build -q -p verifier
    cargo build -q -p bench
    CFG="./target/debug/app-config"

    # -----------------------------------------------------------------------
    echo "== 3. app-config のテストと、設定ファイル一式"
    out="$(cargo test -p app-config 2>&1)" || { echo "$out" >&2; fail "app-config のテストが失敗しました"; }
    grep -E '^test result: ok\. [1-9][0-9]* passed' <<<"$out" | head -1

    for f in config/default.toml config/dev.toml config/production.example.toml; do
        [[ -s "$f" ]] || fail "$f がありません"
    done
    grep -qx '/config/local.toml' .gitignore || fail ".gitignore に /config/local.toml がありません"
    grep -qx '/secrets/' .gitignore || fail ".gitignore に /secrets/ がありません"
    [[ ! -e config/local.toml ]] || echo "注意: config/local.toml があります（このスイートは、手元の設定を使いません）"

    REQUIRED_KEYS=(
        app.env app.mode api.port api.request_timeout_secs web.port db.backend db.nodes db.keyspace
        seal.max_ballots seal.interval_secs seal.min_ballots_after_interval sealer.lease_ttl_secs shard.count
        auth.mode auth.argon2.memory_kib auth.argon2.iterations auth.argon2.parallelism session.ttl_secs
        credentials.output_file_enabled credentials.output_path credentials.password_length credentials.login_id_length
        election.seed_dir election.election_id election.voting_opens_at election.voting_closes_at
        election.display_timezone election.state_cache_secs
        chain.reveal_ballots admin.bind labels.site_title labels.done_message labels.login_heading
        labels.ballot_item labels.progress labels.voting_not_started_message labels.voting_closing_message
        labels.voting_closed_message
    )
    for key in "${REQUIRED_KEYS[@]}"; do
        with_config /nonexistent "$EMPTY_SECRETS" -- "$CFG" get "$key" >/dev/null 2>&1 \
            || fail "設定項目 ${key} を取り出せません（config/default.toml に無い?）"
    done
    echo "必須の設定項目 ${#REQUIRED_KEYS[@]} 件: OK"

    LC_ALL=C awk '
        /^[[:space:]]*#/ { comment = ($0 ~ /[^ -~]/); prev_ok = comment; next }
        /^[[:space:]]*[A-Za-z0-9_.]+[[:space:]]*=/ {
            if (!prev_ok) { printf "コメントの無い項目: 行 %d: %s\n", NR, $0; bad = 1 }
            next
        }
        { prev_ok = 0 }
        END { exit bad }
    ' config/default.toml || fail "config/default.toml の項目には、直前に日本語のコメントが必要です"
    echo "default.toml の全項目に日本語のコメント: OK"

    web_port="$(with_config /nonexistent "$EMPTY_SECRETS" -- "$CFG" get web.port)"
    api_port="$(with_config /nonexistent "$EMPTY_SECRETS" -- "$CFG" get api.port)"
    grep -Eq "^port = ${web_port}\$" crates/web/Trunk.toml || fail "crates/web/Trunk.toml の port が web.port（${web_port}）と一致しません"
    grep -Fq "backend = \"http://localhost:${api_port}/api/\"" crates/web/Trunk.toml \
        || fail "crates/web/Trunk.toml の proxy 先が api.port（${api_port}）と一致しません"
    echo "Trunk.toml と default.toml のポート（web=${web_port}, api=${api_port}）: 一致"

    with_config /nonexistent "$EMPTY_SECRETS" -- "$CFG" validate >/dev/null || fail "既定の設定が有効ではありません"

    # -----------------------------------------------------------------------
    BODY=""
    head_field() { sed -n "s/.*\"$1\":\\(\"\\?\\)\\([^\",}]*\\)\\1.*/\\2/p" <<<"$BODY"; }

    start_api() {
        local config_dir="$1" secrets_dir="$2"
        shift 2
        # 既定は開始時刻を過去にして、起動直後に自動で open にする（原則17・18。このスイートは投票の
        # 受付期間そのものは見ないので、後ろに同じ変数を渡せば上書きできる）。
        WITH_EXEC=1 with_config "$config_dir" "$secrets_dir" APP__API__PORT="$PORT" APP__ELECTION__SEED_DIR="$SEED_DIR" \
            APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00" RUST_LOG=info,sealer=debug "$@" -- ./target/debug/api >"$LOG" 2>&1 &
        SUITE_PIDS[api]=$!
        for _ in $(seq 1 100); do
            alive api || fail "api が起動直後に終了しました"
            curl -fsS -o /dev/null "${BASE}/healthz" 2>/dev/null && break
            sleep 0.1
        done
        alive api || fail "api の /healthz が応答しません"
        wait_election_open "$BASE" 5 || fail "自動で open になりませんでした（election.voting_opens_at の自動遷移）"
    }
    stop_api() { graceful_stop api; }

    vote_n() {
        local n="$1" token code
        token="$(curl -sS -X POST "${BASE}/api/v1/login" -H 'Content-Type: application/json' \
            -d "{\"voter_id\":\"voter-${n}\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
        [[ -n "$token" ]] || fail "ログインできません（voter-${n}）"
        code="$(seed_vote "$n" 1 "$token")"
        [[ "$code" == 201 ]] || fail "投票が 201 ではありません（voter-${n}: ${code}）"
    }
    read_head() { BODY="$(curl -sS "${BASE}/api/v1/chains/0/head")"; }

    # run_case DESC EXPECT_HEIGHT EXPECT_LAST_BALLOTS CONFIG_DIR SECRETS_DIR [NAME=VALUE...]
    run_case() {
        local desc="$1" want_height="$2" want_count="$3" config_dir="$4" secrets_dir="$5"
        shift 5
        start_api "$config_dir" "$secrets_dir" APP__SESSION__SECRET="$SECRET" "$@"
        for n in $(seq 1 12); do vote_n "$n"; done
        sleep 2.5
        read_head
        local height ballots
        height="$(head_field height)"
        ballots="$(head_field ballot_count)"
        [[ "$height" == "$want_height" ]] || fail "${desc}: 高さが ${want_height} のはずが ${height}（${BODY}）"
        if [[ "$want_height" -gt 0 ]]; then
            [[ "$ballots" == "$want_count" ]] || fail "${desc}: 先頭ブロックの票数が ${want_count} のはずが ${ballots}"
            local seals
            seals="$({ grep -ac 'trigger=count' "$LOG" || true; })"
            [[ "$seals" == "$want_height" ]] || fail "${desc}: trigger=count の封印が ${want_height} 回のはずが ${seals} 回"
        else
            if grep -aq 'ブロックを封印しました' "$LOG"; then fail "${desc}: 封印されないはずなのに封印されました"; fi
        fi
        if grep -aq "$SECRET" "$LOG"; then fail "${desc}: ログにセッション署名鍵が出力されています"; fi
        stop_api
        echo "${desc}: 高さ=${height}${ballots:+ 先頭の票数=${ballots}}: OK"
    }

    echo "== 4. 設定ファイルの値を書き換えると、動作が変わる（12 票を投票）"
    D_DEFAULT="$(new_dir)"
    run_case "既定（seal.max_ballots=100）: 12 票では封印されない" 0 0 "$D_DEFAULT" "$EMPTY_SECRETS"

    D_LOCAL="$(new_dir)"
    printf '[seal]\nmax_ballots = 5\n' >"$D_LOCAL/local.toml"
    run_case "local.toml の seal.max_ballots=5: 5 件ずつ封印（12 票 → 2 ブロック、残り 2 件）" 2 5 "$D_LOCAL" "$EMPTY_SECRETS"

    D_ENVFILE="$(new_dir)"
    printf '[seal]\nmax_ballots = 4\n' >"$D_ENVFILE/dev.toml"
    run_case "環境別ファイル dev.toml の seal.max_ballots=4（app.env=dev）: 12 票 → 3 ブロック" 3 4 "$D_ENVFILE" "$EMPTY_SECRETS" APP__APP__ENV=dev
    run_case "環境別ファイルは、選ばれた環境（app.env=test）のものだけ読む: dev.toml は無視" 0 0 "$D_ENVFILE" "$EMPTY_SECRETS" APP__APP__ENV=test

    D_PRIO="$(new_dir)"
    printf '[seal]\nmax_ballots = 5\n' >"$D_PRIO/local.toml"
    printf '[seal]\nmax_ballots = 4\n' >"$D_PRIO/dev.toml"
    run_case "local.toml（5）は dev.toml（4）に勝つ" 2 5 "$D_PRIO" "$EMPTY_SECRETS" APP__APP__ENV=dev

    echo "== 5. 環境変数が設定ファイルより優先される"
    run_case "local.toml（5）より環境変数 APP__SEAL__MAX_BALLOTS=3 が優先: 3 件ずつ封印（12 票 → 4 ブロック）" 4 3 "$D_LOCAL" "$EMPTY_SECRETS" APP__SEAL__MAX_BALLOTS=3

    shown="$(with_config "$D_LOCAL" "$EMPTY_SECRETS" APP__SHARD__COUNT=2 -- "$CFG" show)"
    grep -Fq "max_ballots = 5  # $D_LOCAL/local.toml" <<<"$shown" || { echo "$shown" >&2; fail "show に local.toml の値と出所が出ていません"; }
    grep -Fq 'count = 2  # 環境変数 APP__SHARD__COUNT' <<<"$shown" || { echo "$shown" >&2; fail "show に環境変数の値と出所が出ていません"; }
    grep -Fq 'port = 18080  # config/default.toml（既定値）' <<<"$shown" || fail "show に既定値の出所が出ていません"
    echo "show: 実効値と出所（既定値 / ファイル / 環境変数）: OK"

    # -----------------------------------------------------------------------
    echo "== 6. 秘密情報: secrets/ と環境変数から読める。設定ファイルには書けない。どこにも漏れない"
    D_NONE="$(new_dir)"
    S_DIR="$(new_dir)"
    printf '%s\n' "$SECRET" >"$S_DIR/session_secret"
    printf '%s' "$SEED_A" >"$S_DIR/sealer_signing_seed"
    start_api "$D_NONE" "$S_DIR" APP__SHARD__COUNT=1
    vote_n 1
    read_head
    KEY_FROM_FILE="$(head_field signer_public_key)"
    [[ ${#KEY_FROM_FILE} -eq 64 ]] || fail "secrets/sealer_signing_seed で署名鍵が決まりません: ${BODY}"
    stop_api
    start_api "$D_NONE" "$S_DIR" APP__SESSION__SECRET="$SECRET" APP__SEALER__SIGNING_SEED="$SEED_A"
    read_head
    [[ "$(head_field signer_public_key)" == "$KEY_FROM_FILE" ]] || fail "同じ署名鍵の種なのに、公開鍵が違います"
    stop_api
    start_api "$D_NONE" "$S_DIR" APP__SEALER__SIGNING_SEED="$SEED_B"
    read_head
    [[ "$(head_field signer_public_key)" != "$KEY_FROM_FILE" ]] || fail "環境変数の署名鍵の種が、secrets/ より優先されていません"
    stop_api
    echo "環境変数 > secrets/: OK"

    shown="$(with_config "$D_NONE" "$EMPTY_SECRETS" APP__SESSION__SECRET="$MARK_SECRET" APP__SEALER__SIGNING_SEED="$MARK_SEED" -- "$CFG" show)"
    if grep -Fq "$MARK_SECRET" <<<"$shown" || grep -Fq "$MARK_SEED" <<<"$shown"; then
        echo "$shown" >&2
        fail "show の出力に秘密情報が含まれています"
    fi
    grep -Fq 'secret = "***"' <<<"$shown" && grep -Fq 'signing_seed = "***"' <<<"$shown" \
        || { echo "$shown" >&2; fail "show で秘密情報が *** に伏せられていません"; }
    if with_config "$D_NONE" "$EMPTY_SECRETS" APP__SESSION__SECRET="$MARK_SECRET" -- "$CFG" get session.secret >"$TMP/get.out" 2>&1; then
        fail "get session.secret が成功しました（秘密情報は取得できないはず）"
    fi
    if grep -Fq "$MARK_SECRET" "$TMP/get.out"; then fail "get のエラーに秘密情報が含まれています"; fi
    echo "show の出力に秘密情報なし（***）: OK"

    # -----------------------------------------------------------------------
    # expect_failure DESC "パターン1" "パターン2"... -- COMMAND...
    expect_failure() {
        local desc="$1"
        shift
        local patterns=()
        while [[ "$1" != "--" ]]; do
            patterns+=("$1")
            shift
        done
        shift
        local out rc=0
        out="$(WITH_TIMEOUT=20 "$@" 2>&1)" || rc=$?
        [[ "$rc" -ne 0 && "$rc" -ne 124 ]] || fail "${desc}: 起動に失敗するはずが、終了コード ${rc}: ${out}"
        local p
        for p in "${patterns[@]}"; do
            grep -Fq -- "$p" <<<"$out" || { echo "$out" >&2; fail "${desc}: エラーに「${p}」が含まれていません"; }
        done
        if grep -Fq -- "$MARK_SECRET" <<<"$out"; then fail "${desc}: エラーに秘密情報が含まれています"; fi
        if [[ -n "${FORBID:-}" ]] && grep -Fq -- "$FORBID" <<<"$out"; then fail "${desc}: エラーに秘密情報（${FORBID}）が含まれています"; fi
        echo "${desc}: 失敗（終了コード ${rc}）、メッセージに $(printf '「%s」' "${patterns[@]}"): OK"
    }

    echo "== 7. 不正な設定では、どのファイル（環境変数）のどの項目がなぜ不正かを出して起動に失敗する"
    API_BIN=./target/debug/api
    S1="APP__SESSION__SECRET=$SECRET"

    D="$(new_dir)"
    printf '[seal]\nmax_ballots = 0\n' >"$D/local.toml"
    expect_failure "api: local.toml の seal.max_ballots=0" "$D/local.toml" "seal.max_ballots" "1 以上" -- \
        with_config "$D" "$EMPTY_SECRETS" "$S1" -- "$API_BIN"

    expect_failure "api: 環境変数 APP__SHARD__COUNT=abc" "環境変数 APP__SHARD__COUNT" "shard.count" "整数" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__SHARD__COUNT=abc -- "$API_BIN"

    D="$(new_dir)"
    printf '[seal]\nmax_ballot = 5\n' >"$D/local.toml"
    expect_failure "api: 未知の項目（タイプミス）" "$D/local.toml" "seal.max_ballot" "未知の項目" -- \
        with_config "$D" "$EMPTY_SECRETS" "$S1" -- "$API_BIN"

    D="$(new_dir)"
    printf '[session]\nsecret = "leaky-file-secret-0123456789"\n' >"$D/local.toml"
    FORBID="leaky-file-secret" expect_failure "api: 設定ファイルに秘密情報を書いた" "$D/local.toml" "session.secret" "APP__SESSION__SECRET" -- \
        with_config "$D" "$EMPTY_SECRETS" -- "$API_BIN"

    D="$(new_dir)"
    printf '[seal\nmax_ballots = 5\n' >"$D/local.toml"
    expect_failure "api: TOML の構文エラー" "$D/local.toml" "TOML" -- with_config "$D" "$EMPTY_SECRETS" "$S1" -- "$API_BIN"

    D="$(new_dir)"
    printf '[api]\nport = "80"\n' >"$D/local.toml"
    expect_failure "api: 型が違う（api.port が文字列）" "$D/local.toml" "api.port" "整数" -- \
        with_config "$D" "$EMPTY_SECRETS" "$S1" -- "$API_BIN"

    expect_failure "api: 範囲外（api.port と web.port が同じ）" "web.port" "同じポート" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__WEB__PORT=18080 -- "$API_BIN"

    expect_failure "api: 不正な app.mode" "app.mode" "memory / db" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__APP__MODE=disk -- "$API_BIN"

    expect_failure "api: 不正なキースペース名" "db.keyspace" "英字" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__DB__KEYSPACE=a-b -- "$API_BIN"

    expect_failure "api: 未知の環境変数" "環境変数 APP__SEAL__NOPE" "未知の項目" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__SEAL__NOPE=1 -- "$API_BIN"

    FORBID="tiny-secret-val" expect_failure "api: セッション署名鍵が短い" "session.secret" "16 バイト以上" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" APP__SESSION__SECRET=tiny-secret-val -- "$API_BIN"

    expect_failure "api: セッション署名鍵が未設定" "APP__SESSION__SECRET" "secrets/session_secret" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" -- "$API_BIN"

    expect_failure "api: auth.mode=db には app.mode=db が必要" "auth.mode" "app.mode=db" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__AUTH__MODE=db -- "$API_BIN"

    expect_failure "api: 開始が終了より後" "election.voting_closes_at" "voting_opens_at より後" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__ELECTION__VOTING_OPENS_AT=2030-01-02T00:00:00Z \
        APP__ELECTION__VOTING_CLOSES_AT=2030-01-01T00:00:00Z -- "$API_BIN"

    expect_failure "api: 投票期間の形式が不正" "election.voting_opens_at" "RFC 3339" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__ELECTION__VOTING_OPENS_AT=tomorrow -- "$API_BIN"

    expect_failure "api: 締切の形式が不正" "election.voting_closes_at" "RFC 3339" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__ELECTION__VOTING_CLOSES_AT=tomorrow -- "$API_BIN"

    expect_failure "api: 不正な app.env" "app.env" "dev / production / test" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__APP__ENV=staging -- "$API_BIN"

    expect_failure "api: production には config/production.toml が必要" "production.toml" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" "$S1" APP__APP__ENV=production -- "$API_BIN"

    D="$(new_dir)"
    printf '[seal]\nmax_ballots = 0\ninterval_secs = 0\n[shard]\ncount = 0\n' >"$D/local.toml"
    expect_failure "api: 複数の問題を一度に報告" "3 件" "seal.max_ballots" "seal.interval_secs" "shard.count" -- \
        with_config "$D" "$EMPTY_SECRETS" "$S1" -- "$API_BIN"

    expect_failure "sealer: app.mode=memory では起動できない" "app.mode=db" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" APP__SEALER__SIGNING_SEED="$MARK_SEED" -- ./target/debug/sealer
    expect_failure "sealer: 署名鍵の種が未設定" "APP__SEALER__SIGNING_SEED" "secrets/sealer_signing_seed" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" APP__APP__MODE=db -- ./target/debug/sealer
    expect_failure "sealer: 不正な設定（sealer.lease_ttl_secs=2）" "sealer.lease_ttl_secs" "3 以上" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" APP__APP__MODE=db APP__SEALER__SIGNING_SEED="$MARK_SEED" APP__SEALER__LEASE_TTL_SECS=2 -- ./target/debug/sealer
    expect_failure "verifier: 不正な設定（api.port=0）" "api.port" "1 以上" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" APP__API__PORT=0 -- ./target/debug/verifier verify
    expect_failure "bench: session.secret が未設定" "APP__SESSION__SECRET" -- \
        with_config "$D_NONE" "$EMPTY_SECRETS" -- ./target/debug/bench load --targets http://127.0.0.1:1 --mode rate --rate 1 \
        --duration 1 --label t --out "$TMP/bench.json"

    start_api "$D_NONE" "$EMPTY_SECRETS" APP__SESSION__SECRET="$SECRET"
    vout="$(with_config "$D_NONE" "$EMPTY_SECRETS" APP__API__PORT="$PORT" -- ./target/debug/verifier verify 2>&1)" \
        || { echo "$vout" >&2; fail "verifier verify（--api なし）が、設定の api.port の api を検証できません"; }
    grep -Fq '検証 OK' <<<"$vout" || { echo "$vout" >&2; fail "verifier verify の出力に「検証 OK」がありません"; }
    stop_api
    echo "verifier: --api を省略すると、設定の api.port（${PORT}）を検証: OK"

    # -----------------------------------------------------------------------
    echo "== 8. 画面の文言（labels.*）が、ビルド時に web へ渡る"
    command -v trunk >/dev/null 2>&1 || fail $'trunk が見つかりません。\n  次を実行してください: cargo install trunk --locked'
    rustup target list --installed 2>/dev/null | grep -qx 'wasm32-unknown-unknown' \
        || fail $'wasm ターゲットがありません。\n  次を実行してください: rustup target add wasm32-unknown-unknown'
    LABEL_TITLE="設定確認用タイトル-$RANDOM"
    LABEL_DONE="設定確認用の完了文言-$RANDOM"
    LABEL_ITEM="設定確認用の呼び名-$RANDOM"
    LABEL_PROGRESS="全{total}件のうち{current}件目-$RANDOM"
    web_env="$(with_config /nonexistent "$EMPTY_SECRETS" APP__LABELS__SITE_TITLE="$LABEL_TITLE" APP__LABELS__DONE_MESSAGE="$LABEL_DONE" \
        APP__LABELS__BALLOT_ITEM="$LABEL_ITEM" APP__LABELS__PROGRESS="$LABEL_PROGRESS" -- "$CFG" web-env)"
    DIST="$TMP/dist"
    if ! build_log="$(cd crates/web && eval "$web_env" && trunk build --dist "$DIST" 2>&1)"; then
        echo "$build_log" >&2
        fail "trunk build（labels 付き）が失敗しました"
    fi
    wasm_file="$(find "$DIST" -maxdepth 1 -name '*.wasm' | head -1)"
    [[ -n "$wasm_file" ]] || fail "dist に .wasm がありません"
    grep -aFq "$LABEL_TITLE" "$wasm_file" || fail "labels.site_title が web のビルドに反映されていません"
    grep -aFq "$LABEL_DONE" "$wasm_file" || fail "labels.done_message が web のビルドに反映されていません"
    grep -aFq "$LABEL_ITEM" "$wasm_file" || fail "labels.ballot_item が web のビルドに反映されていません"
    grep -aFq "$LABEL_PROGRESS" "$wasm_file" || fail "labels.progress が web のビルドに反映されていません"
    echo "labels.site_title / done_message / ballot_item / progress が、ビルド時の環境変数で web に渡る: OK"
    if cargo tree -p web --edges normal,build 2>/dev/null | grep -q 'app-config'; then
        fail "web が app-config に依存しています（原則 5）"
    fi
    echo "web は app-config に依存しない: OK"

    echo "OK: core#3〜8 設定"
)

# ===========================================================================
# 9. 性能計測ツール一式（旧 check_step8.sh）
# ===========================================================================
check_bench_tool() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    OUT="$(mktemp -d)"
    trap 'rm -rf "$OUT"' EXIT

    echo "== 9-1. bench クレートのテスト"
    cargo test -q -p bench >/dev/null || fail "bench のテストが失敗しました"
    echo "OK"

    echo "== 9-2. スモーク計測（shard.count=4, sealer 2, api 2。設定を短縮）"
    export BENCH_WARMUP_SECONDS=3 BENCH_WARMUP_RATE=100 BENCH_RATE_SECONDS=8 BENCH_STEADY_RATE=300
    export BENCH_SAT_SECONDS=8 BENCH_STORE_SECONDS=5
    # 最小件数 1: ツールの動作確認なので、ドレインで端数まで封印させて「未封印 0」を確かめる
    # （最小件数そのもの（原則9）は chain.sh#8 と domain::seal_policy のテストが確認する）。
    export BENCH_CONCURRENCY=32 BENCH_INTERVAL_SECS=10 BENCH_MIN_BALLOTS=1 BENCH_LEASE_TTL_SECS=6 BENCH_DRAIN_MAX_SECONDS=90
    ./scripts/bench.sh run --configs 4:2:2:drain --out "$OUT" >"$OUT/run.log" 2>&1 \
        || { tail -n 40 "$OUT/run.log" >&2; fail "bench.sh run が失敗しました"; }
    ./scripts/bench.sh store --shards 4 --out "$OUT" >"$OUT/store.log" 2>&1 \
        || { tail -n 40 "$OUT/store.log" >&2; fail "bench.sh store が失敗しました"; }

    RUN="$OUT/s4-k2-a2"
    for f in meta.json warmup.json rate.json sat.json summary.json drain.json cpu.csv pending.csv pending-after-sat.txt \
        sealer-1.log sealer-2.log api-0.log api-1.log proxyhistograms-after-steady.txt proxyhistograms-after-sat.txt; do
        [[ -s "$RUN/$f" ]] || fail "$RUN/$f がありません（または空です）"
    done
    [[ -s "$OUT/store-4.json" ]] || fail "store-4.json がありません"
    [[ -s "$OUT/tables.md" ]] || fail "tables.md がありません"

    for phase in sat rate; do
        grep -Eq '"accepted": [1-9][0-9]*' "$RUN/$phase.json" || fail "$phase: 受理した投票がありません"
        if grep -E '^\s+"[0-9]+": [0-9]+' "$RUN/$phase.json" | grep -v '"201"' | grep -q .; then
            fail "$phase: 201 以外の応答があります: $(grep -E '^\s+"[0-9]+": [0-9]+' "$RUN/$phase.json" | tr -d '\n')"
        fi
    done
    steady_pending="$(sed -n 's/.*"pending_max": *\([0-9]*\).*/\1/p' "$RUN/summary.json" | head -1)"
    [[ -n "$steady_pending" && "$steady_pending" -lt 2000 ]] || fail "定常負荷で未封印が積み上がっています（最大 ${steady_pending:-不明}）"
    grep -q '"drained": true' "$RUN/summary.json" || fail "ドレインが完了していません"
    grep -Eq '"unsealed": 0' "$RUN/summary.json" || fail "未封印の票が残っています"
    grep -Eq '"total_blocks": [1-9][0-9]*' "$RUN/summary.json" || fail "封印のログが集計されていません"
    grep -aq 'took_ms=' "$RUN/sealer-1.log" "$RUN/sealer-2.log" || fail "封印ログに took_ms がありません"
    for name in api sealer db bench machine; do
        grep -q ",${name}," "$RUN/cpu.csv" || fail "cpu.csv に ${name} のサンプルがありません"
    done
    for section in "飽和測定" "定常負荷" "封印の trigger 別" "封印遅延の分布" "took_ms" "CPU 使用率" "DB 直接"; do
        grep -q "$section" "$OUT/tables.md" || fail "tables.md に「${section}」の節がありません"
    done
    echo "出力: OK（$(grep -E 'accepted' "$RUN/sat.json" | head -1 | tr -d ' ,') / $(grep -E 'throughput_rps' "$RUN/sat.json" | head -1 | tr -d ' ,')）"
    echo "OK: core#9 bench ツール"
)

check_healthz
check_seal_policy
check_config
check_bench_tool

echo "OK: check/core.sh"
