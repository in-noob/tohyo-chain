#!/usr/bin/env bash
# 手元で画面を操作して確認するための、開発用の起動スクリプト。
#
#   scripts/dev_up.sh [memory|cassandra] [--auth stub|db]     （既定は memory）
#   scripts/dev_down.sh                      停止（cassandra では docker compose stop。ボリュームは残す）
#
#   memory    : api（dev-tools 有効・sealer 内蔵・seal.interval_secs=10・seal.min_ballots_after_interval=10（config/dev.toml））と trunk serve を起動する。
#               再起動すると、票もチェーンも消える。改ざんデモ（/debug/tamper）が使える。認証は stub（入力した ID をそのまま採用）。
#   cassandra : 上記に加えて、docker compose で Cassandra を起動して healthy を待ち、既定のキースペース vote に
#               スキーマを投入する（IF NOT EXISTS なので、既存のデータは残る）。sealer は別プロセスで動かす
#               （api は封印しない）。投票は DB に残る。改ざんデモの /debug/tamper は使えない（メモリ上のストアだけが対象）。
#
#   認証（--auth）: 既定は、memory が stub、cassandra が db。
#     stub : 入力した ID をそのまま採用する（名簿にある ID だけが投票できる）。
#     db   : 事前登録した ID とパスワードだけを認証する（cassandra だけ。memory では使えない）。起動のとき、DB に認証情報が無ければ
#            credgen で名簿の有権者を登録し、ID とパスワードを CSV（credentials.output_path。既定 secrets/credentials.csv。
#            git 管理外・権限 0600）に出力する。登録済みなら何もしない。パスワードは、画面にもログにも出さない。
#   環境変数 APP__AUTH__MODE を指定していれば、それを使う（--auth が優先）。
#
# 設定: config/default.toml < config/dev.toml < config/local.toml < secrets/ < 環境変数 APP__…（README の「設定」節）。
#   このスクリプトが決めるのは、app.mode（memory / db）・auth.mode（stub / db）と、秘密情報の開発用の固定値だけ。
#   ポートは設定から読む: api.port（既定 18080）、web.port（既定 8080）。/api は trunk serve の proxy が api へ転送する
#   （設定から Trunk 用の設定ファイル .dev/trunk.toml を生成して使う）。画面の文言（labels.*）も、設定からビルド時に渡す（app-config web-env）。
# ログ: logs/{api,sealer,trunk}.log。PID: .dev/pids（「役割 PID」を 1 行ずつ）。
#
# !! シークレットと署名鍵は、開発用の固定値（scripts/lib/common.sh の dev_default_secrets。scripts/sample_data.sh と共用）。
#    公開されているので、本番では絶対に使わないこと。
#    （手元で secrets/session_secret や APP__SESSION__SECRET を指定していれば、そちらを使う。）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

MODE=memory
AUTH_ARG=""
USAGE="使い方: scripts/dev_up.sh [memory|cassandra] [--auth stub|db]（既定は memory。認証の既定は、memory が stub、cassandra が db）"
while (($# > 0)); do
    case "$1" in
        memory | cassandra) MODE="$1" ;;
        --auth=*) AUTH_ARG="${1#--auth=}" ;;
        --auth)
            [[ $# -ge 2 ]] || {
                echo "$USAGE" >&2
                exit 2
            }
            AUTH_ARG="$2"
            shift
            ;;
        *)
            echo "$USAGE" >&2
            exit 2
            ;;
    esac
    shift
done
DEV_DIR=.dev
PIDS_FILE="$DEV_DIR/pids"
LOG_DIR=logs

# 手元の設定（config/local.toml、secrets/、環境変数 APP__…）を、そのまま使う（確認スクリプトと違って分離しない）。
CFG_USE_REAL=1
source "$SCRIPT_DIR/lib/common.sh"

