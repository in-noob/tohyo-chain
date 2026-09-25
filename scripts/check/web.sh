#!/usr/bin/env bash
# web スイート: 画面（対応表: docs/testing.md）。
#   1. flow モジュールの単体テスト・純粋性と依存方向（原則5）・wasm 向け clippy
#   2. デザイントークン（色の直接指定がないこと）・テーマのテスト（コントラスト比など）・選択状態を色だけで表さない部品
#   3. trunk build --release（1 回だけビルドし、その成果物に対して、dist のサイズ・インラインスクリプトの位置・CSS のトークンを確認する）
#   4. 白票: 候補者一覧の最後に白票の選択肢（allow_blank=false なら出さない）・確認画面の文言・ビューアの別の行
#      （API と集計の確認は core.sh#10）
#   5. 投票のやり直し（再投票。ADR 0022）: 完了画面の「投票をやり直す」（許可・期間内だけ）・固定の順番の一覧・上限に達した
#      投票用紙は選べず理由を表示・確認画面は「前回の投票内容を変更します」・ビューアの「#<前の票> を置き換え（A→B）」
#      （API・封印・集計は core.sh#11・chain.sh#9）
# ブラウザでの動作確認は docs/manual_check_step5.md・docs/manual_check_step15.md（手動）で行う。
# 注意: trunk は crates/web で実行する（このスクリプトは、ワークスペースのルートから実行する）。
set -euo pipefail

# --help: 先頭のコメント（使い方）を表示して終わる（何も起動・変更しない。scripts/check/docs.sh#8 が確認する）。
[[ "${1:-}" == -h || "${1:-}" == --help ]] && { sed -n '2,/^[^#]/{/^#/s/^# \{0,1\}//p}' "$0"; exit 0; }

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR/.."
source scripts/lib/common.sh

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "== 0. 前提の確認"
command -v rustup >/dev/null 2>&1 \
    || fail $'rustup が見つかりません（wasm ターゲットの確認に必要）。\n  https://rustup.rs からインストールしてください。'
rustup target list --installed 2>/dev/null | grep -qx 'wasm32-unknown-unknown' \
    || fail $'wasm ターゲット wasm32-unknown-unknown がインストールされていません。\n  次を実行してください: rustup target add wasm32-unknown-unknown'
command -v trunk >/dev/null 2>&1 \
    || fail $'trunk が見つかりません。\n  次を実行してください: cargo install trunk --locked'
# Trunk はカレントディレクトリの Cargo.toml からルートパッケージを探す。ワークスペースのルートは
# 仮想マニフェスト（パッケージなし）なので、index.html と Trunk.toml は crates/web 直下に置き、
# trunk も crates/web で実行する（ルートで実行すると「could not find the root package of the target crate」で失敗する）。
for f in index.html Trunk.toml; do
    [[ -f "crates/web/$f" ]] || fail "crates/web/$f がありません（trunk 用のファイルは crates/web 直下に置く）"
    [[ ! -e "$f" ]] || fail "ルート直下に $f があります。crates/web 直下に置いてください"
done

