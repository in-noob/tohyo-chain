# shellcheck shell=bash
# scripts/check/*.sh・scripts/dev_up.sh・scripts/db_reset.sh・scripts/bench.sh・scripts/tally.sh 共通の処理。
# ワークスペースのルートで source する。
#
#   設定（crates/app-config）  cfg_build / cfg_init / cfg_get / admin_token（README の「設定」節）
#   DB（Cassandra / ScyllaDB） db_setup_vars / db_check_docker / db_static_checks / db_ensure / db_fresh /
#                              db_fresh_stop / db_node_addr / db_stop
#   専用キースペース           ks_init / ks_cql / ks_create / ks_matching / ks_drop
#   選挙データ（seedgen）      seed_generate / seed_district / seed_vote
#   起動・停止・待ち合わせ     spawn / alive / graceful_stop / hard_stop / stop_all / wait_port / wait_healthz /
#                              wait_until / now_ms
#   アサーション               fail / request / expect_status / count / login
#
# 確認スイート（scripts/check/*.sh）は、手元の設定（config/local.toml、secrets/、環境変数 APP__…）の影響で
# 結果が変わらないよう、cfg_init で設定を分離する（下記）。CFG_USE_REAL=1 なら分離しない
# （scripts/dev_up.sh・scripts/db_reset.sh は、手元の実際の設定を使う）。

# fail が未定義のスクリプトのための既定。個々のスイートは、ログの末尾を表示するなど、より詳しい fail を上書きしてよい。
if ! declare -F fail >/dev/null 2>&1; then
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
fi

# ---------------------------------------------------------------------------
# 設定（crates/app-config）
# ---------------------------------------------------------------------------

cfg_build() {
    cargo build -q -p app-config || fail "app-config のビルドに失敗しました"
    CFG_BIN="$(pwd)/target/debug/app-config"
}

# cfg_init: 設定ファイルの場所を存在しないディレクトリに向け、環境変数 APP__… を消して app.env=test にする
# （既定値 + スイートが渡す APP__… だけになる）。ただし、DB の種類 APP__DB__BACKEND（cassandra / scylla）は引き継ぐ。
cfg_init() {
    cfg_build
    if [[ "${CFG_USE_REAL:-0}" == "1" ]]; then
        return 0
    fi
    local backend="${APP__DB__BACKEND:-}" name
    for name in $(compgen -e | grep '^APP__' || true); do
        unset "$name"
    done
    export APP_CONFIG_DIR=/nonexistent/app-config/config
    export APP_SECRETS_DIR=/nonexistent/app-config/secrets
    export APP__APP__ENV=test
    if [[ -n "$backend" ]]; then
        export APP__DB__BACKEND="$backend"
    fi
}

cfg_get() {
    "$CFG_BIN" get "$1"
}

# admin.token（秘密情報）は app-config get で取得できない（原則: 秘密情報は取得できない）。
# api / sealer と同じ場所（環境変数 APP__ADMIN__TOKEN、無ければ secrets/admin_token）から、
# scripts/election.sh 自身が読む。どちらにも無ければ空を返す。
admin_token() {
    if [[ -n "${APP__ADMIN__TOKEN:-}" ]]; then
        printf '%s' "$APP__ADMIN__TOKEN"
    else
        cat "${APP_SECRETS_DIR:-secrets}/admin_token" 2>/dev/null | tr -d '\n' || true
    fi
}

# ---------------------------------------------------------------------------
# DB（Cassandra / ScyllaDB。docker compose）
# ---------------------------------------------------------------------------
#
# 分離の方針（テストどうしが干渉しないように）:
#   - DB のコンテナは共用（手動で使う開発用の DB）。確認スイートは、起動していなければ起動するだけで、
#     停止も、ボリュームの破棄（down -v）もしない。手動で入れたデータ（キースペース vote）に一切触れない。
#   - スイートは、実行ごとの専用キースペース（vote_<tag>_<UNIXTIME>_<PID>）を作り、api / sealer / verifier /
#     統合テストのすべてにその名前を環境変数（APP__DB__KEYSPACE / TEST_KEYSPACE_PREFIX）で渡す。
#   - 終了時（成功でも失敗でも）、trap で、そのキースペース（と、接頭辞で始まる統合テストのキースペース）を DROP する。
#   - 性能計測（bench.sh）は、専用の Compose プロジェクト（vote-bench）と別ポートの DB を使い、共用 DB に触れない。
#
# 環境変数:
#   APP__DB__BACKEND=cassandra|scylla  使う DB（設定 db.backend）。既定は cassandra（ローカル開発の既定）。
#                                ScyllaDB は明示したときだけ。ChromeOS の Linux（39 ビットの仮想アドレス空間）では
#                                ScyllaDB が起動できない。確認スイートは、手元の設定を無視するが、この変数だけは引き継ぐ。
#   DB_PORT                      DB を公開するホスト側のポート（docker compose の変数。既定 9042）
#   KEEP_KEYSPACE=1              終了時に専用キースペースを DROP せず残し、名前を表示する（失敗時の調査用）
#   STOP_DB=1                    終了時に DB のコンテナを停止する（ボリュームは残す。既定は起動したまま）