CLEANUP_ON_EXIT=0
fail() {
    echo "FAIL: $1" >&2
    for f in api sealer trunk; do
        if [[ -s "$LOG_DIR/$f.log" ]]; then
            echo "--- $LOG_DIR/$f.log（末尾）---" >&2
            tail -n 15 "$LOG_DIR/$f.log" >&2
        fi
    done
    exit 1
}
# 起動の途中で失敗したときは、ここまでに起動したものを止める（すでに起動済みだった場合は触らない）。
on_exit() {
    local rc=$?
    if [[ "$CLEANUP_ON_EXIT" == 1 && "$rc" -ne 0 ]]; then
        echo "-- 起動に失敗したため、起動したプロセスを止めます" >&2
        "$SCRIPT_DIR/dev_down.sh" >&2 || true
    fi
}
trap on_exit EXIT

# 認証: --auth > 環境変数 APP__AUTH__MODE > 既定（memory は stub、cassandra は db）。
if [[ -n "$AUTH_ARG" ]]; then
    AUTH="$AUTH_ARG"
elif [[ -n "${APP__AUTH__MODE:-}" ]]; then
    AUTH="$APP__AUTH__MODE"
elif [[ "$MODE" == cassandra ]]; then
    AUTH=db
else
    AUTH=stub
fi
case "$AUTH" in
    stub | db) ;;
    *)
        echo "--auth は stub か db です（指定: ${AUTH}）。$USAGE" >&2
        exit 2
        ;;
esac
if [[ "$MODE" == memory && "$AUTH" == db ]]; then
    echo "認証 db は cassandra モードでだけ使えます（認証情報と名簿が DB にあるため）。$USAGE" >&2
    exit 2
fi

port_in_use() { (: </dev/tcp/127.0.0.1/"$1") 2>/dev/null; }
alive() { kill -0 "$1" 2>/dev/null; }
now_ms() { date +%s%3N; }

# wait_until TIMEOUT_SECS DESCRIPTION CMD...: CMD が成功するまで待つ。
wait_until() {
    local timeout="$1" what="$2" deadline
    shift 2
    deadline=$((SECONDS + timeout))
    until "$@"; do
        ((SECONDS < deadline)) || fail "${what}が ${timeout} 秒以内に完了しません"
        sleep 0.5
    done
}

# start ROLE LOGFILE CMD...: 新しいセッションで起動する（dev_up.sh が終わっても動き続け、グループごと止められる）。
start() {
    local role="$1" log="$2" pid
    shift 2
    setsid "$@" >"$log" 2>&1 </dev/null &
    pid=$!
    echo "$role $pid" >>"$PIDS_FILE"
    echo "  ${role}: PID ${pid}（ログ: ${log}）"
}

# ---------------------------------------------------------------------------
echo "== 0. 前提の確認"
cfg_init
# app.mode はこのスクリプトの引数で決める。秘密情報は、開発用の固定値（指定が無いときだけ）。
if [[ "$MODE" == cassandra ]]; then
    export APP__APP__MODE=db
else
    export APP__APP__MODE=memory
fi
export APP__AUTH__MODE="$AUTH"
dev_default_secrets
# 設定が不正なら、何も起動する前に、ここで（どのファイルのどの項目がなぜ不正か）を表示して終了する。
"$CFG_BIN" validate >/dev/null || fail "設定が不正です（上のメッセージを参照。実効値は: cargo run -p app-config -- show）"
ensure_revote_key
API_PORT_CFG="$(cfg_get api.port)"
WEB_PORT="$(cfg_get web.port)"
if [[ -f "$PIDS_FILE" ]]; then
    running=""
    while read -r role pid; do
        [[ -n "${pid:-}" ]] && alive "$pid" && running="${running} ${role}(${pid})"
    done <"$PIDS_FILE"
    if [[ -n "$running" ]]; then
        echo "すでに起動しています:${running}" >&2
        echo "先に scripts/dev_down.sh を実行してください。" >&2
        exit 1
    fi
    rm -f "$PIDS_FILE" # 前回の PID の残り（プロセスはもういない）
fi
for p in "$API_PORT_CFG" "$WEB_PORT"; do
    ! port_in_use "$p" || fail "ポート ${p} はすでに使われています（別のプロセスが待ち受けています）"
done
command -v trunk >/dev/null 2>&1 \
    || fail $'trunk が見つかりません。\n  次を実行してください: cargo install trunk --locked'
