#!/usr/bin/env bash
# 性能計測（scripts/bench.sh）。結果は docs/benchmark.md にまとめる。
#
#   scripts/bench.sh run    [--configs "S:sealer数:api数,..."] [--out DIR]   構成ごとの負荷計測
#   scripts/bench.sh store  [--shards "1,4,8"]                 [--out DIR]   DB 直接（api なし）の計測
#   scripts/bench.sh report --out DIR                                        集計（summary.json と tables.md）
#   scripts/bench.sh trickle ...                                             低負荷（time トリガー）の計測
#
# 1 構成の流れ: DB を初期化 → sealer・api を起動 → ウォームアップ（低レート）→ 定常負荷測定（固定レート。
#   sealer が追いつけるレート）→ ドレイン（どのシャードの未封印も seal.min_ballots_after_interval 件未満になる
#   （= 時間では、もう封印されない）まで待つ。間隔が 600 秒なので最長 約 10 分。構成に :nodrain を付けると省略）→ 飽和測定（クローズドループ。最後に実施）→ 集計。
#   飽和測定は sealer が追いつけず未封印が積み上がるため、必ず最後に行う（定常・ドレインの計測を壊さない）。
#   構成の書式: S:sealer数:api数[:nodrain]（例: 4:2:1 は shard.count=4, sealer 2 プロセス, api 1 台）。
#
#   scripts/bench.sh trickle [--config S:sealer数:api数] [--rate 件/秒] [--minutes N] [--out DIR]
#     時間による封印（time）が支配的になる低負荷（1 シャードあたり 100 件/600 秒 未満）の計測。
#
# 環境変数（既定値）:
#   BENCH_WARMUP_SECONDS=20  BENCH_RATE_SECONDS=180（定常）  BENCH_SAT_SECONDS=30  BENCH_STORE_SECONDS=30
#   BENCH_STEADY_RATE=400  定常負荷のレート（全 api の合計、件/秒）  BENCH_WARMUP_RATE=100
#   BENCH_CONCURRENCY=128        負荷生成の同時接続数（全 api の合計）
#   BENCH_MAX_BALLOTS=100  BENCH_INTERVAL_SECS=600  BENCH_MIN_BALLOTS=10  BENCH_LEASE_TTL_SECS=30   （本番相当の封印設定）
#   BENCH_DRAIN_MAX_SECONDS=900  ドレインの待ち時間の上限
#   CASSANDRA_MAX_HEAP=4G  CASSANDRA_HEAP_NEW=1G     （ペアで指定。計測用の既定）
#   APP__DB__BACKEND / KEEP_KEYSPACE は scripts/lib/common.sh を参照。設定は、手元の config/local.toml などから分離し、
#   このスクリプトが APP__… で渡す（api・sealer・bench はすべて app-config から読む）。
# DB は、共用の DB（Compose プロジェクト vote-prototype、ポート 9042）とは別の専用プロジェクト vote-bench（ポート 19042）で
# 起動する（構成ごとに down -v でクリーンにする）。共用 DB と手動のキースペース vote には触れない。
#   BENCH_COMPOSE_PROJECT=vote-bench  BENCH_DB_PORT=19042
# ビルドは速度最適化の perf プロファイル（target/perf）で行う。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# 計測用の既定のヒープ（ペアで上書きする。compose の既定 1G / 256M は変えない）。
export CASSANDRA_MAX_HEAP="${CASSANDRA_MAX_HEAP:-4G}"
export CASSANDRA_HEAP_NEW="${CASSANDRA_HEAP_NEW:-1G}"

# 専用の Compose プロジェクト・ポートで DB を動かす（共用 DB を破棄しない）。
export COMPOSE_PROJECT_NAME="${BENCH_COMPOSE_PROJECT:-vote-bench}"
export DB_PORT="${BENCH_DB_PORT:-19042}"
source "$SCRIPT_DIR/lib/common.sh"
db_setup_vars
ks_init bench

