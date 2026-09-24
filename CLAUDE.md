# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`tohyo-chain` is a Web投票システムのプロトタイプ（Cargo ワークスペース、Rust edition 2024）。水平スケール可能な API と、
ハッシュチェーン（簡易ブロックチェーン）による改ざん検知を検証する。クレート構成・起動方法は [README.md](README.md)、
守るべき原則は本ファイルの「投票システム プロトタイプ」節、設計判断は `docs/adr/` を参照。

## Commands

- Build: `cargo build`
- Run: `cargo run`
- Test all: `cargo test`
- Run a single test: `cargo test <test_name>` (add `-- --nocapture` to see stdout)
- Lint: `cargo clippy`
- Format: `cargo fmt`
- Web (Trunk): **`trunk` コマンドは必ず `crates/web` で実行する**（`cd crates/web && trunk serve` / `cd crates/web && trunk build --release`）。ワークスペースのルートで実行すると `could not find the root package of the target crate` で失敗する
- 設定の確認: `cargo run -p app-config -- show`（実効値と出所。秘密情報は ***）/ `get KEY` / `validate`。設定は原則 11（config/、環境変数は `APP__<セクション>__<項目>`）
- 手元で動かす: `scripts/dev_up.sh [memory|cassandra] [--auth stub|db]` / `scripts/dev_down.sh`（画面 http://localhost:8080、api 18080）。認証の既定は、memory が stub、cassandra が db（起動時に credgen が名簿の有権者を登録し、ID とパスワードを secrets/credentials.csv に出力する）
- 選挙状態（scheduled → open → closing → closed。原則17・18）: `scripts/election.sh status | schedule --opens-at <RFC3339> --closes-at <RFC3339> | open --now | close --now`（管理用リスナー admin.bind、既定 127.0.0.1:18081。トークンは secrets/admin_token か環境変数 APP__ADMIN__TOKEN）
- Step 完了確認: `scripts/check_all.sh`（fmt / clippy / test と `scripts/check/*.sh`（core / chain / election / auth / web / docs の 6 スイート）をすべて実行。ルートから実行する。対応表は docs/testing.md）

## Tooling notes

- Edition 2024 requires Rust 1.85 or newer.
- `.vscode/launch.json` has CodeLLDB (`lldb`) debug configs for the `api` binary and its unit tests; it needs the CodeLLDB extension.
- `target/` is git-ignored.
- Trunk はカレントディレクトリの `Cargo.toml` からルートパッケージを探す。ワークスペースのルートは仮想マニフェスト（パッケージなし）なので、`index.html` と `Trunk.toml`（proxy 設定を含む）は `crates/web/` 直下に置き、`trunk` も `crates/web` で実行する。成果物は `crates/web/dist/`（git 管理外）。
- web のビルドには `rustup target add wasm32-unknown-unknown` と `cargo install trunk --locked`（0.21 系）が必要。

## Working rules

- 回答と説明は日本語で行う
- コードを変更したら、必ず `cargo fmt` → `cargo clippy` → `cargo test` の順に実行し、警告やエラーがゼロになったことを確認してから報告する
- 修正の際は「なぜその修正が必要か」を、所有権・借用・ライフタイムの観点で説明する
- 私が「説明して」とだけ頼んだときは、コードを変更せずに説明のみ行う
- `unwrap()` / `expect()` の多用と `unsafe` は避け、使う場合は理由を明記する
- 私は Rust を学習中なので、Rust らしい書き方（イディオム）があれば提案する

# 投票システム プロトタイプ

## 目的
Web投票システムのプロトタイプ。水平スケール可能なAPIと、
ハッシュチェーン（簡易ブロックチェーン）による改ざん検知を検証する。

## 絶対に守る原則
1. 秘密投票: voter_id と candidate_id を、同じレコード・ログ・メトリクス・エラーに含めない。
   再投票のために使う仮名 slot = HMAC(revote_key, election_id||voter_id||contest_id) だけは、
   票と一緒に記録してよい。revote_key は secrets/ で管理し、締切の手続きの中で破棄する。
   前回の投票内容は画面にも API にも返さない。
   実装（ADR 0022）: vote.allow_revote（既定 false）/ vote.max_revotes（既定 5）。true のときだけ、票に slot・seq（その slot の
   何番目か）・supersedes（1 つ前の票のハッシュ = domain::ballot_hash）を付け、シャードは hash(slot)（domain::shard_for_slot）で決める。
   false のときは slot を記録しない（2 回目の投票は 409）。slot の HMAC は各 ID に 2 バイトの長さ接頭辞を付ける（application::RevoteKey）。
   revote_key は secrets/revote_key（64 桁の hex）にだけ置く（環境変数では渡せない。ログ・DB に出さない。無ければ api は起動しない）。
   participation は seq（受理した票の数）を持ち、再投票は LWT の比較更新（UPDATE … SET seq = n+1 IF seq = n。n は画面が見た
   ballots_cast で、API の revote に入れる）。slot_state(slot → seq, last_ballot_hash) はキーが slot（voter_id ではない）。
   締切の手続きの中で、sealer（memory モードは api 内蔵のスケジューラ）が鍵をファイルごと破棄し、election_audit に
   revote_key_destroyed を記録する（破棄できなければ closed に進めない）。scripts/check/core.sh#11・chain.sh#9 が確認する。