rustup target list --installed 2>/dev/null | grep -qx 'wasm32-unknown-unknown' \
    || fail $'wasm ターゲットがありません。\n  次を実行してください: rustup target add wasm32-unknown-unknown'

if [[ "$AUTH" == db ]]; then
    # 平文のパスワードを CSV に出力しないと、登録しても、だれもログインできない（DB にはハッシュしかない）。
    [[ "$(cfg_get credentials.output_file_enabled)" == true ]] \
        || fail "認証 db の開発起動には、credentials.output_file_enabled=true が必要です（config/dev.toml で有効にしています。手元の設定で false にしていないか確認してください）"
    CSV_PATH="$(cfg_get credentials.output_path)"
fi

mkdir -p "$DEV_DIR" "$LOG_DIR"
: >"$PIDS_FILE"
echo "$MODE" >"$DEV_DIR/mode"
CLEANUP_ON_EXIT=1
: >"$LOG_DIR/api.log"
: >"$LOG_DIR/trunk.log"
rm -f "$LOG_DIR/sealer.log"

# ---------------------------------------------------------------------------
if [[ "$MODE" == cassandra ]]; then
    echo "== 1. Cassandra の起動（docker compose）"
    db_setup_vars
    db_check_docker
    db_ensure
    # 既定のキースペース vote にスキーマを投入する（IF NOT EXISTS。既存のデータは消えない）。
    KEYSPACE="$(cfg_get db.keyspace)"
    SCHEMA_KEYSPACE="$KEYSPACE" "${COMPOSE[@]}" run --rm "$SCHEMA_SERVICE" >/dev/null 2>&1 \
        || fail "スキーマの投入に失敗しました（キースペース ${KEYSPACE}）"
    echo "  Cassandra: healthy、スキーマ（キースペース ${KEYSPACE}）: OK"
fi

echo "== 2. ビルド"
cargo build -q -p api --features dev-tools
cargo build -q -p seedgen
[[ "$MODE" == cassandra ]] && cargo build -q -p sealer
[[ "$AUTH" == db ]] && cargo build -q -p credgen

if [[ "$AUTH" == db ]]; then
    echo "== 3. ID・パスワードの登録（auth.mode=db。credgen）"
    registered="$(ks_cql "SELECT count(*) FROM ${KEYSPACE}.credentials" 2>/dev/null | grep -E '^\s*[0-9]+\s*$' | tr -d ' ' | head -1 || true)"
    [[ -n "$registered" ]] || fail "認証情報の件数を取得できません（キースペース ${KEYSPACE}）"
    if [[ "$registered" == 0 ]]; then
        # DB に認証情報が無い（初回、または db_reset.sh --all の後）。前回の CSV が残っていると、credgen は上書きせず失敗するうえ、
        # 古いパスワードは、もう使えないので、退避してから登録する。
        if [[ -e "$CSV_PATH" ]]; then
            mv -f "$CSV_PATH" "${CSV_PATH}.old"
            echo "  DB に認証情報が無いので、前回の ${CSV_PATH}（古いパスワードは使えません）を ${CSV_PATH}.old に退避しました"
        fi
        credgen_out="$(./target/debug/credgen 2>&1)" || {
            echo "$credgen_out" >&2
            fail "credgen が失敗しました（上のメッセージを参照）"
        }
        grep -E '^(登録|出力):' <<<"$credgen_out" | sed 's/^/  /'
    else
        echo "  登録済み: ${registered} 件（credgen は実行しません）"
        [[ -e "$CSV_PATH" ]] \
            || echo "  注意: ${CSV_PATH} がありません（パスワードを確認できません）。発行し直すなら: APP__CREDENTIALS__OUTPUT_PATH=<新しい CSV> cargo run -q -p credgen -- --reissue"
    fi
fi

echo "== 4. 起動"
export RUST_LOG="${RUST_LOG:-info,tower_http=warn}"

if [[ "$MODE" == cassandra ]]; then
    # shard.count と署名鍵は、DB（キースペース）に登録済みの値と一致していること（食い違うと、起動を拒否される。ログに理由が出る）。
    APP__SEALER__ID="${APP__SEALER__ID:-dev-sealer}" start sealer "$LOG_DIR/sealer.log" ./target/debug/sealer
