#!/usr/bin/env bash
# 共通完了条件と、scripts/check/*.sh の全スイートを、順番に実行する。
# 各スイートの完了条件は scripts/check/*.sh にあり、exit 0 で終わることが完了の定義（docs/testing.md に対応表がある）。
set -euo pipefail

# --help: 先頭のコメント（使い方）を表示して終わる（何も起動・変更しない。scripts/check/docs.sh#8 が確認する）。
[[ "${1:-}" == -h || "${1:-}" == --help ]] && { sed -n '2,/^[^#]/{/^#/s/^# \{0,1\}//p}' "$0"; exit 0; }

cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace

NAMES=()
STATUSES=()
DURATIONS=()
overall=0

for script in scripts/check/*.sh; do
    name="$(basename "$script" .sh)"
    echo "== ${script}"
    start=$SECONDS
    if "$script"; then
        status=OK
    else
        status=FAIL
        overall=1
    fi
    duration=$((SECONDS - start))
    NAMES+=("$name")
    STATUSES+=("$status")
    DURATIONS+=("$duration")
done

fmt_duration() {
    local total="$1"
    printf '%dm%02ds' $((total / 60)) $((total % 60))
}

echo
echo "== スイートごとの結果"
printf '%-10s %-6s %8s\n' "スイート" "結果" "所要時間"
total=0
for i in "${!NAMES[@]}"; do
    printf '%-10s %-6s %8s\n' "${NAMES[$i]}" "${STATUSES[$i]}" "$(fmt_duration "${DURATIONS[$i]}")"
    total=$((total + DURATIONS[i]))
done
printf '%-10s %-6s %8s\n' "合計" "-" "$(fmt_duration "$total")"

exit "$overall"
