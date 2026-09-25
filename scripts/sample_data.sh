#!/usr/bin/env bash
# 開発環境の DB に、確認用のサンプルデータ（ユーザー単位のパターン P01〜P13）を一括で登録する（app.mode=db 用。ADR 0023）。
#
#   scripts/sample_data.sh [--phase before|open|closed] [--yes]
#
#   --phase before  投票の開始前（開始は今から sample.open_hours 時間後）。事前の投票が必要なパターン（P05〜P09）は作れない
#   --phase open    投票期間中（既定。期間は「今から sample.open_hours 時間」）
#   --phase closed  投票の終了後（open で事前の投票をしてから、scripts/election.sh close --now で締め切る）
#   --yes           DB のリセット（scripts/db_reset.sh --all）の確認を省略する
#
# 手順（既存のスクリプト・CLI を組み合わせる。新しい仕組みは作らない）:
#   1. scripts/db_reset.sh --all（キースペースを削除して、docs/schema.cql からスキーマを作り直す）
#   2. seedgen で小規模の選挙データを作り、パターンの有権者だけを名簿（voters.csv）に残す
#   3. api と sealer を起動し、scripts/election.sh schedule で選挙期間を設定する
#   4. credgen で有権者を登録する（P13 は、その有権者だけを credgen --reissue で再発行する）
#   5. 事前に必要な投票を、API（/api/v1/login・/api/v1/contests/…/vote）で行う
#   6. 封印を待つ（open は封印のルールで。closed は scripts/election.sh close --now の締切の手続きで）
#   7. <sample.output_dir>/credentials_patterns.csv（権限 0600。git 管理外）を出力し、要約表を表示する。api と sealer は止める
#
# 出力した CSV は、そのままテスト仕様になる（期待結果は、実際の選挙状態・選挙のルール（allow_revote / allow_blank /
# max_revotes）・期間から計算する）。scripts/check/auth.sh#2 が、各行を実際に試して一致を確認する。
#
# 決まり:
#   - app.env=production では、何もせずに終了する。app.mode=memory では、db モードで実行するよう案内して終了する。
#   - 認証は db（事前登録の ID とパスワード）。環境変数 APP__AUTH__MODE を db にして api を起動する。
#   - 秘密情報は、scripts/dev_up.sh と同じ開発用の固定値（指定が無いときだけ）。署名鍵が同じなので、この後
#     scripts/dev_up.sh cassandra でそのまま画面から確認できる（選挙データの場所を渡すこと。最後に表示する）。
# 設定は、手元の実際の設定（config/*.toml・secrets/・環境変数 APP__…）を使う（scripts/db_reset.sh と同じ）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

USAGE="使い方: scripts/sample_data.sh [--phase before|open|closed] [--yes]"
PHASE=open
YES_ARG=()
while (($# > 0)); do
    case "$1" in
        --phase=*) PHASE="${1#--phase=}" ;;
        --phase)
            [[ $# -ge 2 ]] || {
                echo "$USAGE" >&2
                exit 2
            }
            PHASE="$2"
            shift
            ;;
        --yes) YES_ARG=(--yes) ;;
        -h | --help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "不明な引数: $1（${USAGE}）" >&2
            exit 2
            ;;
    esac
    shift
done
case "$PHASE" in
    before | open | closed) ;;
    *)
        echo "--phase は before / open / closed のどれかです（指定: ${PHASE}）。${USAGE}" >&2
        exit 2
        ;;
esac

LOG_DIR=""
fail() {
    echo "エラー: $1" >&2
    for f in "${LOG_DIR:-/nonexistent}"/api.log "${LOG_DIR:-/nonexistent}"/sealer.log; do
        if [[ -s "$f" ]]; then
            echo "--- ${f}（末尾）---" >&2
            tail -n 12 "$f" >&2
        fi
    done
    exit 1
}

# 実際の設定を使う（cfg_init の分離をしない）。
export CFG_USE_REAL=1
source "$SCRIPT_DIR/lib/common.sh"
cfg_build
APP_ENV="$(cfg_get app.env)" || fail "設定を読み込めません（上のメッセージを参照）"
APP_MODE="$(cfg_get app.mode)"

# --- 0. 前提の確認 ---
if [[ "$APP_ENV" == "production" ]]; then
    fail "app.env=production では、サンプルデータを登録できません（何も変更していません）。"