2. ballot_id は UUIDv4。UUIDv7 など時刻を含むIDは使用禁止。
3. 時刻は分単位に丸めて保存する。ブロック内の票は ballot_id のハッシュ順に並べる。
4. ブロックのハッシュ計算は固定長ビッグエンディアンのバイナリ正規化で行う
   （serde_json等でハッシュ対象を作らない）。SHA-256を使用。
   ブロックの形式の版 2 から、票の contest_id / candidate_id（文字列 ID）は、2 バイトの固定幅の
   長さ接頭辞つき（ballot_id(16) ‖ len(2) ‖ contest_id ‖ len(2) ‖ candidate_id。ADR 0013）。
   白票の票は、candidate_id に予約値 "blank" をそのまま入れる（形式の版は変えない。ADR 0021）。
   版 3 から、再投票のつながりを持つ票だけ、後ろに 0x01 ‖ slot(32) ‖ seq(4)（初回）または
   0x02 ‖ slot(32) ‖ seq(4) ‖ supersedes(32) を足す（つながりの無い票は版 2 と同じバイト列。ADR 0022）。
5. 分離: crates/web は crates/shared-types 以外のワークスペースクレートに依存しない。
   crates/domain は IO・async ランタイム・DBクレートに依存しない。
6. api はステートレス。セッションは HMAC 署名トークン。サーバメモリに状態を持たない
   （候補者マスタの読み取りキャッシュは例外）。
7. 認証はスコープ外。Authenticator トレイト + StubAuthenticator
   （入力IDをvoter_idとして採用、マイナンバー欄は受け取って破棄）。
8. unsafe 禁止。unwrap/expect はテストコードのみ。エラーは thiserror（ライブラリ）/
   anyhow（バイナリ）。
9. ブロックの封印ルール（シャードごとに独立して判定）:
   - 未封印が seal.max_ballots(100) 件に達したら、100 件ですぐに封印する
   - 前回の封印（または投票開始）から seal.interval_secs(600) 秒以上経ち、かつ未封印が
     seal.min_ballots_after_interval(10) 件以上なら、全件を封印する
   - 上の2つに当てはまらなければ待つ（0件のときも待つ。窓のリセットはしない）
   - 投票終了の手続きの中でだけ、残りを件数に関係なく封印する（1件以上ある場合）。
     SIGTERM で停止するときはフラッシュしない
   実装（ADR 0020）: 判定は domain::seal_policy の純粋関数（時刻は引数）。結果は SealCount(100) / SealAll / Wait /
   CloseFlush の 4 つ（decide は投票期間中、decide_close は締切の手続きの中。250 件なら 100・100（count）・50（close））。
   経過時間の起点 = max(前回の封印時刻, 投票開始時刻)（window_start）。投票開始時刻は domain::voting_started_at
   （open に遷移した時刻 election_state.opened_at と election.voting_opens_at の遅い方）。時刻は壁時計の UNIX 秒で測る。
   引き継ぎ・再起動の後は、前回の封印時刻を先頭ブロックの sealed_at_minute の分の最後の秒とみなす（早くは封印しない）。
   ログの trigger は count | time | close の 3 種類。設定の最小件数が max_ballots より大きい場合は、時間では封印されない。
   scripts/check/chain.sh#8 が、dev の設定（interval=10 秒・min=10）で確認する。
10. 確認は scripts/check/<スイート名>.sh にまとめ、scripts/check_all.sh ですべて実行する。
    共通の処理は scripts/lib/common.sh に置く。新しい機能を追加するときは、関係するスイートに
    確認項目を追加する。