fi
start api "$LOG_DIR/api.log" ./target/debug/api
# trunk は crates/web で実行する（ルートで実行すると、ルートパッケージが見つからず失敗する）。
# ポートと proxy 先は設定から渡す。画面の文言（labels.*）は、ビルド時の環境変数で渡す。
# （trunk の --proxy-backend は、Trunk.toml の proxy を上書きせず追加してしまうので、設定から Trunk 用の設定ファイルを生成して使う。）
WEB_ENV="$("$CFG_BIN" web-env)"
TRUNK_CONFIG="$PWD/$DEV_DIR/trunk.toml"
cat >"$TRUNK_CONFIG" <<TOML
# scripts/dev_up.sh が、設定（web.port / api.port）から生成したファイル（手で編集しない。crates/web/Trunk.toml と同じ内容）。
[build]
target = "$PWD/crates/web/index.html"
dist = "$PWD/crates/web/dist"

[serve]
addresses = ["127.0.0.1"]
port = ${WEB_PORT}

[[proxy]]
backend = "http://localhost:${API_PORT_CFG}/api/"
TOML
(cd crates/web && eval "$WEB_ENV" && exec setsid trunk --config "$TRUNK_CONFIG" serve \
    >"../../$LOG_DIR/trunk.log" 2>&1 </dev/null) &
echo "trunk $!" >>"$PIDS_FILE"
echo "  trunk: PID $!（ログ: $LOG_DIR/trunk.log。初回は wasm のビルドで数分かかることがある）"