fi
if [[ "$APP_MODE" == "memory" ]]; then
    echo "app.mode=memory: サンプルデータは DB に登録します。db モードで実行してください:" >&2
    echo "  APP__APP__MODE=db scripts/sample_data.sh --phase ${PHASE}${YES_ARG[*]:+ ${YES_ARG[*]}}" >&2
    echo "（memory モードの api は、再起動で票が消え、認証も stub（ID をそのまま採用）なので、パターンを作れません。何も変更していません）" >&2
    exit 1
fi
export APP__AUTH__MODE=db
dev_default_secrets
"$CFG_BIN" validate >/dev/null || fail "設定が不正です（上のメッセージを参照。実効値は: cargo run -p app-config -- show）"

OUT_DIR="$(cfg_get sample.output_dir)"
OPEN_HOURS="$(cfg_get sample.open_hours)"
EID="$(cfg_get election.election_id)"
PUBLIC_PORT="$(cfg_get api.port)"
ADMIN_BIND="$(cfg_get admin.bind)"
TZ_NAME="$(cfg_get election.display_timezone)"
BLANK_NAME="$(cfg_get labels.blank_name)"
SEED_DIR="$OUT_DIR/seed"
CSV_OUT="$OUT_DIR/credentials_patterns.csv"
LOG_DIR="$OUT_DIR/logs"
CREDGEN_CSV="$OUT_DIR/.credgen.csv"
REISSUE_CSV="$OUT_DIR/.credgen-reissue.csv"
BASE="http://127.0.0.1:${PUBLIC_PORT}"
export BASE

port_in_use() { (: </dev/tcp/127.0.0.1/"$1") 2>/dev/null; }
for p in "$PUBLIC_PORT" "${ADMIN_BIND##*:}"; do
    ! port_in_use "$p" || fail "ポート ${p} はすでに使われています（scripts/dev_up.sh で起動中なら、先に scripts/dev_down.sh で止めてください）"
done

# 期間（RFC 3339。表示のタイムゾーンのオフセットつき。tzdata が無ければ UTC のオフセットになるが、同じ時刻を表す）。
rfc3339() { TZ="$TZ_NAME" date -d "@$1" +%Y-%m-%dT%H:%M:%S%:z; }
NOW="$(date +%s)"
if [[ "$PHASE" == before ]]; then
    OPENS=$((NOW + OPEN_HOURS * 3600))
else
    OPENS="$NOW"
fi
CLOSES=$((OPENS + OPEN_HOURS * 3600))
OPENS_AT="$(rfc3339 "$OPENS")"
CLOSES_AT="$(rfc3339 "$CLOSES")"
# api / sealer には、この選挙データと締切を渡す。開始時刻は空にして、schedule より前に自動で open にならないようにする
# （設定に過去の開始時刻が書かれていると、起動した直後に open になり、schedule できなくなるため）。
export APP__ELECTION__SEED_DIR="$SEED_DIR"
export APP__ELECTION__VOTING_OPENS_AT=""
export APP__ELECTION__VOTING_CLOSES_AT="$CLOSES_AT"