db_setup_vars() {
    cfg_init
    DB_BACKEND="$(cfg_get db.backend)"
    local first_node
    first_node="$(cfg_get db.nodes | cut -d, -f1)"
    export DB_PORT="${DB_PORT:-${first_node##*:}}"
    DB_SERVICE="$DB_BACKEND"
    SCHEMA_SERVICE="schema-${DB_BACKEND}"
    COMPOSE=(docker compose --profile "$DB_BACKEND")
    COMPOSE_ALL=(docker compose --profile cassandra --profile scylla)
    KS=""
}

db_check_docker() {
    command -v docker >/dev/null 2>&1 \
        || fail $'docker が見つかりません。Docker Engine と Compose プラグインをインストールしてください。\n  https://docs.docker.com/engine/install/'
    docker info >/dev/null 2>&1 \
        || fail $'docker デーモンに接続できません。起動していること、現在のユーザーが docker グループに属していることを確認してください。\n  例: sudo usermod -aG docker "$USER"（反映には再ログインが必要。すぐ試すなら: sg docker -c '"$0"'）'
    "${COMPOSE[@]}" version >/dev/null 2>&1 \
        || fail "docker compose プラグインが見つかりません"
}

# 静的検査（schema.cql が Cassandra / ScyllaDB 共通で、キースペース名を固定していないこと。Cassandra のヒープ設定）。
db_static_checks() {
    local scylla_only='tablets|bypass cache|using timeout|service_level|scylla'
    if grep -vE '^\s*--' docs/schema.cql | grep -Eni "$scylla_only"; then
        fail "docs/schema.cql に Scylla 固有の構文があります（Cassandra と共通の CQL にしてください）"
    fi
    grep -q '{{KEYSPACE}}' docs/schema.cql \
        || fail "docs/schema.cql にキースペース名のプレースホルダ {{KEYSPACE}} がありません"
    if grep -vE '^\s*--' docs/schema.cql | grep -Eni 'vote\.|EXISTS vote'; then
        fail "docs/schema.cql に固定のキースペース名（vote）があります。{{KEYSPACE}} を使ってください"
    fi
    if [[ -n "${CASSANDRA_MAX_HEAP:-}" || -n "${CASSANDRA_HEAP_NEW:-}" ]]; then
        [[ -n "${CASSANDRA_MAX_HEAP:-}" && -n "${CASSANDRA_HEAP_NEW:-}" ]] \
            || fail "CASSANDRA_MAX_HEAP と CASSANDRA_HEAP_NEW はペアで指定してください（片方だけだと Cassandra が起動しません）"
    fi
    local cassandra_config want_max="${CASSANDRA_MAX_HEAP:-1G}" want_new="${CASSANDRA_HEAP_NEW:-256M}"
    cassandra_config="$("${COMPOSE_ALL[@]}" config 2>/dev/null)"
    grep -q "MAX_HEAP_SIZE: ${want_max}" <<<"$cassandra_config" && grep -q "HEAP_NEWSIZE: ${want_new}" <<<"$cassandra_config" \
        || fail "docker-compose.yml の cassandra に MAX_HEAP_SIZE=${want_max} と HEAP_NEWSIZE=${want_new} がセットで指定されていません"
}

db_failed() {
    echo "--- docker logs（${DB_SERVICE}）の FATAL / ERROR 行 ---" >&2
    "${COMPOSE[@]}" logs --no-color "$DB_SERVICE" 2>&1 | grep -E 'FATAL|ERROR' >&2 || echo "（該当する行はありません）" >&2
    echo "---" >&2
    fail "$1"
}