WARMUP="${BENCH_WARMUP_SECONDS:-20}"
WARMUP_RATE="${BENCH_WARMUP_RATE:-100}"
STEADY_RATE="${BENCH_STEADY_RATE:-400}"
SAT="${BENCH_SAT_SECONDS:-30}"
RATE_SECS="${BENCH_RATE_SECONDS:-180}"
STORE_SECS="${BENCH_STORE_SECONDS:-30}"
CONCURRENCY="${BENCH_CONCURRENCY:-128}"
MAX_BALLOTS="${BENCH_MAX_BALLOTS:-100}"
INTERVAL="${BENCH_INTERVAL_SECS:-600}"
MIN_BALLOTS="${BENCH_MIN_BALLOTS:-10}"
LEASE_TTL="${BENCH_LEASE_TTL_SECS:-30}"
DRAIN_MAX="${BENCH_DRAIN_MAX_SECONDS:-900}"
API_BASE_PORT="${BENCH_API_BASE_PORT:-18100}"
SECRET="bench-secret-0123456789abcdef"
SEED="0909090909090909090909090909090909090909090909090909090909090909"

BIN=./target/perf
PIDS=()
SAMPLERS=()

# 投票する有権者と投票用紙は、生成した選挙データ（seedgen）の名簿から作る（api は、名簿にある有権者の、属する選挙区の
# 投票用紙にしか投票させないため）。1 人あたり 9 枚の投票用紙 = 有権者数 × 9 件の投票計画。足りなければ、計画が尽きた
# ところで止まる（結果の plan_exhausted）ので、BENCH_VOTERS（既定 20000）を増やす。

prepare_seed() {
    SEED_TMP="$(mktemp -d)"
    seed_generate "$SEED_TMP" "${BENCH_VOTERS:-20000}"
}

kill_all() {
    for pid in "${SAMPLERS[@]:-}" "${PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill -KILL "$pid" 2>/dev/null || true
    done
    for pid in "${SAMPLERS[@]:-}" "${PIDS[@]:-}"; do
        [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
    done
    SAMPLERS=()
    PIDS=()
}
cleanup() {
    rm -rf "${SEED_TMP:-}"
    kill_all
    ks_drop
    # KEEP_KEYSPACE=1 のときは、調査用に DB（とキースペース）を残す。
    [[ "${KEEP_KEYSPACE:-0}" == "1" ]] || db_fresh_stop
}
trap cleanup EXIT

now_ms() { date +%s%3N; }

# wait_until TIMEOUT_MS COMMAND...
wait_until() {
    local timeout_ms="$1" start
    shift
    start="$(now_ms)"
    until "$@"; do
        (($(now_ms) - start > timeout_ms)) && return 1
        sleep 0.5
    done
}

build() {
    echo "== ビルド（perf プロファイル）"
    cargo build -q --profile perf -p api -p sealer -p bench --features api/dev-tools
}

# ---------------------------------------------------------------------------
# サンプラー（CPU・未封印の件数）
# ---------------------------------------------------------------------------

# プロセスの CPU 時間（utime + stime、クロック tick）の合計。
proc_ticks() {
    local total=0 pid line
    local -a f
    for pid in "$@"; do
        [[ -r "/proc/$pid/stat" ]] || continue
        # 「pid (comm) 」を取り除くと、state が先頭（f[0]）、utime が f[11]、stime が f[12]。
        line="$(sed -E 's/^[0-9]+ \(.*\) //' "/proc/$pid/stat" 2>/dev/null || true)"
        [[ -n "$line" ]] || continue
        read -r -a f <<<"$line"
        total=$((total + ${f[11]:-0} + ${f[12]:-0}))
    done
    echo "$total"
}

# マシン全体の使用 tick（idle・iowait 以外）。
machine_ticks() {
    awk '/^cpu /{ busy=0; for (i=2;i<=NF;i++) busy+=$i; print busy - $5 - $6; exit }' /proc/stat
}

# sample_cpu OUTFILE DBPID: 1 秒ごとに `unix_ms,component,cpu%`（100% = 1 コア）を書く。
sample_cpu() {
    local out="$1" dbpid="$2" hz prev_t t dt name cur
    hz="$(getconf CLK_TCK)"
    declare -A prev
    prev_t="$(now_ms)"
    for name in api sealer bench; do
        # shellcheck disable=SC2046
        prev[$name]="$(proc_ticks $(pgrep -x "$name" || true))"
    done
    prev[db]="$(proc_ticks "$dbpid")"
    prev[machine]="$(machine_ticks)"
    while true; do
        sleep 1
        t="$(now_ms)"
        dt=$((t - prev_t))
        ((dt > 0)) || continue
        for name in api sealer bench db machine; do
            case "$name" in
                db) cur="$(proc_ticks "$dbpid")" ;;
                machine) cur="$(machine_ticks)" ;;
                # shellcheck disable=SC2046
                *) cur="$(proc_ticks $(pgrep -x "$name" || true))" ;;
            esac
            local delta=$((cur - ${prev[$name]}))
            ((delta < 0)) && delta=0 # プロセスが終了して合計が減った
            echo "${t},${name},$((delta * 100 * 1000 / (hz * dt)))" >>"$out"
            prev[$name]="$cur"
        done
        prev_t="$t"
    done
}

