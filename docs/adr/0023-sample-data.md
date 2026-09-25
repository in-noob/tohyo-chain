# 0023: 確認用のサンプルデータ（scripts/sample_data.sh）と、テスト仕様を兼ねる CSV

## 背景
- 画面や API を手で確認するとき、「未投票」「一部だけ投票済み」「白票で投票済み」「再投票の上限に到達」「再発行済み」など、
  ユーザー単位の状態を毎回手作業で作っていた。状態を作るには、DB のリセット・選挙データ・選挙期間・有権者の登録・投票・封印を、
  正しい順番で行う必要がある。
- 期待される結果（ログインできるか、投票が何で拒否されるか）は、選挙状態（原則17・18）と、open で固定した選挙のルール
  （原則19: `allow_blank` / `allow_revote` / `max_revotes`）と期間で変わる。手で書いた期待結果は、設定を変えるとすぐに古くなる。
- 必要な部品（`db_reset.sh` / `seedgen` / `credgen` / `election.sh`）は、すべてそろっている。

## 決定
- `scripts/sample_data.sh [--phase before|open|closed] [--yes]` を足す。**新しい仕組みは作らず、既存の部品を順に使う**:
  1. `scripts/db_reset.sh --all`（スキーマの作成を含む。production の拒否・api / sealer の稼働中の拒否・確認もそのまま使う）
  2. `seedgen` で小規模の選挙データ（32 都道府県・各 1 選挙区・候補者 2 人）を作り、名簿（voters.csv）をパターンの有権者だけに書き換えて、
     `seedgen --check` で検証する。32 は、参議院の合区（鳥取・島根 = `31_32`）が現れる最小の都道府県の数（P03）。
     P02（投票用紙 1 枚）・P04（0 枚）は、名簿の選挙区をそれぞれ `supreme_court_review.national` だけ・空にする
  3. api・sealer を起動し、`scripts/election.sh schedule` で期間を設定する（open / closed は開始 = 今、before は開始 = 今から
     `sample.open_hours` 時間後。長さはどれも `sample.open_hours`）。開始時刻は、sealer が自動で open にする（既存の仕組み）
  4. `credgen` で登録する。有権者 ID はパターン ID の小文字（`p01` …）にする。credgen は有権者 ID の順に CSV を書くので、
     行とパターンが対応する。P13 は、その有権者だけの名簿（選挙の定義は同じもののシンボリックリンク）で `credgen --reissue` を実行する
  5. 事前の投票を、公開 API（`/api/v1/login`・`/api/v1/contests/…/vote`）で行う
  6. 封印: open は封印のルール（原則9）に任せて待つ（最小件数に満たなければ、その旨を表示して進む）。closed は
     `election.sh close --now` の締切の手続き（全件の封印・revote_key の破棄・closed への遷移）を待つ
  7. `<sample.output_dir>/credentials_patterns.csv` を出力し、要約表を表示して、api・sealer を止める
- **CSV をテスト仕様にする**: 期待結果の列は「<操作> → <結果>」の形にする（例: `2枚目に候補者1で投票 → 成功（201）`、
  `1枚目を候補者2でやり直し → 拒否（409 revote_limit_reached）`）。操作（どの投票用紙に・どの投票先で）と、結果（HTTP ステータスと
  エラーの種類）が読めるので、人が画面で試す手順にも、スクリプトが API で試す入力にもなる。
  - 期待結果は、実行の最後に `GET /api/v1/election-status`（状態・期間・固定したルール）と `GET /api/v1/ballot-status`（各有権者の
    投票用紙と `ballots_cast`）を読んで、API の判定の順（受付期間 → 再投票の可否 → 投票対象か → 投票先 → 投票済みか → 上限）で計算する。
  - 試す順は、ログイン → 再投票 → 投票（投票の確認で未投票が投票済みに変わると、再投票の期待結果が変わるため）。
  - `scripts/check/auth.sh#2` が、3 つの phase で実行し、api・sealer を起動し直して、各行を実際に試す。
- 事前の投票が必要なパターン（P05〜P09）は、before では作れないので「作成不可（開始前のため）」と書き、ID・パスワードを空にする。
  白票・再投票が選挙のルールで無効なとき（P07 / P08・P09）も、同じく作成不可（理由つき）にする。
- 設定: `sample.open_hours`（既定 24。1〜720）と `sample.output_dir`（既定 `out/sample`。git 管理外）を足す（原則11: スクリプトの
  値も app-config から読む。出力先を設定にしたのは、確認スイートが手元の `out/sample` を上書きしないようにするため）。
- 開発用の秘密情報（`dev_up.sh` の固定値）と、再投票の鍵の生成は、`scripts/lib/common.sh`（`dev_default_secrets` /
  `ensure_revote_key`）に移し、`dev_up.sh` と共用する。署名鍵の種が同じでないと、作ったデータを `dev_up.sh cassandra` で
  開いたときに、sealer が「登録済みの公開鍵と違う」と起動を拒否するため。

## 理由
- 既存の部品を使えば、本番と同じ経路（credgen の Argon2id・LWT、API の受付判定、sealer の自動遷移と締切の手続き）で状態が
  できる。DB に直接書き込むと、participation・slot_state・ballot_pool の整合（ADR 0022）を自前で再現することになり、壊れやすい。
- 期待結果を実際の状態とルールから計算するので、設定（`allow_revote` など）を変えても CSV が古くならない。その CSV を
  スクリプトが実際に試すので、期待結果の計算そのものも検証される（計算と API の判定が食い違えば、auth.sh#2 が失敗する）。
- 平文のパスワードを含むファイルは、`credgen` と同じく権限 0600・git 管理外にする。途中で使う credgen の CSV は、終了時に消す。
  要約表にはパスワードを出さない。サンプルデータはダミーの有権者なので、パターン名が投票の状態（白票・A→B）を表すことは
  秘密投票（原則1）の対象外（実在の有権者の記録ではなく、production では実行できない）。

## 代替案
- **DB に直接 INSERT する**: 速いが、票・封印・再投票の鎖（supersedes）・監査ログを自前で作ることになり、
  本番の経路と食い違う。不採用。
- **期待結果を手で書いた固定の表にする**: 設定や phase の組み合わせごとに表が要り、変更に追従できない。不採用。
- **seedgen に「パターン用」の生成オプションを足す**: 名簿の 1 行を書き換えるだけのために、Rust の生成ロジックと
  そのテストを増やすことになる。名簿は CSV なので、スクリプトで書き換えて `seedgen --check` で検証する方が小さい。不採用。
- **sample_data.sh が api・sealer を起動したままにする**: 画面（trunk）が無いので結局 `dev_up.sh` が要り、ポートが衝突する。
  止めて、`dev_up.sh cassandra` の起動方法（選挙データの場所）を表示する方にした。