db_wait_healthy() {
    local container deadline
    container="$("${COMPOSE[@]}" ps -q "$DB_SERVICE")"
    [[ -n "$container" ]] || db_failed "DB のコンテナがありません"
    deadline=$((SECONDS + 400))
    until [[ "$(docker inspect -f '{{.State.Health.Status}}' "$container" 2>/dev/null)" == "healthy" ]]; do
        [[ "$(docker inspect -f '{{.State.Status}}' "$container" 2>/dev/null)" == "running" ]] \
            || db_failed "DB のコンテナが終了しました"
        "${COMPOSE[@]}" logs --no-color "$DB_SERVICE" 2>&1 | grep -q 'entered FATAL state' \
            && db_failed "DB のプロセスが FATAL 状態になりました"
        ((SECONDS < deadline)) || db_failed "DB が 400 秒以内に healthy になりませんでした"
        sleep 3
    done
}

# 共用 DB を使う: 起動していなければ起動して、healthy になるまで待つ。停止も破棄もしない。
db_ensure() {
    "${COMPOSE[@]}" up -d "$DB_SERVICE" >/dev/null 2>&1 || db_failed "docker compose up に失敗しました"
    db_wait_healthy
}

# 専用の Compose プロジェクト（bench.sh の vote-bench）向け: クリーンな状態から DB を起動する（down -v してから起動）。
# 共用のプロジェクト（vote-prototype）では実行を拒否する（手動で入れたデータを消さないため）。
db_fresh() {
    local project="${COMPOSE_PROJECT_NAME:-}"
    [[ -n "$project" && "$project" != "vote-prototype" ]] \
        || fail "db_fresh は専用の Compose プロジェクトでだけ使えます（COMPOSE_PROJECT_NAME を vote-prototype 以外に設定）。共用 DB は破棄しません"
    "${COMPOSE_ALL[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    "${COMPOSE[@]}" up -d "$DB_SERVICE" >/dev/null 2>&1 || db_failed "docker compose up に失敗しました"
    db_wait_healthy
}

# db_node_addr: DB コンテナの IP:9042（ホストから直接届く）。公開ポートが 9042 以外のとき（bench の 19042）は、
# ドライバがノード検出で得たコンテナ IP に公開ポートの番号を組み合わせて接続に失敗するため、こちらを使う。
db_node_addr() {
    local ip
    ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$("${COMPOSE[@]}" ps -q "$DB_SERVICE")")"
    [[ -n "$ip" ]] || fail "DB コンテナの IP アドレスを取得できません"
    echo "${ip}:9042"
}

# 専用プロジェクトの DB を、ボリュームごと破棄する。
db_fresh_stop() {
    local project="${COMPOSE_PROJECT_NAME:-}"
    [[ -n "$project" && "$project" != "vote-prototype" ]] || return 0
    "${COMPOSE_ALL[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
}

# 終了時: 既定では DB を起動したままにする。STOP_DB=1 のときだけ、コンテナを停止する（ボリュームは残す）。
db_stop() {
    if [[ "${STOP_DB:-0}" == "1" ]]; then
        "${COMPOSE_ALL[@]}" stop >/dev/null 2>&1 || true
    fi
}

# ---------------------------------------------------------------------------
# 実行ごとの専用キースペース
# ---------------------------------------------------------------------------

# ks_init TAG: この実行専用のキースペース名を決める（例: vote_chain_1789912345_123456）。
# 名前は、先頭が英字・英数字と _ のみ・48 文字以内（infra_scylla::env::parse_keyspace の規則）。
ks_init() {
    KS="vote_${1}_$(date +%s)_$$"
    [[ "$KS" =~ ^[a-z][a-z0-9_]{0,47}$ ]] || fail "キースペース名が不正です: ${KS}"
    export APP__DB__KEYSPACE="$KS"
    # 統合テスト（infra-scylla）・性能計測が作るキースペースの接頭辞。終了時に、この接頭辞のものもまとめて DROP する。
    export TEST_KEYSPACE_PREFIX="$KS"
    export BENCH_KEYSPACE_PREFIX="$KS"
}

ks_cql() {
    "${COMPOSE[@]}" exec -T "$DB_SERVICE" cqlsh -e "$1"
}