# sample_pending OUTFILE URL: 5 秒ごとに `unix_ms,未封印の合計`（/debug/pool）を書く。
sample_pending() {
    local out="$1" url="$2" total
    while true; do
        total="$(curl -s -m 10 "${url}/debug/pool" | sed -n 's/.*"total":\([0-9]*\).*/\1/p')"
        [[ -n "$total" ]] && echo "$(now_ms),${total}" >>"$out"
        sleep 5
    done
}

pending_total() {
    curl -s -m 10 "$1/debug/pool" | sed -n 's/.*"total":\([0-9]*\).*/\1/p'
}

# どのシャードの未封印も seal.min_ballots_after_interval 件未満か（= 時間では、もう封印されない。原則9）。
# 残りは締切の手続きの中でだけ封印されるので、ドレインはここまでを待つ。
pending_drained() {
    local body
    body="$(curl -s -m 10 "$1/debug/pool")" || return 1
    [[ "$body" == *'"shards"'* ]] || return 1
    ! grep -o '"pending":[0-9]*' <<<"$body" | cut -d: -f2 | awk -v min="$MIN_BALLOTS" '$1 >= min { found = 1 } END { exit !found }'
}

json_num() { sed -n "s/.*\"$1\": *\\([0-9.]*\\).*/\\1/p" "$2" | head -1; }