cleanup() {
    local rc=$?
    common_cleanup
    if [[ "$rc" -ne 0 ]]; then
        hard_stop api
        hard_stop sealer
    fi
    rm -f "$CREDGEN_CSV" "$REISSUE_CSV"
    rm -rf "$OUT_DIR/seed-reissue"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
echo "== 1. DB のリセットとスキーマの作成（scripts/db_reset.sh --all）"
./scripts/db_reset.sh --all "${YES_ARG[@]}" || fail "DB のリセットに失敗しました（上のメッセージを参照）"

# ---------------------------------------------------------------------------
echo "== 2. 小規模の選挙データ（seedgen → ${SEED_DIR}）"
cargo build -q -p api -p sealer -p seedgen -p credgen || fail "ビルドに失敗しました"
mkdir -p "$OUT_DIR" "$LOG_DIR"
# 32 都道府県: 参議院の合区（鳥取・島根 = 31_32）が現れる最小の数。ほかは各 1 選挙区・候補者 2 人（A→B の再投票に使う）。
# 有権者 voter-K は、都道府県 K に属する（voter-31 は合区）。
./target/debug/seedgen --out "$SEED_DIR" --election-id "$EID" --prefectures 32 --districts-per-pref 1 \
    --candidates-per-district 2 --voters 31 --municipalities-per-pref 1 --pref-assembly-districts-per-pref 1 \
    --force >/dev/null || fail "seedgen が失敗しました"
VOTERS_CSV="$SEED_DIR/$EID/voters.csv"
districts_of() { sed -n "s/^$1,//p" "$VOTERS_CSV"; }

# パターン: ID|名前|名簿の選挙区（seed: 生成した有権者の行 / 値そのもの）|事前の投票（none / first / all / blank / revote / limit）
PATTERNS=(
    "P01|通常（未投票、投票用紙が複数）|seed:voter-1|none"
    "P02|投票用紙が1枚だけ|supreme_court_review.national|none"
    "P03|合区の選挙区に属する|seed:voter-31|none"
    "P04|投票できる投票用紙がない||none"
    "P05|一部の投票用紙だけ投票済み|seed:voter-2|first"
    "P06|すべて投票済み|seed:voter-3|all"
    "P07|白票で投票済み|seed:voter-4|blank"
    "P08|再投票済み（A→B）|seed:voter-5|revote"
    "P09|再投票の上限に到達|seed:voter-6|limit"
    "P10|パスワード誤り|seed:voter-7|none"
    "P11|存在しないID|-|none"
    "P12|IDの形式が不正|-|none"
    "P13|再発行済み|seed:voter-8|none"
)

# 作れないパターンの理由と、備考（空なら作れる）。選挙のルールは、まだ固定されていないので設定の値で判断する
# （open に移るのはこの後で、固定されるのは、この実行が起動する sealer の設定の値 = 同じ値）。
ALLOW_BLANK="$(cfg_get vote.allow_blank)"
ALLOW_REVOTE="$(cfg_get vote.allow_revote)"
unavailable_reason() {
    local prep="$1"
    [[ "$prep" == none ]] && return 0
    if [[ "$PHASE" == before ]]; then
        echo "開始前のため|事前の投票が必要なパターン（開始前は投票できない）"
    elif [[ "$prep" == blank && "$ALLOW_BLANK" != true ]]; then
        echo "白票が無効のため|vote.allow_blank=false"
    elif [[ ("$prep" == revote || "$prep" == limit) && "$ALLOW_REVOTE" != true ]]; then
        echo "再投票が無効のため|vote.allow_revote=false"
    fi
}

# 名簿は、作れるパターンの有権者だけにする（ID は pattern_id の小文字。credgen は ID の順に CSV を書くので、この順に並ぶ）。
declare -A NAME PREP REASON ROLL
REGISTERED=()
{
    echo "voter_id,districts"
    for row in "${PATTERNS[@]}"; do
        IFS='|' read -r pid name roll prep <<<"$row"
        NAME[$pid]="$name"
        PREP[$pid]="$prep"
        REASON[$pid]="$(unavailable_reason "$prep")"
        [[ "$roll" == - || -n "${REASON[$pid]}" ]] && continue
        [[ "$roll" == seed:* ]] && roll="$(districts_of "${roll#seed:}")"
        ROLL[$pid]="$roll"
        REGISTERED+=("$pid")
        echo "${pid,,},${roll}"
    done
} >"$VOTERS_CSV.new"
mv -f "$VOTERS_CSV.new" "$VOTERS_CSV"
./target/debug/seedgen --check "$SEED_DIR" --election-id "$EID" >/dev/null || fail "選挙データの検証に失敗しました"
echo "  名簿: ${#REGISTERED[@]} 人（${REGISTERED[*]}）"

# ---------------------------------------------------------------------------
echo "== 3. api・sealer の起動と、選挙期間の設定（scripts/election.sh schedule）"
ensure_revote_key
spawn sealer "$LOG_DIR/sealer.log" env APP__SEALER__ID=sample-sealer ./target/debug/sealer
spawn api "$LOG_DIR/api.log" ./target/debug/api
wait_healthz "$BASE" 60 api || fail "api が起動しません"
chain_ready() { [[ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/v1/chains/0/head")" == 200 ]]; }
wait_until 90000 chain_ready || fail "sealer がチェーンを用意しません"
./scripts/election.sh schedule --opens-at "$OPENS_AT" --closes-at "$CLOSES_AT" >/dev/null \
    || fail "選挙期間の設定に失敗しました"
echo "  期間: ${OPENS_AT} 〜 ${CLOSES_AT}（sample.open_hours=${OPEN_HOURS}）"
if [[ "$PHASE" != before ]]; then
    wait_election_open "$BASE" 60 || fail "開始時刻を過ぎても、sealer が open にしません"
    echo "  選挙状態: open"
fi

# ---------------------------------------------------------------------------
echo "== 4. 有権者の登録（credgen）"
rm -f "$CREDGEN_CSV" "$REISSUE_CSV"
APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true APP__CREDENTIALS__OUTPUT_PATH="$CREDGEN_CSV" \
    ./target/debug/credgen | sed 's/^/  /' || fail "credgen が失敗しました"
declare -A LOGIN PASS PREF DIST
i=1
for pid in "${REGISTERED[@]}"; do
    i=$((i + 1))
    IFS=, read -r LOGIN[$pid] PASS[$pid] PREF[$pid] DIST[$pid] < <(sed -n "${i}p" "$CREDGEN_CSV")
done
# P13: その有権者だけの名簿（選挙の定義は同じもの）で、credgen --reissue を実行する（古い ID とパスワードは使えなくなる）。
REISSUE_SEED="$OUT_DIR/seed-reissue"
mkdir -p "$REISSUE_SEED/$EID"
for f in election.toml districts.csv candidates; do
    ln -sfn "$(cd "$SEED_DIR/$EID" && pwd)/$f" "$REISSUE_SEED/$EID/$f"
done
printf 'voter_id,districts\np13,%s\n' "${ROLL[P13]}" >"$REISSUE_SEED/$EID/voters.csv"
APP__ELECTION__SEED_DIR="$REISSUE_SEED" APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true \
    APP__CREDENTIALS__OUTPUT_PATH="$REISSUE_CSV" ./target/debug/credgen --reissue | sed 's/^/  (P13) /' \
    || fail "credgen --reissue が失敗しました"
IFS=, read -r NEW_LOGIN NEW_PASS _ _ < <(sed -n 2p "$REISSUE_CSV")

# ---------------------------------------------------------------------------
echo "== 5. 事前の投票（API）"
# vote_ok TOKEN K CANDIDATE [REVOTE]: 表示順で K 番目の投票用紙に投票し、201 でなければ失敗にする。
# CANDIDATE は、候補者の連番（1 / 2）か blank。
vote_ok() {
    local contest candidate="$3"
    contest="$(ballot_field "$2" contest_id)"
    [[ "$candidate" == blank ]] || candidate="${contest#*/}.c${candidate}"
    cast_vote "$1" "$contest" "$candidate" "${4:-}"
    expect_status 201 "事前の投票（${2} 枚目）"
}
PRE_VOTES=0
for pid in "${REGISTERED[@]}"; do
    prep="${PREP[$pid]}"
    [[ "$prep" == none ]] && continue
    db_login "${LOGIN[$pid]}" "${PASS[$pid]}"
    expect_status 200 "${pid} のログイン"
    token="$(token_of)"
    ballots_of "$token"
    case "$prep" in
        first) vote_ok "$token" 1 1 ;;
        all) for k in $(seq 1 "$(ballot_count)"); do vote_ok "$token" "$k" 1; done ;;
        blank) vote_ok "$token" 1 blank ;;
        revote)
            vote_ok "$token" 1 1
            vote_ok "$token" 1 2 1
            ;;
        limit)
            vote_ok "$token" 1 1
            max="$(cfg_get vote.max_revotes)"
            for n in $(seq 1 "$max"); do vote_ok "$token" 1 $((n % 2 + 1)) "$n"; done
            ;;
    esac
    ballots_of "$token"
    for k in $(seq 1 "$(ballot_count)"); do PRE_VOTES=$((PRE_VOTES + $(ballot_field "$k" ballots_cast))); done
    echo "  ${pid}（${NAME[$pid]}）: 済み"