# ks_create [SKIP_IDEMPOTENCY_CHECK]: 専用キースペースにスキーマ（docs/schema.cql のテンプレート）を投入する。
# 既定は 2 回流して、冪等（IF NOT EXISTS）であることも確認する。1 つのスイートの中で複数のキースペースを作るときは、
# 2 回目以降は "skip"（既に確認済み）を渡して、schema ジョブの起動を 1 回に減らしてよい。
ks_create() {
    [[ -n "$KS" ]] || fail "ks_init を先に呼んでください"
    SCHEMA_KEYSPACE="$KS" "${COMPOSE[@]}" run --rm "$SCHEMA_SERVICE" >/dev/null 2>&1 \
        || fail "スキーマの投入に失敗しました（キースペース ${KS}）"
    if [[ "${1:-}" != skip ]]; then
        SCHEMA_KEYSPACE="$KS" "${COMPOSE[@]}" run --rm "$SCHEMA_SERVICE" >/dev/null 2>&1 \
            || fail "スキーマの再投入（冪等性）に失敗しました（キースペース ${KS}）"
    fi
}

# ks_matching: 専用キースペース名（と、その接頭辞で始まるもの）の一覧。
ks_matching() {
    ks_cql 'SELECT keyspace_name FROM system_schema.keyspaces' 2>/dev/null \
        | tr -d ' ' | grep -E "^${KS}(_[a-z0-9_]*)?$" || true
}

# ks_drop: 終了時の後始末（trap から呼ぶ。成功でも失敗でも実行する）。
# 専用キースペースと、その接頭辞で始まるもの（統合テストのキースペース）を DROP する。
# KEEP_KEYSPACE=1 なら DROP せず、残したキースペース名を表示する。共用の vote には触れない。
ks_drop() {
    [[ -n "${KS:-}" ]] || return 0
    local names name
    names="$(ks_matching)"
    [[ -n "$names" ]] || return 0
    if [[ "${KEEP_KEYSPACE:-0}" == "1" ]]; then
        echo "KEEP_KEYSPACE=1: 次のキースペースを残しました:" >&2
        sed 's/^/  /' <<<"$names" >&2
        return 0
    fi
    for name in $names; do
        ks_cql "DROP KEYSPACE IF EXISTS ${name}" >/dev/null 2>&1 \
            || echo "警告: キースペース ${name} を DROP できませんでした" >&2
    done
}

# ---------------------------------------------------------------------------
# 選挙データ（seedgen）
# ---------------------------------------------------------------------------
#
# api は、名簿にある有権者の、属する選挙区の投票用紙にしか投票させない（対象外は 403）。だから、確認スイートも、
# 生成したデータの有権者 voter-1 … voter-N に、その有権者の投票用紙（表示順の K 番目）へ投票する。
#   seed_generate DIR VOTERS   3 都道府県・小選挙区 2・候補者 4 の小さなデータ（有権者 VOTERS 人。1 人あたり 9 枚の投票用紙）を
#                              DIR に作り、設定（APP__ELECTION__SEED_DIR）で api に渡す
#   seed_district N K          有権者 voter-N の、表示順で K 番目の選挙区の ID（K=1 が小選挙区、2 が比例ブロック、3 が参議院選挙区…）
#   seed_vote N K TOKEN        voter-N が K 番目の投票用紙に投票し、HTTP ステータスを出力する（BASE の api へ）

SEED_ID=2026-general
export SEED_ID

seed_generate() {
    cargo build -q -p seedgen || fail "seedgen のビルドに失敗しました"
    ./target/debug/seedgen --out "$1" --election-id "$SEED_ID" --prefectures 3 --districts-per-pref 2 \
        --candidates-per-district 4 --voters "$2" --municipalities-per-pref 2 \
        --pref-assembly-districts-per-pref 2 --force >/dev/null || fail "seedgen が失敗しました"
    export APP__ELECTION__SEED_DIR="$1"
    export SEED_VOTERS_CSV="$1/$SEED_ID/voters.csv"
}

# voters.csv は、ヘッダ 1 行のあとに voter-1, voter-2, … の順で並ぶ（seedgen の出力）。
seed_district() {
    sed -n "$(($1 + 1))p" "$SEED_VOTERS_CSV" | cut -d, -f2 | cut -d';' -f"$2"
}