echo "== 5. 起動の完了を待つ"
api_pid="$(awk '$1 == "api" {print $2}' "$PIDS_FILE")"
api_ready() {
    alive "$api_pid" || fail "api が終了しました"
    [[ "$(curl -s --max-time 2 "http://127.0.0.1:${API_PORT_CFG}/healthz" || true)" == *'"status":"ok"'* ]]
}
wait_until 60 "api の起動" api_ready
if [[ "$MODE" == cassandra ]]; then
    # sealer がシャード 0 のリースを取り、ジェネシスを作るまで（前回クラッシュした場合はリースの期限切れまで）待つ。
    sealer_pid="$(awk '$1 == "sealer" {print $2}' "$PIDS_FILE")"
    chain_ready() {
        alive "$sealer_pid" || fail "sealer が終了しました"
        [[ "$(curl -s --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${API_PORT_CFG}/api/v1/chains/0/head" || true)" == 200 ]]
    }
    wait_until 90 "sealer のチェーン準備" chain_ready
fi
trunk_pid="$(awk '$1 == "trunk" {print $2}' "$PIDS_FILE")"
web_ready() {
    alive "$trunk_pid" || fail "trunk serve が終了しました"
    [[ "$(curl -s --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${WEB_PORT}/" || true)" == 200 ]]
}
wait_until 600 "画面（trunk serve）の起動" web_ready
CLEANUP_ON_EXIT=0

API="http://localhost:${API_PORT_CFG}"
# 選挙データ（設定 election.seed_dir / election.election_id）と、名簿の有権者の例。
ELECTION_DIR="$(cfg_get election.seed_dir)/$(cfg_get election.election_id)"
ELECTION_SUMMARY="$(./target/debug/seedgen --check "$(cfg_get election.seed_dir)" --election-id "$(cfg_get election.election_id)" 2>/dev/null | sed 's/^OK: [^:]*: //' || true)"
VOTER_EXAMPLES="$(sed -n '2,6p' "${ELECTION_DIR}/voters.csv" 2>/dev/null | cut -d, -f1 | paste -sd' ' || true)"
NOTE_PERSIST=""
if [[ "$MODE" == cassandra ]]; then
    NOTE_PERSIST="   ※ cassandra モードでは投票が DB（キースペース vote）に残る。投票済みの ID は再投票できないので、別の ID を使う。
"
fi
if [[ "$AUTH" == db ]]; then
    LOGIN_SECTION="■ ログイン（認証は DB: 事前登録した ID とパスワードだけ。入力した ID をそのまま使うことはできない）:
   ID とパスワードは ${CSV_PATH}（権限 0600・git 管理外。平文のパスワードを含むので、扱いに注意）:
     head -3 ${CSV_PATH}          # 列: login_id, password, 都道府県, 選挙区
   ログイン ID は、名簿の ID（alice など）とは別のランダムな値。投票できるのは、その有権者の投票用紙だけ（選挙データ: ${ELECTION_DIR}（${ELECTION_SUMMARY}））。
   投票する順番は固定: 有権者に関係する投票用紙が表示順に並び、先頭の未投票へ自動で進む。
   マイナンバー欄は任意で、受け取って破棄される。
   パスワードを忘れた・CSV を失くしたとき: APP__CREDENTIALS__OUTPUT_PATH=<新しい CSV> cargo run -q -p credgen -- --reissue
   DB を初期状態に戻すとき: scripts/dev_down.sh → APP__APP__MODE=db scripts/db_reset.sh --all --yes → scripts/dev_up.sh cassandra（認証情報は、自動で登録し直される）
   認証を stub（入力した ID をそのまま使う）にして起動するには: scripts/dev_down.sh → scripts/dev_up.sh cassandra --auth stub"
else
    LOGIN_SECTION="■ ログインに使える ID（認証はスタブ。入力した ID をそのまま使う。投票できるのは、選挙データの名簿にある有権者だけ）:
   選挙データ: ${ELECTION_DIR}（${ELECTION_SUMMARY}）
   名簿の有権者の例（先頭）: ${VOTER_EXAMPLES}
   規則: 英数字・アンダースコア（_）・ハイフン（-）だけ、1〜64 文字。名簿に無い ID は、ログインできるが、投票用紙は 1 枚もない。
   投票する順番は固定: 有権者に関係する投票用紙が表示順に並び、先頭の未投票へ自動で進む。
   マイナンバー欄は任意で、受け取って破棄される。"
fi
cat <<MSG

============================================================
 起動しました（モード: ${MODE}、認証: ${AUTH}）
============================================================
■ ブラウザで開く:  http://localhost:${WEB_PORT}
   （api は ${API}。画面の /api は proxy で api へ転送される）

${LOGIN_SECTION}
${NOTE_PERSIST}
■ チェーンの先頭（シャード 0）を見る:
   curl -s ${API}/api/v1/chains/0/head
   （票は、未封印が 10 件以上になり、前回の封印から 10 秒以上経つと封印される（config/dev.toml）。
    10 件に満たない票は、scripts/election.sh close --now（締切の手続き）で封印される）

■ チェーンを検証する（全シャード・突合・アンカー）:
   cargo run -q -p verifier -- verify --api ${API}

■ 改ざんデモ:
MSG
if [[ "$MODE" == memory ]]; then
    cat <<MSG
   1) 画面から数票投票し、封印（10 秒）を待つ
   2) curl -s -X POST ${API}/debug/tamper      # 封印済みの票を 1 件書き換える（メモリ上）
   3) cargo run -q -p verifier -- verify --api ${API}   # 改ざんが検出され、NG になる
   （元に戻すには、dev_down.sh → dev_up.sh で再起動する）
MSG
else
    cat <<MSG
   cassandra モードでは /debug/tamper は使えない（メモリ上のストアだけが対象）。次のオフラインのデモを使う:
   cargo run -q -p verifier -- demo
   （api を介したデモは、scripts/dev_up.sh memory で起動する）
MSG
fi
cat <<MSG

■ ログを見る:
   tail -f ${LOG_DIR}/api.log ${LOG_DIR}/trunk.log$([[ "$MODE" == cassandra ]] && echo " ${LOG_DIR}/sealer.log")

■ 実効の設定を見る（秘密情報は ***）:  cargo run -q -p app-config -- show

■ 選挙状態（scheduled → open → closing → closed）を操作する:
   scripts/election.sh status
   scripts/election.sh schedule --opens-at <RFC3339> --closes-at <RFC3339>
   scripts/election.sh open --now
   scripts/election.sh close --now
   （管理用リスナー: $(cfg_get admin.bind)。トークンは開発用の固定値を使っている）

■ 停止:  scripts/dev_down.sh
MSG