done
echo "  受理した票: ${PRE_VOTES} 件"

# ---------------------------------------------------------------------------
echo "== 6. 封印"
pending() { ./scripts/election.sh status 2>/dev/null | sed -n 's/^シャードごとの未封印: //p' | tr ',' '+' | bc_sum; }
phase_now() { ./scripts/election.sh status 2>/dev/null | sed -n 's/^状態: //p'; }
all_sealed() { [[ "$(pending)" == 0 ]]; }
is_closed() { [[ "$(phase_now)" == closed ]]; }
if [[ "$PHASE" == closed ]]; then
    # 締切の手続き（全シャードのフラッシュ・再投票の鍵の破棄・closed への遷移）は、sealer が自動で行う。
    ./scripts/election.sh close --now --yes >/dev/null || fail "close --now に失敗しました"
    wait_secs=$(($(cfg_get election.state_cache_secs) + $(cfg_get api.request_timeout_secs) + 60))
    wait_until $((wait_secs * 1000)) is_closed || fail "締切の手続きが ${wait_secs} 秒以内に終わりません（選挙状態: $(phase_now)）"
    echo "  close --now → 締切の手続きで全件を封印 → 選挙状態: closed"
elif ((PRE_VOTES == 0)); then
    echo "  票がないので、封印するものはありません"