seed_vote() {
    local district
    district="$(seed_district "$1" "$2")"
    curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
        "${BASE}/api/v1/contests/${SEED_ID}/${district}/vote" \
        -H "Authorization: Bearer ${3}" -H 'Content-Type: application/json' \
        -d "{\"candidate_id\":\"${district}.c$(($1 % 4 + 1))\"}"
}
export -f seed_district seed_vote

# ---------------------------------------------------------------------------
# 起動・停止・ポートの待ち合わせ
# ---------------------------------------------------------------------------
#
# 名前つきでバックグラウンド起動し（spawn）、生存確認（alive）・正常停止（graceful_stop）・強制終了（hard_stop）をする。
# 1 つのスイートの中で、複数の役割（api・sealer・sealer-a・sealer-b …）を、名前で区別して扱える。

declare -gA SUITE_PIDS=()
declare -gA SUITE_LOGS=()

# spawn NAME LOGFILE CMD...: 名前つきでバックグラウンド起動する。PID は SUITE_PIDS[NAME] に入る。
# ログファイルは、起動のたびに空にする（1 つの名前を使い回して再起動するとき、直前のプロセスの出力と混ざらない）。
spawn() {
    local name="$1" log="$2"
    shift 2
    : >"$log"
    "$@" >"$log" 2>&1 &
    SUITE_PIDS["$name"]=$!
    SUITE_LOGS["$name"]="$log"
}

# spawn_append NAME LOGFILE CMD...: spawn と同じだが、ログファイルを空にしない（追記する）。
# 複数のプロセス（api・sealer など）で 1 つのログファイルを共有するときや、再起動をまたいでログを
# 累積させたいとき（DB 永続化の確認など）に使う。
spawn_append() {
    local name="$1" log="$2"
    shift 2
    "$@" >>"$log" 2>&1 &
    SUITE_PIDS["$name"]=$!
    SUITE_LOGS["$name"]="$log"
}