11. 設定は config/ 配下の TOML に集約する。読み込みの優先順は
    config/default.toml → config/{APP_ENV}.toml → config/local.toml（gitignore）→ 環境変数 APP__*。
    トークン用のシークレットや署名鍵のシードなどの秘密情報は、設定ファイルに書かず
    環境変数か gitignore されたファイルから読む。
    実装（crates/app-config）: 環境は app.env（環境変数 APP__APP__ENV。既定 dev）で選ぶ。優先順位（後勝ち）は
    default.toml（バイナリに埋め込み）< config/<app.env>.toml < config/local.toml < secrets/ < 環境変数
    APP__<セクション>__<項目>。秘密情報（session.secret、sealer.signing_seed）は設定ファイルに書くとエラーで、
    環境変数か secrets/（gitignore。1 ファイル 1 値）から読む。api / sealer / verifier / bench / スクリプトはすべて
    app-config から読み、設定値の直書きや環境変数の直接参照をしない（scripts/check/core.sh・scripts/check/docs.sh が確認する）。
    不正な値は、起動時に「どのファイルのどの項目がなぜ不正か」を表示して終了する。未実装の機能に対応する項目
    （投票の開始時刻 voting_opens_at など）は、指定されたら「未実装」で起動を失敗させる。
    voting_closes_at は、verifier tally の締切判定と、chain.reveal_ballots=after_close の公開の判定に使う（api は締切後の投票を拒否しない）。
    web は app-config に依存せず、ビルド時に必要な値（labels.*）だけを `app-config web-env` の環境変数で受け取る。
12. 利用者に見える文言（画面・エラーメッセージ・集計結果の表示名）はコードに直接書かず、
    設定の labels から読む。コード内部の型名や API のパスは contest のまま残す。
    投票用紙 1 枚の呼び名は labels.ballot_item（既定「投票用紙」）、進捗の表示は labels.progress
    （既定「{total}枚中{current}枚目」）。白票は、選択肢が labels.blank_option、確認画面が labels.blank_confirm、
    集計・ビューア・API のエラーでの呼び名が labels.blank_name（原則20）。再投票は、完了画面のボタンが labels.revote_button、
    確認画面が labels.revote_confirm、上限の理由（画面・API のエラー）が labels.revote_limit_reached（{max} は上限回数）。画面・API のエラー・集計に、旧来の呼び名（contest を片仮名にした語）は
    使わない（crates/・config/・seed/・scripts/・README・CLAUDE.md を scripts/check/docs.sh が確認する）。
13. ID は変更されない文字列コードにする。区割り変更など、将来変わり得る意味を ID に埋め込まない。
    都道府県は JIS X 0401 の2桁コード（01〜47）を使う。
    ID 体系（crates/domain/src/ids.rs。読み込み時に、形式・文字種・最大長を検証する）:
    election_id（例 2026-general）、election_type（例 shugiin_smd）、district_id（例 shugiin_smd.13.01。
    先頭のセグメントが選挙の種類）、contest_id = {election_id}/{district_id}、candidate_id =
    {district_id}.c{連番}（型は domain::CandidateCode。票の投票先は domain::CandidateId = Blank | Candidate(CandidateCode)。
    予約値 "blank" は白票で、選挙データの候補者コードには使えない＝読み込み時にエラー）。文字種は小文字の英数字・_・-（区切りは . と /）。1 つの選挙区が複数の都道府県に
    またがる場合（合区）も、ID は変えず、選挙区の属性 prefectures（リスト）で持つ。
    選挙データは seed/<election_id>/（election.toml・districts.csv・candidates/<選挙の種類>.csv・voters.csv）。
    読み込みと検証は crates/seed、ダミーデータの生成と検証は seedgen（ADR 0014）。
    投票する順番は固定（表示順: 選挙の種類の order、次に選挙区の order）。利用者は選べず、画面は先頭の
    未投票へ自動で進む。有権者は、voters.csv の名簿にある選挙区の投票用紙にだけ投票できる（対象外は 403）。