else
    # 投票期間中は、封印のルール（原則9）に従って sealer が封印する。件数が最小件数に満たないシャードは、締切の手続きまで残る。
    wait_secs=$(($(cfg_get seal.interval_secs) + 20))
    if wait_until $((wait_secs * 1000)) all_sealed; then
        echo "  封印のルールで、全件を封印しました"
    else
        echo "  注意: 未封印が $(pending) 件残っています（封印のルールの最小件数 seal.min_ballots_after_interval に満たないため。締切の手続きで封印されます）"
    fi
fi

# ---------------------------------------------------------------------------
echo "== 7. CSV の出力（${CSV_OUT}）"
# 期待結果は、実際の選挙状態・固定した選挙のルール・期間から計算する（GET /api/v1/election-status。認証不要）。
request GET /api/v1/election-status
expect_status 200 "GET /api/v1/election-status"
STATUS_JSON="$BODY"
json_value() { sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\).*/\1/p" <<<"$STATUS_JSON"; }
PHASE_NOW="$(json_value phase)"
R_ALLOW_BLANK="$(json_value allow_blank)"
R_ALLOW_REVOTE="$(json_value allow_revote)"
R_MAX_REVOTES="$(json_value max_revotes)"
S_OPENS="$(json_value opens_at)"
S_CLOSES="$(json_value closes_at)"
S_NOW="$(json_value now)"
# 投票を受け付けるか（domain::vote_gate と同じ判定。原則18）。受け付けないなら、その理由のエラー。
if [[ "$PHASE_NOW" == scheduled ]]; then
    GATE=voting_not_started
elif [[ "$PHASE_NOW" == closing ]]; then
    GATE=voting_closing
elif [[ "$PHASE_NOW" == closed ]]; then
    GATE=voting_closed
elif [[ "$S_OPENS" != null && "$S_NOW" -lt "$S_OPENS" ]]; then
    GATE=voting_not_started
elif [[ "$S_CLOSES" != null && "$S_NOW" -ge "$S_CLOSES" ]]; then
    GATE=voting_closed
else
    GATE=""
fi
OTHER_CONTEST="${EID}/$(sed -n 2p "$SEED_DIR/$EID/districts.csv" | cut -d, -f1)"