alive() {
    local pid="${SUITE_PIDS[$1]:-}"
    [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

pid_of() { echo "${SUITE_PIDS[$1]:-}"; }
log_of() { echo "${SUITE_LOGS[$1]:-}"; }

# graceful_stop NAME [EXPECT_CODE=0]: SIGTERM を送って待ち、終了コードを確認する。
graceful_stop() {
    local name="$1" expect="${2:-0}" pid="${SUITE_PIDS[$1]:-}" rc
    [[ -n "$pid" ]] || return 0
    kill -TERM "$pid" 2>/dev/null || true
    set +e
    wait "$pid"
    rc=$?
    set -e
    unset 'SUITE_PIDS[$name]'
    [[ "$rc" == "$expect" ]] || fail "${name}: SIGTERM 後の終了コードが ${expect} ではありません（exit=${rc}）"
}

# hard_stop NAME: SIGKILL で強制終了する（終了コードは見ない。クラッシュを模す）。
hard_stop() {
    local name="$1" pid="${SUITE_PIDS[$1]:-}"
    [[ -n "$pid" ]] || return 0
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    unset 'SUITE_PIDS[$name]'
}

# stop_all: 残っているプロセス（spawn で起動した、まだ生きているもの）をすべて SIGKILL する（trap cleanup から呼ぶ）。
stop_all() {
    local name
    for name in "${!SUITE_PIDS[@]}"; do
        hard_stop "$name"
    done
}

# wait_port PORT [TIMEOUT_SECS=10]: TCP ポートが listen 状態になるまで待つ。
wait_port() {
    local port="$1" timeout="${2:-10}" deadline
    deadline=$((SECONDS + timeout))
    until (: </dev/tcp/127.0.0.1/"$port") 2>/dev/null; do
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# wait_healthz BASE_URL [TIMEOUT_SECS=15] [PROCESS_NAME]: /healthz が 200 を返すまで待つ。PROCESS_NAME を指定すると、
# その名前（spawn で付けた名前）のプロセスが生きているかも、待つ間ずっと確認する。
wait_healthz() {
    local base="$1" timeout="${2:-15}" name="${3:-}" deadline
    deadline=$((SECONDS + timeout))
    while true; do
        if [[ -n "$name" ]]; then alive "$name" || fail "${name} が起動直後に終了しました"; fi
        curl -fsS -o /dev/null "${base}/healthz" 2>/dev/null && return 0
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# wait_election_open BASE [TIMEOUT_SECS=15]: 選挙状態（原則17）が open になるまで待つ。
# 投票フローを見るスイートは、election.voting_opens_at を過去にして起動し、wait_healthz の後にこれを呼ぶ
# （memory モードは api 内蔵のスケジューラ、db モードはアンカーのリースを持つ sealer が自動で open にする）。
wait_election_open() {
    local base="$1" timeout="${2:-15}" deadline
    deadline=$((SECONDS + timeout))
    while true; do
        [[ "$(curl -s "${base}/api/v1/election-status" 2>/dev/null)" == *'"phase":"open"'* ]] && return 0
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

now_ms() { date +%s%3N; }

# wait_until TIMEOUT_MS COMMAND...: コマンドが成功するまで待つ（0.3 秒間隔）。
wait_until() {
    local timeout_ms="$1" start
    shift
    start="$(now_ms)"
    until "$@"; do
        (($(now_ms) - start > timeout_ms)) && return 1
        sleep 0.3
    done
}

# wait_for_log PATTERN TIMEOUT_MS LOGFILE: ログファイルに PATTERN（拡張正規表現）が出るまで待つ（0.1 秒間隔）。
wait_for_log() {
    local pattern="$1" timeout_ms="$2" log="$3" start
    start="$(now_ms)"
    until grep -Eq -- "$pattern" "$log"; do
        (($(now_ms) - start > timeout_ms)) && return 1
        sleep 0.1
    done
}

# ---------------------------------------------------------------------------
# HTTP のアサーション
# ---------------------------------------------------------------------------
#
# BASE（api のベース URL）を使う。request が STATUS / BODY を設定し、expect_status / count がそれを読む。

STATUS=""
BODY=""
_REQUEST_TMP=""

# request が使う一時ファイルを片付ける。呼び出し側（スイート）の cleanup（trap ... EXIT）から呼ぶ。
# 常に成功で返す（request を一度も呼んでいない＝$_REQUEST_TMP が空のときの「該当なし」を、失敗にしない。
# trap ... EXIT の中は set -e の対象なので、ここが失敗すると、以降の後始末が実行されないまま、
# スイート全体の終了コードが上書きされてしまう）。
common_cleanup() {
    if [[ -n "$_REQUEST_TMP" ]]; then
        rm -f "$_REQUEST_TMP"
    fi
    return 0
}

# request METHOD PATH [TOKEN] [JSON]: BASE へリクエストし、STATUS / BODY を設定する。
request() {
    [[ -n "$_REQUEST_TMP" ]] || _REQUEST_TMP="$(mktemp)"
    local method="$1" path="$2" token="${3:-}" data="${4:-}"
    local args=(-sS -o "$_REQUEST_TMP" -w '%{http_code}' -X "$method" "${BASE}${path}")
    [[ -n "$token" ]] && args+=(-H "Authorization: Bearer ${token}")
    [[ -n "$data" ]] && args+=(-H 'Content-Type: application/json' -d "$data")
    STATUS="$(curl "${args[@]}" || true)"
    BODY="$(cat "$_REQUEST_TMP")"
}

expect_status() {
    [[ "$STATUS" == "$1" ]] || fail "$2: status=${STATUS} (期待: $1) body=${BODY}"
}

# BODY 中のパターンの出現回数。
count() {
    { grep -o "$1" <<<"$BODY" || true; } | wc -l | tr -d ' '
}

# login VOTER_ID: BASE にログインし、トークンを標準出力へ返す（stub 認証。マイナンバーは固定のダミー値）。
login() {
    curl -sS -X POST "${BASE}/api/v1/login" -H 'Content-Type: application/json' \
        -d "{\"voter_id\":\"$1\",\"my_number\":\"123456789012\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p'
}
export -f login

pool_total() {
    request GET /debug/pool
    expect_status 200 "/debug/pool"
    sed -n 's/.*"total":\([0-9]*\).*/\1/p' <<<"$BODY"
}

# 「1+2+3」形式の足し算を bash で計算する（bc が無い環境でも動くように）。
bc_sum() {
    local expr total=0 n
    read -r expr || true
    IFS='+' read -ra parts <<<"${expr:-0}"
    for n in "${parts[@]}"; do total=$((total + n)); done
    echo "$total"
}