14. 投票期間中に、公開 API やビューアから候補者別の票の中身や集計を見られないようにする
    （chain.reveal_ballots で制御する）。
    実装（ADR 0017）: ブロックチェーンのビューア（画面 /chain 以下・API /api/v1/chains・/anchors。ログイン不要）。
    reveal_ballots=after_close（要 election.voting_closes_at）の締切前は、ブロックの詳細に票の中身（ballot_id・
    contest_id・candidate_id・再投票の slot / seq / supersedes）を返さず、ヘッダーの情報だけを返す（ballots_revealed=false）。
    突合の集計（/api/v1/audit/counts）の cast（再投票を含む票の数）と pending_initial も、締切前は返さない。
    締切後は、再投票の票に「#<前の票> を置き換え（A→B）」のリンクを出す（ADR 0022）。締切の判定は api の時計。
    Cache-Control: public, max-age=31536000, immutable は、内容が確定した応答だけに付ける（票を返す詳細・before_height が
    先頭 + 1 以下の一覧）。締切前の（票を伏せた）詳細は no-store（締切後に中身が変わるので、immutable にすると CDN が
    票なしの版を保持する）。エラーも no-store。票が非公開の間は、verifier の検証・集計もできない（終了コード 4）。
    scripts/check/chain.sh が確認する（ブラウザでの確認は docs/manual_check_step14.md）。
    集計は verifier tally（scripts/tally.sh。ADR 0016）: 集計の前に、チェーン全体の検証（再投票のつながり: slot ごとに seq が
    1 から連続・supersedes が 1 つ前の票のハッシュ・seq が max_revotes+1 以下・allow_revote=false なのに seq>1 の票がない。ADR 0022）と
    投票済み記録との突合（participation = チェーン内の slot の数（重複を除く）+ 未封印の最初の票）を必ず
    行い、失敗したら集計しない（終了コード 3。投票用紙の候補者でも白票でもない票がチェーンにあるときも 3。白票は候補者とは別に数え、
    表では候補者の後の別の行、CSV では白票の列に出す。ADR 0021。再投票は slot ごとに最後の票だけを数え、締切後の集計だけ、
    再投票の件数と変更の内訳（A→B の件数表。revotes.csv）も出す。ADR 0022）。未封印の票が残っていたら、件数を表示して中止する（4。残りの票は締切の
    手続き（closing）の中でだけ封印されるので、closed を待ってから再実行）。選挙状態が closed より前は --allow-interim が
    なければ集計しない（4。原則18。--allow-interim は app.env=dev のときだけ）。出力は、表と out/tally/{日時（UTC）}/ の CSV・JSON（scripts/check/chain.sh が確認する）。
15. パスワードは Argon2id でハッシュ化して保存する。平文はログにもDBにも残さない。
    ログインが失敗したとき、「IDが存在しない」と「パスワードが違う」を応答内容でも
    応答時間でも区別できないようにする。
    実装（ADR 0015）: auth.mode=db（app.mode=db が必要）は、事前登録の ID とパスワードだけを認証する。
    登録は CLI credgen（crates/credgen）: 暗号論的乱数で、見間違えやすい文字（0/O/1/I/l）を除いた
    32 文字から、ログイン ID（credentials.login_id_length）とパスワード（credentials.password_length）を作る。
    DB の credentials には login_id・Argon2id ハッシュ・voter_id（内部用の乱数。ログイン ID とは別）だけを保存する。
    平文のパスワードは、credentials.output_file_enabled=true のときだけ output_path に CSV
    （login_id, password, 都道府県, 選挙区。権限 0600・上書きしない・gitignore）で出力する。false のときは警告して、
    --confirm-no-output がなければ処理しない。2 回目の実行は既定でスキップ、--reissue で再発行。
    ログインの失敗（存在しない ID・パスワード違い・形式不正）は、同じ 401・同じ文言・ダミーハッシュの照合で
    応答時間も揃える。マイナンバー欄は受け取って破棄する（保存もログ出力もしない）。
    DB のリセットは scripts/db_reset.sh（--votes は投票データだけ、--all はキースペースごと。
    app.env=production では拒否。api / sealer が動いていたら止めるよう促して終了。scripts/check/auth.sh が確認する）。
16. 画面のデザインは、CSS カスタムプロパティのデザイントークンで定義する（ADR 0018）。色・余白・角丸・フォントサイズは
    crates/web/style.css の :root に定義し、[data-theme="dark"] は色のトークンだけを上書きする。色を、トークン以外の場所
    （CSS のルール・Rust・index.html）に直接書かない。ライト（白基調）とダークを、ヘッダーの切り替えボタン（ライト / ダーク /
    OSに合わせる）で切り替える。初回は prefers-color-scheme に従い、選択は localStorage の "theme" に保存する（保存してよいのは
    テーマの設定だけ。トークンや投票の内容は保存しない）。WASM の読み込み前に配色を決めるため、index.html の <head> 内
    （スタイルシートより前）のインラインスクリプトが data-theme を先に設定する（規則は web/src/theme.rs と同じ）。
    文字と背景のコントラスト比は、両方のテーマで WCAG AA（4.5:1 以上。枠線・フォーカスは 3:1 以上）とし、web::theme の
    テストがトークンの値から計算して確認する。選択状態は、色だけで表さない（✓・太い枠・記号・⚠）。
    scripts/check/web.sh が確認する（目で見る確認は docs/manual_check_step15.md）。