ok() { echo "成功（$1）"; }
ng() { echo "拒否（$1 $2）"; }
# expect_vote PID: 「表示順で先頭の未投票の投票用紙（無ければ 1 枚目）に、候補者 1（P07 は白票）で投票する」の期待結果。
# API の判定の順（受付期間 → 投票対象か → 投票先 → 投票済みか）に合わせる。
expect_vote() {
    local pid="$1" n k=0 target cand="候補者1" result
    n="$(ballot_count)"
    [[ "${PREP[$pid]}" == blank ]] && cand="$BLANK_NAME"
    for i in $(seq 1 "$n"); do
        [[ "$(ballot_field "$i" voted)" == false ]] && k="$i" && break
    done
    if ((n == 0)); then
        target="対象外の投票用紙（${OTHER_CONTEST}）"
    else
        target="$((k > 0 ? k : 1))枚目"
    fi
    if [[ -n "$GATE" ]]; then
        result="$(ng 403 "$GATE")"
    elif ((n == 0)); then
        result="$(ng 403 not_eligible)"
    elif [[ "$cand" == "$BLANK_NAME" && "$R_ALLOW_BLANK" != true ]]; then
        result="$(ng 422 blank_not_allowed)"
    elif ((k == 0)); then
        result="$(ng 409 already_voted)"
    else
        result="$(ok 201)"
    fi
    echo "${target}に${cand}で投票 → ${result}"
}
# expect_revote PID: 「表示順で先頭の投票済みの投票用紙（無ければ 1 枚目）を、候補者 2 でやり直す（revote = ballots_cast）」の期待結果。
# API の判定の順（受付期間 → 再投票の可否 → 投票対象か → 投票済みか → 上限）に合わせる。
expect_revote() {
    local n k=0 target result cast=0
    n="$(ballot_count)"
    for i in $(seq 1 "$n"); do
        [[ "$(ballot_field "$i" voted)" == true ]] && k="$i" && break
    done
    ((k > 0)) && cast="$(ballot_field "$k" ballots_cast)"
    if ((n == 0)); then
        target="対象外の投票用紙（${OTHER_CONTEST}）"
    else
        target="$((k > 0 ? k : 1))枚目"
    fi
    if [[ -n "$GATE" ]]; then
        result="$(ng 403 "$GATE")"
    elif [[ "$R_ALLOW_REVOTE" != true ]]; then
        result="$(ng 409 revote_not_allowed)"
    elif ((n == 0)); then
        result="$(ng 403 not_eligible)"
    elif ((k == 0)); then
        result="$(ng 409 not_voted)"
    elif ((cast >= R_MAX_REVOTES + 1)); then
        result="$(ng 409 revote_limit_reached)"
    else
        result="$(ok 201)"
    fi
    echo "${target}を候補者2でやり直し → ${result}"
}
note_of() {
    case "$1" in
        P01) echo "すべて未投票" ;;
        P02) echo "投票用紙は1枚（全国の選挙区）だけ" ;;
        P03) echo "参議院選挙区が合区（鳥取県・島根県）" ;;
        P04) echo "名簿の選挙区が空（ログインはできるが、投票用紙は0枚）" ;;
        P05) echo "1枚目だけ投票済み" ;;
        P06) echo "すべての投票用紙に投票済み" ;;
        P07) echo "1枚目に${BLANK_NAME}で投票済み" ;;
        P08) echo "1枚目を1回やり直し済み（受理した票は2件）" ;;
        P09) echo "1枚目を上限の$(cfg_get vote.max_revotes)回やり直し済み" ;;
        P10) echo "ログインIDは正しく、パスワードだけが誤り" ;;
        P11) echo "形式は正しいが、登録されていないログインID" ;;
        P12) echo "空白と記号を含む（形式が不正な）ログインID" ;;
    esac
}
LOGIN_OK="$(ok 200)"
LOGIN_NG="$(ng 401 unauthorized)"
NOT_TRIED="—（ログインできないため試さない）"
wrong_of() { if [[ "${1: -1}" == A ]]; then echo "${1%?}B"; else echo "${1%?}A"; fi; }
ID_LEN="$(cfg_get credentials.login_id_length)"
PW_LEN="$(cfg_get credentials.password_length)"
UNKNOWN_ID="$(printf 'Z%.0s' $(seq 1 "$ID_LEN"))"
DUMMY_PW="$(printf 'Z%.0s' $(seq 1 "$PW_LEN"))"