# ===========================================================================
# 1. flow の単体テスト・純粋性・wasm clippy
# ===========================================================================
check_flow_and_purity() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }

    echo "== 1-1. flow モジュールのテスト"
    REQUIRED=(
        done_message_is_exactly_the_specified_text
        current_is_always_the_first_unvoted_in_display_order
        current_is_none_when_everything_is_voted_or_there_are_no_ballots
        a_ballot_voted_out_of_order_never_sends_the_user_back_or_around
        progress_counts_the_current_ballot_in_display_order
        progress_text_is_built_from_the_configured_template
        ballot_states_mark_done_current_and_upcoming
        mark_voted_updates_only_the_target_and_keeps_the_input_intact
        continue_goes_to_the_next_unvoted_in_display_order_or_finishes
        a_full_session_visits_each_ballot_once_in_display_order_then_finishes
        reloading_resumes_from_the_first_unvoted_ballot
        logged_out_users_are_sent_to_login
        a_ballot_page_only_opens_for_the_current_ballot
        done_page_only_right_after_a_vote
        happy_path_pick_confirm_submit_accept
        submit_is_only_possible_from_confirming
        outcomes_route_the_user_and_reset_the_phase
        recoverable_failures_stay_on_the_page
        voter_id_validation_matches_the_server_rules
    )
    if ! out="$(cargo test -p web --lib flow:: 2>&1)"; then
        echo "$out" >&2
        fail "flow のテストが失敗しました"
    fi
    passed="$(grep -Ec '^test flow::.* \.\.\. ok$' <<<"$out" || true)"
    failed="$(grep -Ec '^test flow::.* \.\.\. FAILED$' <<<"$out" || true)"
    for name in "${REQUIRED[@]}"; do
        grep -Eq "^test flow::tests::${name} \.\.\. ok$" <<<"$out" \
            || fail "必須テスト ${name} が成功していません（存在しない可能性があります）"
    done
    [[ "$failed" -eq 0 ]] || fail "失敗したテストが ${failed} 件あります"
    echo "flow tests: ${passed} passed, ${failed} failed（必須 ${#REQUIRED[@]} 件を含む）"
    # 投票する順番は固定（先頭の未投票へ進む）。折り返し（wrap-around）の動きは、コードにもテストにも無いこと。
    if grep -nE 'wrap|fn next_unvoted|next_unvoted_contest' crates/web/src/flow.rs; then
        fail "flow に、折り返し（wrap-around）や next_unvoted_contest が残っています（順番は固定: 先頭の未投票へ進む）"
    fi

    echo "== 1-2. 純粋性と依存方向の確認"
    if grep -nE '^\s*use\s+(leptos|leptos_router|gloo_net|web_sys|wasm_bindgen|js_sys)' \
        crates/web/src/flow.rs crates/web/src/error.rs; then
        fail "flow / error が UI・ブラウザ API に依存しています"
    fi
    if grep -rn 'wasm_bindgen_test' crates/web; then
        fail "wasm-bindgen-test は使わない方針です"
    fi
    workspace_deps="$(cargo tree -p web --depth 1 --edges normal \
        | grep -E '\(/[^)]*/crates/[^)]*\)' | grep -v '^web v' || true)"
    forbidden="$(grep -v 'shared-types' <<<"$workspace_deps" || true)"
    if [[ -n "$forbidden" ]]; then
        echo "$forbidden" >&2
        fail "web が shared-types 以外のワークスペースクレートに依存しています（原則5）"
    fi
    echo "純粋性・依存方向: OK"

    echo "== 1-3. clippy（wasm32-unknown-unknown）"
    cargo clippy -q -p web --target wasm32-unknown-unknown -- -D warnings

    echo "OK: web#1 flow・純粋性・wasm clippy"
)