# ---------------------------------------------------------------------------
# 構成ごとの計測
# ---------------------------------------------------------------------------
run_config() {
    local shards="$1" sealers="$2" apis="$3" drain_mode="${4:-drain}" out="$5"
    local rate="${6:-$STEADY_RATE}" steady_secs="${7:-$RATE_SECS}" with_sat="${8:-yes}" tag="${9:-}"
    local dir="$out/s${shards}-k${sealers}-a${apis}${tag}"
    mkdir -p "$dir"
    echo
    echo "=========== 構成: shard.count=${shards} sealer=${sealers} api=${apis} ${drain_mode}${tag:+ ($tag)} ==========="
    cat >"$dir/meta.json" <<JSON
{"shards":${shards},"sealers":${sealers},"apis":${apis},"concurrency":${CONCURRENCY},
 "drain":$([[ "$drain_mode" == drain ]] && echo true || echo false),
 "steady_rate":${rate},"warmup_rate":${WARMUP_RATE},
 "max_ballots":${MAX_BALLOTS},"interval_secs":${INTERVAL},"min_ballots_after_interval":${MIN_BALLOTS},"lease_ttl_secs":${LEASE_TTL},
 "warmup_secs":${WARMUP},"steady_secs":${steady_secs},"sat_secs":${SAT},
 "cassandra_max_heap":"${CASSANDRA_MAX_HEAP}","cassandra_heap_new":"${CASSANDRA_HEAP_NEW}"}
JSON

    echo "-- DB を初期化（専用プロジェクト ${COMPOSE_PROJECT_NAME}、キースペース ${KS}）"
    db_fresh
    ks_create
    local dbpid
    # 接続先はコンテナの IP の 9042。ホスト側の公開ポート（19042）経由だと、ドライバがノード検出で得たコンテナ IP に
    # 公開ポートの番号を組み合わせて接続しようとして失敗する（コンテナ内のポートは 9042）。
    DB_NODE="$(db_node_addr)"
    dbpid="$(docker inspect -f '{{.State.Pid}}' "$("${COMPOSE[@]}" ps -q "$DB_SERVICE")")"

    export APP__APP__MODE=db APP__DB__NODES="$DB_NODE" APP__DB__KEYSPACE="$KS"
    export APP__SHARD__COUNT="$shards" APP__SEAL__MAX_BALLOTS="$MAX_BALLOTS" APP__SEAL__INTERVAL_SECS="$INTERVAL"
    export APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL="$MIN_BALLOTS"
    export APP__SEALER__LEASE_TTL_SECS="$LEASE_TTL" APP__SEALER__SIGNING_SEED="$SEED" APP__SESSION__SECRET="$SECRET"
    # 開始時刻を過去にして、アンカーのリースを持つ sealer が起動直後に自動で open にする（原則17・18）。
    export APP__ELECTION__VOTING_OPENS_AT="2020-01-01T00:00:00+00:00"
    export RUST_LOG="info,tower_http=warn"

    echo "-- sealer を ${sealers} プロセス起動"
    local i
    for i in $(seq 1 "$sealers"); do
        APP__SEALER__ID="bench-sealer-${i}" "$BIN/sealer" >"$dir/sealer-${i}.log" 2>&1 &
        PIDS+=($!)
    done
    all_leased() {
        local n
        n="$(cat "$dir"/sealer-*.log | { grep -a 'リースを取得しました' || true; } | { grep -ao 'shard=[0-9]*' || true; } | sort -u | wc -l)"
        [[ "$n" -eq "$shards" ]]
    }
    wait_until 300000 all_leased || fail "全シャードのリースが取得されません"

    echo "-- api を ${apis} 台起動"
    local targets="" port
    for i in $(seq 0 $((apis - 1))); do
        port=$((API_BASE_PORT + i))
        APP__API__PORT="$port" "$BIN/api" >"$dir/api-${i}.log" 2>&1 &
        PIDS+=($!)
        targets="${targets:+$targets,}http://127.0.0.1:${port}"
    done
    api_ready() {
        local p
        for i in $(seq 0 $((apis - 1))); do
            p=$((API_BASE_PORT + i))
            curl -fsS -o /dev/null "http://127.0.0.1:${p}/healthz" 2>/dev/null || return 1
            [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${p}/api/v1/chains/0/head")" == 200 ]] || return 1
        done
    }
    wait_until 60000 api_ready || fail "api が起動しません"
    local first_api="http://127.0.0.1:${API_BASE_PORT}"
    wait_election_open "$first_api" 30 || fail "自動で open になりませんでした（election.voting_opens_at の自動遷移）"

    : >"$dir/cpu.csv"
    : >"$dir/pending.csv"
    sample_cpu "$dir/cpu.csv" "$dbpid" &
    SAMPLERS+=($!)
    sample_pending "$dir/pending.csv" "$first_api" &
    SAMPLERS+=($!)

    local idx=0
    echo "-- ウォームアップ（${WARMUP_RATE} 件/秒、${WARMUP} 秒）"
    "$BIN/bench" load --targets "$targets" --mode rate --rate "$WARMUP_RATE" \
        --concurrency "$CONCURRENCY" --duration "$WARMUP" --start-index "$idx" --label warmup --out "$dir/warmup.json"
    idx="$(json_num next_index "$dir/warmup.json")"

    echo "-- 定常負荷測定（オープンループ、${rate} 件/秒、${steady_secs} 秒）"
    "$BIN/bench" load --targets "$targets" --mode rate --rate "$rate" \
        --concurrency "$CONCURRENCY" --duration "$steady_secs" --start-index "$idx" --label rate --out "$dir/rate.json"
    idx="$(json_num next_index "$dir/rate.json")"
    "${COMPOSE[@]}" exec -T "$DB_SERVICE" nodetool proxyhistograms >"$dir/proxyhistograms-after-steady.txt" 2>&1 || true

    if [[ "$drain_mode" == drain ]]; then
        echo "-- ドレイン（どのシャードの未封印も ${MIN_BALLOTS} 件未満になるまで。最長 ${DRAIN_MAX} 秒。間隔は ${INTERVAL} 秒）"
        local drain_start drain_end drained=false
        drain_start="$(now_ms)"
        while (($(now_ms) - drain_start < DRAIN_MAX * 1000)); do
            if pending_drained "$first_api"; then
                drained=true
                break
            fi
            sleep 5
        done
        drain_end="$(now_ms)"
        printf '{"drain_start_unix_ms":%s,"drain_end_unix_ms":%s,"drained":%s,"waited_s":%s}\n' \
            "$drain_start" "$drain_end" "$drained" "$(((drain_end - drain_start) / 1000))" >"$dir/drain.json"
        echo "   ドレイン: drained=${drained}（$(((drain_end - drain_start) / 1000)) 秒）"
    fi

    if [[ "$with_sat" == yes ]]; then
        echo "-- 飽和測定（クローズドループ、同時接続 ${CONCURRENCY}、${SAT} 秒。最後に実施）"
        "$BIN/bench" load --targets "$targets" --mode saturate --concurrency "$CONCURRENCY" \
            --duration "$SAT" --start-index "$idx" --label sat --out "$dir/sat.json"
        # 飽和では sealer が追いつかず、未封印が残る。sat 終了直後の様子（未封印の件数）を記録する。
        pending_total "$first_api" >"$dir/pending-after-sat.txt" || true
        "${COMPOSE[@]}" exec -T "$DB_SERVICE" nodetool proxyhistograms >"$dir/proxyhistograms-after-sat.txt" 2>&1 || true
    fi

    # 停止: サンプラー → api → sealer。sealer の正常停止（SIGTERM）は未封印の票をフラッシュしない（原則9）ので、
    # 未封印が残っていてもすぐに終わる（DB はこの後破棄する）。
    for pid in "${SAMPLERS[@]}"; do kill -KILL "$pid" 2>/dev/null || true; done
    SAMPLERS=()
    local pid
    for pid in "${PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
    PIDS=()

    "$BIN/bench" summarize --dir "$dir"
}