17. 選挙状態は scheduled → open → closing → closed の順にしか進まない。
    DB（memory モードではプロセス内）を正とし、変更は管理用のポートか scripts/election.sh からのみ行う。
    期間は秒まで指定し、タイムゾーンのオフセットを必ず付ける（RFC 3339）。
    実装（ADR 0019）: 状態は election_state（単一行）、変更履歴は election_audit（日時・変更前・変更後・
    実行した主体）。init（スキーマを作った後の最初の接続）に、設定の期間を取り込む。その後に設定ファイルの
    期間と DB の期間が違っていたら、起動時に警告し、DB の値を使う。自動遷移（scheduled→open、open→closing、
    closing→closed の締切の手続き）は、アンカーのリースを持つ sealer が行う（memory モードでは api 内蔵の
    スケジューラ）。closing を検知したら、担当（アンカーのリースの有無を問わない）ごとに、持っているシャードを
    直ちにフラッシュする。締切の手続きの待ち時間は election.state_cache_secs + api.request_timeout_secs。
    待ち時間の後に、再投票の鍵（secrets/revote_key）を破棄し、election_audit に revote_key_destroyed を記録する（ADR 0022）。
    管理用リスナー（admin.bind。既定 127.0.0.1:18081）は公開用のポート（api.port）とは別で、トークン
    （admin.token。秘密情報）が無ければ起動しない。open --now / close --now は 1 段の遷移だけを行い、
    締切の手続き自体は常に上記の自動の仕組みが行う。
18. 投票を受け付けてよいのは「状態が open」かつ「開始時刻 <= 現在時刻 < 終了時刻」のときだけ
    （開始時刻は含み、終了時刻は含まない）。
    実装（ADR 0019）: 判定は domain::vote_gate（状態と期間の両方を見る。open --now で期間前に手動で開けた
    場合も判定できるようにするため）。開始前・締切の手続き中・終了後で、別のメッセージ（labels の
    voting_not_started_message / voting_closing_message / voting_closed_message）を返す。ログイン画面・
    進捗画面にも、期間と今の状態を表示する（GET /api/v1/election-status。認証不要）。verifier tally は、
    選挙状態が closed のときだけ実行できる（app.env=dev のときだけ --allow-interim を許可）。
19. 選挙のルール（再投票の可否、再投票の上限、封印ルール）は、open に移った時点で固定し、
    それ以降は変更できない。
    実装（ADR 0021・0022）: 固定しているのは白票の可否（vote.allow_blank）と、再投票の可否・上限（vote.allow_revote /
    vote.max_revotes）。domain::ElectionRules を、open への遷移と同じ条件付き書き込み（DB は election_state.allow_blank /
    allow_revote / max_revotes の LWT、memory はプロセス内）で保存し（ElectionStateStore::transition の
    引数。open 以外の遷移では無視）、以後は ElectionRules::effective で固定した値を使う。固定するのは、open に遷移させた
    プロセス（sealer / api 内蔵のスケジューラ / open --now の api）の設定の値。設定と食い違っていたら起動時に警告する。
    固定したルールは GET /api/v1/election-status の rules で公開する（verifier が再投票のつながりの検証に使う）。
    （封印ルールの固定は未実装。）
20. 白票（どの候補者にも投票しない）を選べる。API は candidate_id の予約値 "blank"（小文字の完全一致）を受け付ける。
    実装（ADR 0021）: 候補者の一覧 API は、白票を含めず allow_blank（固定した vote.allow_blank）を返し、画面は真のときだけ
    候補者一覧の最後に白票の選択肢を置く（web::flow::choices）。確認画面は候補者名の型に当てはめず labels.blank_confirm を出す。
    vote.allow_blank=false なら、画面に出さず、API は 422 blank_not_allowed で拒否する。ビューアは白票を別の書式・別の行で、
    集計は候補者とは別の行で表示する。scripts/check/core.sh#10（API・封印・集計）と web.sh#4（画面）が確認する。

## 技術スタック
- Rust stable, edition 2024, tokio
- API: axum 0.8系, tower-http（trace, cors, timeout）, tracing
- 画面: Leptos 0.8系 CSR + Trunk + leptos_router
- DB: ScyllaDB（scylla クレート 1.x）。ローカルは docker-compose
- 暗号: sha2, ed25519-dalek, rand
- テスト: cargo test, proptest（チェーン検証の性質テスト）
- 依存を追加するときは、その時点の最新安定版を確認し、理由をPR説明に書くこと

## 作業ルール
- 各フェーズ開始時に計画を提示し、承認後に実装する
- cargo fmt / cargo clippy -- -D warnings / cargo test を通してから完了報告する
- 設計判断は docs/adr/NNNN-*.md に「背景・決定・理由・代替案」で記録する
