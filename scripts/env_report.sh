#!/usr/bin/env bash
# 想定する環境の版を、リポジトリのファイルと各ツールの --version から生成する（docs/environment.md に貼る）。
#
#   scripts/env_report.sh            生成した Markdown を表示する
#   scripts/env_report.sh --update   docs/environment.md の env_report の区間を、生成した内容で置き換える
#   scripts/env_report.sh --check    docs/environment.md の区間と、生成した内容が一致するか確認する（差分があれば表示して 1）
#   scripts/env_report.sh --host     この機械だけの参考情報（OS・CPU・Docker など。版を固定しないもの）を表示する（文書には貼らない）
#
# 生成元（手で版を書かない）:
#   - rust-toolchain.toml   Rust の版・コンポーネント・ターゲット（rustup が自動で合わせる）
#   - crates/web/Trunk.toml trunk-version（Trunk の版）
#   - 各ツールの --version  rustc / cargo / rustfmt / clippy / trunk（上の 2 つで版を固定しているので、手順どおりに
#                           入れた環境なら、どの機械でも同じ出力になる）
#   - Cargo.toml と Cargo.lock  ワークスペースの依存（[workspace.dependencies]）の、実際に使われている版
#   - docker-compose.yml    DB のコンテナイメージ
# Docker 本体や OS の版は機械ごとに違うので、区間には入れない（--host で表示する）。
# scripts/check/docs.sh#6 が --check を実行する。
set -euo pipefail

case "${1:-}" in
    -h | --help)
        sed -n '2,/^[^#]/{/^#/s/^# \{0,1\}//p}' "$0"
        exit 0
        ;;
esac

cd "$(dirname "$0")/.."

DOC=docs/environment.md
BEGIN_MARK='<!-- env_report:begin（scripts/env_report.sh --update が書き換える。手で編集しない） -->'
END_MARK='<!-- env_report:end -->'

# toml_value FILE KEY: 「KEY = "値"」または「KEY = [..]」の右辺（引用符と角括弧を除く）。
toml_value() {
    sed -nE "s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*(.*)$/\1/p" "$1" | head -1 | tr -d '"[]' | sed -E 's/[[:space:]]*,[[:space:]]*/, /g; s/[[:space:]]+$//'
}

# tool_version NAME CMD...: コマンドの --version の 1 行目。無ければ「見つかりません」。
tool_version() {
    local name="$1" out
    shift
    if out="$("$@" 2>/dev/null | head -1)" && [[ -n "$out" ]]; then
        printf '| %s | `%s` |\n' "$name" "$out"
    else
        printf '| %s | （見つかりません: %s） |\n' "$name" "$*"
    fi
}

# lock_versions CRATE: Cargo.lock にある、その名前のパッケージの版（複数あればすべて）。
lock_versions() {
    awk -v name="$1" '
        /^\[\[package\]\]/ { pkg = "" }
        /^name = / { gsub(/"/, "", $3); pkg = $3 }
        /^version = / && pkg == name { gsub(/"/, "", $3); print $3 }
    ' Cargo.lock | sort -uV | paste -sd, - | sed 's/,/, /g'
}

report() {
    echo '### Rust ツールチェーン（`rust-toolchain.toml`）'
    echo
    echo '| 項目 | 値 |'
    echo '|---|---|'
    echo "| channel | \`$(toml_value rust-toolchain.toml channel)\` |"
    echo "| components | $(toml_value rust-toolchain.toml components) |"
    echo "| targets | $(toml_value rust-toolchain.toml targets) |"
    echo "| edition（\`Cargo.toml\`） | $(toml_value Cargo.toml edition) |"
    echo
    echo '### ツールの版（各ツールの `--version`）'
    echo
    echo '| ツール | 出力 |'
    echo '|---|---|'
    tool_version rustc rustc --version
    tool_version cargo cargo --version
    tool_version rustfmt rustfmt --version
    tool_version clippy cargo clippy --version
    echo "| trunk（\`crates/web/Trunk.toml\` の trunk-version） | \`$(toml_value crates/web/Trunk.toml trunk-version)\` |"
    tool_version trunk trunk --version
    echo
    echo '### ワークスペースの依存（`Cargo.toml` の `[workspace.dependencies]` の名前と、`Cargo.lock` にある版。間接依存の版も並ぶ）'
    echo
    echo '| クレート | 版 |'
    echo '|---|---|'
    local name
    while read -r name; do
        echo "| ${name} | $(lock_versions "$name") |"
    done < <(awk '/^\[workspace.dependencies\]/ { on = 1; next } /^\[/ { on = 0 } on && /^[a-z]/ { print $1 }' Cargo.toml | sort)
    echo
    echo '### DB のコンテナイメージ（`docker-compose.yml`）'
    echo
    echo '| サービス | イメージ |'
    echo '|---|---|'
    awk '/^  [a-z-]+:$/ { svc = $1; sub(/:$/, "", svc) } /^    image:/ { print "| " svc " | `" $2 "` |" }' docker-compose.yml
}

block() {
    echo "$BEGIN_MARK"
    echo
    report
    echo
    echo "$END_MARK"
}

# 文書の区間（開始と終了の印を含む）。
doc_block() {
    awk -v b="$BEGIN_MARK" -v e="$END_MARK" '$0 == b { on = 1 } on { print } $0 == e { on = 0 }' "$DOC"
}

host() {
    echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "uname: $(uname -srm)"
    echo "nproc: $(nproc 2>/dev/null || echo '?')"
    echo "docker: $(docker --version 2>/dev/null || echo '見つかりません')"
    echo "docker compose: $(docker compose version 2>/dev/null || echo '見つかりません')"
    echo "bash: ${BASH_VERSION}"
    echo "curl: $(curl --version 2>/dev/null | head -1 || echo '見つかりません')"
    echo "python3: $(python3 --version 2>/dev/null || echo '見つかりません')"
    # 仮想アドレス空間のビット数（39 なら ScyllaDB は起動できない。docs/environment.md）。x86_64 は /proc/cpuinfo に出る。
    echo "virtual address: $(grep -m1 -oE '[0-9]+ bits virtual' /proc/cpuinfo 2>/dev/null || echo '不明（/proc/cpuinfo に記載なし。aarch64 など）')"
}

case "${1:-}" in
    "") block ;;
    --host) host ;;
    --update)
        [[ -f "$DOC" ]] || {
            echo "$DOC がありません" >&2
            exit 1
        }
        grep -qxF "$BEGIN_MARK" "$DOC" && grep -qxF "$END_MARK" "$DOC" || {
            echo "$DOC に env_report の区間（開始と終了の印）がありません" >&2
            exit 1
        }
        new="$(block)"
        tmp="$(mktemp)"
        awk -v b="$BEGIN_MARK" -v e="$END_MARK" -v f=/dev/stdin '
            $0 == b { while ((getline line < f) > 0) print line; skip = 1; next }
            skip && $0 == e { skip = 0; next }
            !skip { print }
        ' "$DOC" <<<"$new" >"$tmp"
        mv "$tmp" "$DOC"
        echo "更新しました: $DOC"
        ;;
    --check)
        if diff -u <(doc_block) <(block); then
            echo "env_report: $DOC と一致しています"
        else
            echo "env_report: $DOC と、再生成した内容が一致しません（上の差分。手順どおりの環境なら scripts/env_report.sh --update）" >&2
            exit 1
        fi
        ;;
    *)
        echo "使い方: scripts/env_report.sh [--update|--check|--host|--help]" >&2
        exit 2
        ;;
esac