run_store() {
    local shards="$1" out="$2"
    echo
    echo "=========== DB 直接: shard.count=${shards} ==========="
    db_fresh
    APP__DB__NODES="$(db_node_addr)" APP__SHARD__COUNT="$shards" "$BIN/bench" store --concurrency "$CONCURRENCY" \
        --duration "$STORE_SECS" --out "$out/store-${shards}.json"
}

write_environment() {
    local out="$1"
    {
        echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "uname: $(uname -srm)"
        echo "nproc: $(nproc)"
        echo "mem: $(awk '/MemTotal/ {printf "%.1f GB", $2/1024/1024}' /proc/meminfo)"
        echo "cpu: $(awk -F: '/model name|Model name|CPU part/ {print $2; exit}' /proc/cpuinfo | xargs)"
        echo "docker: $(docker --version)"
        echo "db_backend: ${DB_BACKEND}"
        echo "cassandra_heap: ${CASSANDRA_MAX_HEAP} / ${CASSANDRA_HEAP_NEW}"
        echo "settings: seal.max_ballots=${MAX_BALLOTS} seal.interval_secs=${INTERVAL} seal.min_ballots_after_interval=${MIN_BALLOTS} sealer.lease_ttl_secs=${LEASE_TTL}"
        echo "git: $(git rev-parse --short HEAD 2>/dev/null || echo 'no commits')"
    } >"$out/environment.txt"
}