# ===========================================================================
# 2. デザイントークン・テーマ
# ===========================================================================
check_design_tokens_and_theme() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    CSS=crates/web/style.css
    [[ -f "$CSS" ]] || fail "$CSS がありません"

    HEX_OR_FUNC='#[0-9a-fA-F]{3,8}\b|\b(rgb|rgba|hsl|hsla|hwb|lab|lch|oklab|oklch|color-mix)\('
    NAMED='\b(white|black|red|green|blue|gray|grey|silver|yellow|orange|purple|pink|brown|cyan|magenta|navy|teal|maroon|olive|lime|aqua|fuchsia|transparent|currentcolor)\b'

    strip_css_comments() {
        awk '
            BEGIN { in_comment = 0 }
            {
                line = $0; out = ""
                while (length(line) > 0) {
                    if (in_comment) {
                        i = index(line, "*/")
                        if (i == 0) { line = "" } else { line = substr(line, i + 2); in_comment = 0 }
                    } else {
                        i = index(line, "/*")
                        if (i == 0) { out = out line; line = "" }
                        else { out = out substr(line, 1, i - 1); line = substr(line, i + 2); in_comment = 1 }
                    }
                }
                print out
            }' "$1"
    }

    echo "== 2-1. 色が、トークン以外の場所に直接書かれていない"
    outside_tokens="$(strip_css_comments "$CSS" | awk '
        /^:root \{/ || /^\[data-theme="dark"\] \{/ { skip = 1; next }
        skip && /^\}/ { skip = 0; next }
        !skip { print NR ": " $0 }')"
    [[ -n "$outside_tokens" ]] || fail "トークン以外の CSS が空です（ブロックの除き方を確認してください）"
    if hits="$(grep -nE "$HEX_OR_FUNC|$NAMED" <<<"$outside_tokens")"; then
        echo "$hits" >&2
        fail "CSS のトークン以外の場所に、色が直接書かれています（var(--color-…) を使ってください）"
    fi
    defs="$(strip_css_comments "$CSS" | grep -cE '^\s*--color-bg:' || true)"
    [[ "$defs" == 2 ]] || fail "--color-bg の定義が 2 か所（:root とダーク）ではありません（${defs}）"
    if hits="$(strip_css_comments "$CSS" | awk '
        /^:root \{/ || /^\[data-theme="dark"\] \{/ { skip = 1; next }
        skip && /^\}/ { skip = 0; next }
        !skip { print NR ": " $0 }' | grep -E '(^|[ ;{])(color|background(-color)?|border(-[a-z]+)*-color|outline-color|fill|stroke|accent-color):[^;]*;' \
        | grep -vE '(color|background|fill|stroke|accent-color): *(var\(--color-[a-z-]+\)|none|inherit|unset);|border(-[a-z]+)*-color: *var\(--color-[a-z-]+\);')"; then
        echo "$hits" >&2
        fail "色のプロパティに、var(--color-…) 以外の値があります"
    fi
    echo "CSS: トークン以外に色の直接指定なし（色のトークンは :root とダークの 2 か所）: OK"

    rust_hits=""
    while IFS= read -r file; do
        found="$(awk '/#\[cfg\(test\)\]/ { exit } { print FILENAME ":" NR ": " $0 }' "$file" \
            | grep -E "\"[^\"]*#[0-9a-fA-F]{3,8}\b|(rgb|rgba|hsl|hsla)\(|style *=|\.style\(|set_property|\bcolor *:|background *:" || true)"
        rust_hits="${rust_hits}${found}"
    done < <(find crates/web/src -name '*.rs')
    if [[ -n "$rust_hits" ]]; then
        echo "$rust_hits" >&2
        fail "Rust（画面のコード）に、色・スタイルが直接書かれています（色は style.css のトークンだけで定義する）"
    fi
    echo "Rust: 色・インラインスタイルの直接指定なし: OK"

    if grep -nE "$HEX_OR_FUNC|<style|style *=|theme-color" crates/web/index.html; then
        fail "index.html に、色・スタイルが直接書かれています"
    fi
    echo "index.html: 色の直接指定なし: OK"

    echo "== 2-2. テーマのテスト（web::theme）"
    REQUIRED=(
        stored_values_are_parsed_and_anything_else_means_system
        only_system_follows_the_os_setting
        the_switch_offers_light_dark_and_follow_the_os
        the_inline_script_in_the_head_uses_the_same_key_and_attribute_before_the_stylesheet
        the_root_defines_colors_spacing_radius_and_font_size_tokens
        the_dark_theme_overrides_only_color_tokens_and_all_of_them
        the_contrast_formula_matches_the_wcag_reference_values
        color_tokens_are_plain_hex_values
        text_colors_meet_wcag_aa_4_5_to_1_in_both_themes
        borders_and_focus_meet_3_to_1_in_both_themes
        the_light_theme_is_white_based
        no_color_literals_outside_the_token_blocks
    )
    if ! out="$(cargo test -p web --lib theme:: 2>&1)"; then
        echo "$out" >&2
        fail "テーマのテストが失敗しました（コントラスト比の不足は、上のメッセージに、どのトークンの組み合わせかが出ます）"
    fi
    for name in "${REQUIRED[@]}"; do
        grep -Eq "^test theme::tests::${name} \.\.\. ok$" <<<"$out" \
            || fail "必須テスト ${name} が成功していません（存在しない可能性があります）"
    done
    passed="$(grep -Ec '^test theme::.* \.\.\. ok$' <<<"$out" || true)"
    echo "theme tests: ${passed} passed（必須 ${#REQUIRED[@]} 件を含む。両方のテーマで、文字と背景のコントラスト比 4.5:1 以上）"

    echo "== 2-3. 選択状態を色だけで表さない部品がある"
    for needle in '.candidate.selected' '.theme-option.selected' '.contest-list li.current' '.status.voted'; do
        grep -qF "$needle" "$CSS" || fail "$CSS に $needle がありません"
    done
    grep -q 'candidate selected' crates/web/src/pages/vote.rs && grep -q '✓ 選択中' crates/web/src/pages/vote.rs \
        || fail "候補者の選択中の表示（太い枠 + 「✓ 選択中」）が vote.rs にありません"
    grep -q 'aria-pressed' crates/web/src/app.rs && grep -q '"✓ "' crates/web/src/app.rs \
        || fail "テーマの切り替えボタンの選択中の表示（aria-pressed + ✓）が app.rs にありません"
    grep -q 'ballot_state.mark()' crates/web/src/pages/progress.rs || fail "進捗の状態の記号が progress.rs にありません"
    grep -q 'alert_text' crates/web/src/pages/login.rs crates/web/src/pages/vote.rs crates/web/src/pages/chain.rs \
        || fail "エラーの「⚠」（alert_text）が使われていません"
    echo "選択中の候補者（太い枠・✓ 選択中）・テーマの切り替え（aria-pressed・✓）・進捗（記号・左の線）・エラー（⚠）: OK"

    echo "OK: web#2 デザイントークン・テーマ"
)

# ===========================================================================
# 3. trunk build --release（1 回だけビルドし、dist のサイズとテーマの両方を確認する）
# ===========================================================================
check_trunk_build() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    echo "== 3. trunk build --release（画面のビルド）"
    cargo build -q -p app-config
    DIST="crates/web/dist"
    rm -rf "$DIST"
    if ! build_log="$(cd crates/web && trunk build --release 2>&1)"; then
        echo "$build_log" >&2
        fail "trunk build --release が失敗しました"
    fi

    [[ -f "$DIST/index.html" ]] || fail "dist/index.html がありません"
    wasm_file="$(find "$DIST" -maxdepth 1 -name '*.wasm' | head -1)"
    [[ -n "$wasm_file" ]] || fail "dist に .wasm がありません"
    # API の接続先は相対パス（/api）で、オリジンを成果物に焼き込まない。
    if grep -rlq 'localhost:8080' "$DIST"; then
        fail "成果物に localhost:8080 が含まれています（API は相対パスで呼ぶこと）"
    fi
    echo "dist/ のサイズ:"
    du -sh "$DIST" | awk '{print "  合計 " $1}'
    for f in "$DIST"/*; do
        printf '  %8d bytes  %s\n' "$(stat -c %s "$f")" "$(basename "$f")"
    done
    printf '  wasm（gzip 圧縮後の目安）: %d bytes\n' "$(gzip -9 -c "$wasm_file" | wc -c)"

    # ビルドされた index.html でも、テーマのスクリプトが <head> の中で、スタイルシート・WASM より前にある。
    script_line="$(grep -n 'data-theme' "$DIST/index.html" | head -1 | cut -d: -f1 || true)"
    [[ -n "$script_line" ]] || fail "dist/index.html に、インラインスクリプト（data-theme）が残っていません"
    css_line="$(grep -nE 'rel="stylesheet"|\.css' "$DIST/index.html" | head -1 | cut -d: -f1 || true)"
    [[ -z "$css_line" || "$script_line" -le "$css_line" ]] || fail "dist/index.html で、スクリプトがスタイルシートより後にあります"
    grep -q 'localStorage' "$DIST/index.html" || fail "dist/index.html に localStorage の処理がありません"
    cat "$DIST"/*.css | grep -q 'data-theme' || fail "dist の CSS に [data-theme] がありません"
    cat "$DIST"/*.css | grep -q -- '--color-bg' || fail "dist の CSS に色のトークンがありません"
    ls "$DIST"/*.wasm >/dev/null 2>&1 || fail "dist に wasm がありません"
    echo "trunk build --release: 成功（dist: $(du -sh "$DIST" | cut -f1)）。インラインスクリプトとトークンが dist に残っている"

    echo "OK: web#3 trunk build --release"
)

# ===========================================================================
# 4. 白票（画面。API・封印・集計は core.sh#10）
# ===========================================================================
check_blank_choice() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    echo "== 4-1. 白票の表示ロジックのテスト（必須ケース）"
    REQUIRED=(
        flow::tests::blank_option_is_listed_last_only_when_allowed
        flow::tests::confirming_a_blank_vote_says_so_explicitly
        flow::tests::a_blank_vote_goes_through_pick_confirm_submit_with_the_reserved_id
        chain::tests::a_blank_ballot_is_shown_with_the_blank_name_not_as_a_candidate
        chain::tests::choice_counts_put_blank_on_its_own_row_after_the_candidates
        labels::tests::defaults_match_config_default_toml
    )
    if ! out="$(cargo test -p web --lib 2>&1)"; then
        echo "$out" >&2
        fail "web のテストが失敗しました"
    fi
    for name in "${REQUIRED[@]}"; do
        grep -Eq "^test ${name} \.\.\. ok$" <<<"$out" \
            || fail "必須テスト ${name} が成功していません（存在しない可能性があります）"
    done
    echo "白票の表示ロジック: 必須 ${#REQUIRED[@]} 件 OK（allow_blank=false では選択肢に出さない・確認画面は labels.blank_confirm）"

    echo "== 4-2. 画面が、API の allow_blank と設定の文言で、白票の選択肢・確認の文言・ビューアの行を組み立てている"
    VOTE=crates/web/src/pages/vote.rs
    grep -q 'flow::choices(' "$VOTE" && grep -q 'response.allow_blank' "$VOTE" && grep -q 'labels::blank_option()' "$VOTE" \
        || fail "$VOTE が、flow::choices に API の allow_blank と labels::blank_option() を渡していません"
    grep -q 'flow::confirm_message(' "$VOTE" && grep -q 'labels::blank_confirm()' "$VOTE" \
        || fail "$VOTE の確認画面が、flow::confirm_message と labels::blank_confirm() を使っていません"
    if grep -n '"blank"' "$VOTE"; then
        fail "$VOTE に白票の予約値が直接書かれています（shared_types::BLANK_CANDIDATE_ID を使う flow に任せる）"
    fi
    grep -q 'chain::choice_counts(' crates/web/src/pages/chain.rs && grep -q 'labels::blank_name()' crates/web/src/pages/chain.rs \
        || fail "ビューアが、白票を別の行にする chain::choice_counts と labels::blank_name() を使っていません"
    grep -qF '.candidate.blank' crates/web/style.css && grep -qF 'table.ballots tr.blank' crates/web/style.css \
        || fail "style.css に、白票の選択肢・ビューアの行の書式（.candidate.blank / table.ballots tr.blank）がありません"
    echo "投票画面（選択肢・確認）・ビューア（別の行）・書式: OK"

    echo "OK: web#4 白票"
)

# ===========================================================================
# 5. 投票のやり直し（画面。API・封印・集計は core.sh#11・chain.sh#9）
# ===========================================================================
check_revote_screens() (
    set -euo pipefail
    fail() {
        echo "FAIL: $1" >&2
        exit 1
    }
    echo "== 5-1. やり直しの画面遷移・表示ロジックのテスト（必須ケース）"
    REQUIRED=(
        flow::tests::the_revote_button_needs_all_ballots_voted_an_open_period_and_revotes_allowed
        flow::tests::the_revote_list_keeps_the_fixed_order_and_blocks_ballots_at_the_limit
        flow::tests::revote_pages_open_only_for_voted_ballots_below_the_limit_while_open
        flow::tests::a_revote_sends_the_seen_count_and_bumps_it_after_acceptance
        flow::tests::revote_failures_return_to_the_revote_list_with_a_notice
        error::tests::conflicts_are_told_apart_by_the_error_code
        chain::tests::a_revote_links_to_the_ballot_it_replaced_with_a_to_b
    )
    if ! out="$(cargo test -p web --lib 2>&1)"; then
        echo "$out" >&2
        fail "web のテストが失敗しました"
    fi
    for name in "${REQUIRED[@]}"; do
        grep -Eq "^test ${name} \.\.\. ok$" <<<"$out" \
            || fail "必須テスト ${name} が成功していません（存在しない可能性があります）"
    done
    echo "やり直しの画面遷移・表示ロジック: 必須 ${#REQUIRED[@]} 件 OK"

    echo "== 5-2. 画面が、設定の文言と flow の判定で組み立てられ、前回の投票内容を出さない"
    grep -q 'flow::can_revote(' crates/web/src/pages/done.rs && grep -q 'labels::revote_button()' crates/web/src/pages/done.rs \
        || fail "完了画面が、flow::can_revote と labels::revote_button() で「投票をやり直す」を出していません"
    REVOTE=crates/web/src/pages/revote.rs
    grep -q 'flow::revote_items(' "$REVOTE" && grep -q 'labels::revote_limit_reached()' "$REVOTE" \
        || fail "$REVOTE が、flow::revote_items と labels::revote_limit_reached() を使っていません"
    grep -q 'labels::revote_confirm()' crates/web/src/pages/vote.rs \
        || fail "やり直しの確認画面が labels::revote_confirm() を出していません"
    # 前回の投票内容は、API も返さない（BallotStatusDto に候補者の項目が無い）。画面が候補者を覚えておく経路も無いこと。
    if grep -nE 'candidate' "$REVOTE"; then
        fail "$REVOTE に候補者の情報が出てきます（前回の投票内容は表示しない）"
    fi
    grep -q 'chain::ballot_anchor(' crates/web/src/pages/chain.rs && grep -qF 'a.replaces' crates/web/style.css \
        || fail "ビューアに、前の票へのリンク（行のアンカー・a.replaces の書式）がありません"
    echo "完了画面のボタン・一覧（上限の理由）・確認の文言・ビューアのリンク・前回の投票内容を出さない: OK"

    echo "OK: web#5 投票のやり直し"
)

check_flow_and_purity
check_design_tokens_and_theme
check_trunk_build
check_blank_choice
check_revote_screens

echo "OK: check/web.sh"