# row PID NAME LOGIN PASSWORD PREF DIST COUNT LOGIN_EXP VOTE_EXP REVOTE_EXP NOTE（どの値にも「,」を含めない）
ROWS=()
row() {
    local IFS=,
    ROWS+=("$*")
}
for entry in "${PATTERNS[@]}"; do
    IFS='|' read -r pid _ _ _ <<<"$entry"
    name="${NAME[$pid]}"
    if [[ -n "${REASON[$pid]}" ]]; then
        cell="作成不可（${REASON[$pid]%%|*}）"
        row "$pid" "$name" "" "" "" "" "" "$cell" "$cell" "$cell" "${REASON[$pid]#*|}"
        continue
    fi
    count=""
    [[ -n "${ROLL[$pid]+x}" ]] && count="$(awk -F';' '{print ($0 == "" ? 0 : NF)}' <<<"${ROLL[$pid]}")"
    case "$pid" in
        P10)
            row "$pid" "$name" "${LOGIN[$pid]}" "$(wrong_of "${PASS[$pid]}")" "${PREF[$pid]}" "${DIST[$pid]}" "$count" \
                "$LOGIN_NG" "$NOT_TRIED" "$NOT_TRIED" "$(note_of "$pid")"
            ;;
        P11)
            row "$pid" "$name" "$UNKNOWN_ID" "$DUMMY_PW" "" "" "" "$LOGIN_NG" "$NOT_TRIED" "$NOT_TRIED" "$(note_of "$pid")"
            ;;
        P12)
            row "$pid" "$name" "bad id!" "$DUMMY_PW" "" "" "" "$LOGIN_NG" "$NOT_TRIED" "$NOT_TRIED" "$(note_of "$pid")"
            ;;
        P13)
            row "$pid" "${name}（古いパスワード）" "${LOGIN[$pid]}" "${PASS[$pid]}" "${PREF[$pid]}" "${DIST[$pid]}" "$count" \
                "$LOGIN_NG" "$NOT_TRIED" "$NOT_TRIED" "再発行前のIDとパスワード（再発行で無効）"
            db_login "$NEW_LOGIN" "$NEW_PASS"
            expect_status 200 "P13（再発行後）のログイン"
            ballots_of "$(token_of)"
            row "$pid" "${name}（新しいパスワード）" "$NEW_LOGIN" "$NEW_PASS" "${PREF[$pid]}" "${DIST[$pid]}" "$count" \
                "$LOGIN_OK" "$(expect_vote "$pid")" "$(expect_revote "$pid")" "再発行後のIDとパスワード（投票の記録は再発行の前と同じ有権者のもの）"
            ;;
        *)
            db_login "${LOGIN[$pid]}" "${PASS[$pid]}"
            expect_status 200 "${pid} のログイン"
            ballots_of "$(token_of)"
            row "$pid" "$name" "${LOGIN[$pid]}" "${PASS[$pid]}" "${PREF[$pid]}" "${DIST[$pid]}" "$count" \
                "$LOGIN_OK" "$(expect_vote "$pid")" "$(expect_revote "$pid")" "$(note_of "$pid")"
            ;;
    esac
done

# 平文のパスワードを含むので、作る時点から 0600（すでにあれば置き換える。古いパスワードは db_reset で使えなくなっている）。
rm -f "$CSV_OUT"
(
    umask 077
    echo "pattern_id,pattern_name,login_id,password,都道府県,選挙区,投票用紙の数,期待結果_ログイン,期待結果_投票,期待結果_再投票,備考"
    printf '%s\n' "${ROWS[@]}"
) >"$CSV_OUT"
chmod 600 "$CSV_OUT"

# ---------------------------------------------------------------------------
echo "== 8. api・sealer の停止"
graceful_stop api
graceful_stop sealer
echo "  停止しました（DB のデータは残っています）"

# 期待結果が有効な期限: 開始前は開始時刻まで、投票期間中は終了時刻まで。
case "$PHASE_NOW" in
    scheduled) VALID_UNTIL="開始時刻 ${OPENS_AT} まで" ;;
    open) VALID_UNTIL="終了時刻 ${CLOSES_AT} まで" ;;
    *) VALID_UNTIL="期限なし（締切済み）" ;;
esac
short() { sed 's/.*→ //' <<<"$1"; }
echo
echo "============================================================"
echo " サンプルデータ（--phase ${PHASE}。選挙状態: ${PHASE_NOW}。ルール: allow_blank=${R_ALLOW_BLANK} allow_revote=${R_ALLOW_REVOTE} max_revotes=${R_MAX_REVOTES}）"
echo "============================================================"
{
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "ID" "パターン" "login_id" "用紙" "ログイン" "投票" "再投票"
    for line in "${ROWS[@]}"; do
        IFS=, read -r pid name login _ _ _ count e_login e_vote e_revote _ <<<"$line"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$pid" "$name" "${login:--}" "${count:--}" \
            "$(short "$e_login")" "$(short "$e_vote")" "$(short "$e_revote")"
    done
} | if command -v column >/dev/null 2>&1; then column -t -s $'\t'; else tr '\t' ' '; fi
cat <<MSG

■ ID・パスワードと期待結果（平文のパスワードを含む。権限 0600・git 管理外）: ${CSV_OUT}
   期待結果は、この実行の直後の状態に対するもの（${VALID_UNTIL}）。確認で投票すると状態が変わるので、やり直すときは、もう一度このスクリプトを実行する。
   確認の手順（scripts/check/auth.sh#2 と同じ）: ログイン → 再投票 → 投票 の順に試す（再投票の確認が、投票の確認の結果に左右されないように）。
■ 画面で確認する（選挙データの場所を渡して起動する。認証は db、署名鍵は同じ開発用の固定値）:
   APP__ELECTION__SEED_DIR=${SEED_DIR} scripts/dev_up.sh cassandra
■ ログ: ${LOG_DIR}/{api,sealer}.log
MSG