# ---------------------------------------------------------------------------
cmd="${1:-}"
shift || true
OUT=""
CONFIGS="1:1:1,4:2:1,8:2:1,1:1:2:nodrain,4:2:2:nodrain,4:2:4:nodrain,8:2:2:nodrain,8:2:4:nodrain"
TRICKLE_CONFIG="4:2:1"
TRICKLE_RATE="0.4"
TRICKLE_MINUTES=25
SHARDS_LIST="1,4,8"
while (($#)); do
    case "$1" in
        --out) OUT="$2"; shift 2 ;;
        --configs) CONFIGS="$2"; shift 2 ;;
        --shards) SHARDS_LIST="$2"; shift 2 ;;
        --config) TRICKLE_CONFIG="$2"; shift 2 ;;
        --rate) TRICKLE_RATE="$2"; shift 2 ;;
        --minutes) TRICKLE_MINUTES="$2"; shift 2 ;;
        *) fail "不明なオプション: $1" ;;
    esac
done
OUT="${OUT:-bench/results/$(date +%Y%m%d-%H%M%S)}"

case "$cmd" in
    run)
        db_check_docker
        build
        prepare_seed
        mkdir -p "$OUT"
        write_environment "$OUT"
        IFS=',' read -ra cfgs <<<"$CONFIGS"
        for cfg in "${cfgs[@]}"; do
            IFS=':' read -r s k a mode <<<"$cfg"
            run_config "$s" "$k" "$a" "${mode:-drain}" "$OUT"
        done
        "$BIN/bench" report --root "$OUT" >"$OUT/tables.md"
        echo "完了: $OUT/tables.md"
        ;;
    trickle)
        db_check_docker
        build
        prepare_seed
        mkdir -p "$OUT"
        write_environment "$OUT"
        IFS=':' read -r s k a <<<"$TRICKLE_CONFIG"
        # 低負荷の観測なので、ウォームアップは 1 件だけにする（大量の票が count トリガーで封印されるのを避ける）。
        WARMUP=1
        WARMUP_RATE=1
        run_config "$s" "$k" "$a" drain "$OUT" "$TRICKLE_RATE" $((TRICKLE_MINUTES * 60)) no "-trickle"
        "$BIN/bench" report --root "$OUT" >"$OUT/tables.md"
        echo "完了: $OUT/tables.md"
        ;;
    store)
        db_check_docker
        build
        prepare_seed
        mkdir -p "$OUT"
        write_environment "$OUT"
        IFS=',' read -ra shard_list <<<"$SHARDS_LIST"
        for s in "${shard_list[@]}"; do
            run_store "$s" "$OUT"
        done
        "$BIN/bench" report --root "$OUT" >"$OUT/tables.md"
        echo "完了: $OUT/tables.md"
        ;;
    report)
        [[ -d "$OUT" ]] || fail "--out に集計する結果ディレクトリを指定してください"
        build
        for d in "$OUT"/*/; do
            [[ -f "$d/meta.json" ]] && "$BIN/bench" summarize --dir "$d"
        done
        "$BIN/bench" report --root "$OUT" >"$OUT/tables.md"
        echo "完了: $OUT/tables.md"
        ;;
    *)
        fail "使い方: scripts/bench.sh run|store|trickle|report [--configs ...] [--shards ...] [--config ...] [--rate N] [--minutes N] [--out DIR]"
        ;;
esac
