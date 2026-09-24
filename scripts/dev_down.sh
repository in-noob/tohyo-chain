#!/usr/bin/env bash
# scripts/dev_up.sh が起動したものをすべて止める（何も起動していなくても成功する）。
#
#   画面（trunk）→ api → sealer の順に止める。api と sealer には SIGTERM を送り、残りの票を締切フラッシュして
#   から終了するのを待つ（sealer はリースも解放する）。cassandra モードでは docker compose stop（ボリュームは削除しない）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

DEV_DIR=.dev
PIDS_FILE="$DEV_DIR/pids"
GRACE_SECS=20

alive() { kill -0 "$1" 2>/dev/null; }

# pid が、その役割のプロセスであること（PID の再利用で無関係のプロセスを止めないため）。
is_ours() {
    local role="$1" pid="$2" cmd
    cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
    [[ "$cmd" == *"$role"* ]]
}

# wait_gone PID SECS: PID が消えるまで待つ。消えたら 0。
wait_gone() {
    local pid="$1" deadline=$((SECONDS + $2))
    while alive "$pid"; do
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# stop_role ROLE PID GROUP_KILL: SIGTERM → 猶予のあいだ待つ → まだ残っていれば SIGKILL。
stop_role() {
    local role="$1" pid="$2" group="$3" target="$2"
    if ! alive "$pid"; then
        echo "  ${role}: すでに止まっています"
        return 0
    fi
    if ! is_ours "$role" "$pid"; then
        echo "  ${role}: PID ${pid} は別のプロセスなので止めません" >&2
        return 0
    fi
    # trunk は cargo / wasm-bindgen などの子プロセスを持つので、セッション（プロセスグループ）ごと止める。
    [[ "$group" == group ]] && target="-$pid"
    kill -TERM -- "$target" 2>/dev/null || true
    if wait_gone "$pid" "$GRACE_SECS"; then
        echo "  ${role}: 停止しました（SIGTERM）"
    else
        echo "  警告: ${role} が ${GRACE_SECS} 秒以内に終了しないので SIGKILL します" >&2
        kill -KILL -- "$target" 2>/dev/null || true
        wait_gone "$pid" 5 || echo "  警告: ${role}（PID ${pid}）を止められません" >&2
    fi
    # グループの残り（子プロセス）も確実に片付ける。
    [[ "$group" == group ]] && kill -KILL -- "$target" 2>/dev/null || true
}

echo "== 停止"
if [[ -f "$PIDS_FILE" ]]; then
    for role in trunk api sealer; do
        pid="$(awk -v r="$role" '$1 == r {print $2}' "$PIDS_FILE" | tail -1)"
        [[ -n "$pid" ]] || continue
        if [[ "$role" == trunk ]]; then
            stop_role "$role" "$pid" group
        else
            stop_role "$role" "$pid" single
        fi
    done
    rm -f "$PIDS_FILE"
else
    echo "  起動しているプロセスはありません"
fi

if [[ "$(cat "$DEV_DIR/mode" 2>/dev/null || true)" == cassandra ]]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        docker compose --profile cassandra --profile scylla stop >/dev/null 2>&1 \
            && echo "  DB: docker compose stop（ボリュームは残しています）" \
            || echo "  警告: docker compose stop に失敗しました" >&2
    else
        echo "  警告: docker に接続できないため、DB のコンテナは止めていません" >&2
    fi
fi
rm -f "$DEV_DIR/mode"
echo "OK: 停止しました"
