# 確認スイートの構成（scripts/check/）

`scripts/check_step0.sh`〜`scripts/check_step15.sh`（16 本、計 3733 行）を、機能ごとの 6 つのスイートに再編した。
共通処理（設定・DB・選挙データ・起動停止・アサーション）は `scripts/lib/common.sh` に集約し、各スイートはそこから読む。

```
scripts/lib/common.sh      共通関数（設定 / DB / 専用キースペース / 選挙データ / 起動・停止・待ち合わせ / アサーション）
scripts/check/core.sh      起動・設定・封印ポリシーの単体テスト・性能計測ツール・白票・再投票（API・封印・集計。memory）
scripts/check/chain.sh     ハッシュチェーン（封印・DB永続化・複数sealer・「更新がなければ追加しない」・集計・ビューア・再投票（DB））
scripts/check/election.sh  投票フローと選挙データ（投票・名簿・大規模データ・壊れたデータの検出・投票順）・選挙状態の遷移
scripts/check/auth.sh      ID・パスワードの事前登録（credgen）・DB 認証・DB のリセット
scripts/check/web.sh       画面（flow・wasm ビルド・デザイントークン・ダークモード・白票の選択肢・投票のやり直し）
scripts/check/docs.sh      呼び名・環境変数名の一貫性、スクリプトの構文（コード・設定・ドキュメントの照合）
scripts/check_all.sh       6 つのスイートを順番に実行し、成功・失敗と所要時間の表を表示する
```

各スイートは、旧スクリプトの該当部分を関数として取り込み、`名前() ( ... )`（サブシェル）で呼ぶ。サブシェルにしたのは、
複数の旧スクリプトを 1 ファイルに集めても、`trap`・変数（`PID`・`BASE`・`LOG` など、旧スクリプトが共通で使っていた名前）が
互いに干渉しないようにするため。失敗はサブシェルの終了コードとして伝わり、呼び出し元は `set -e` で止まる（旧来と同じ）。

## 対応表

「新」列は、確認項目を移した先（`スイート#節番号`）。「除外」は、この再編で削った項目とその理由。

| 旧スクリプト | 項目 | 新 |
|---|---|---|
| check_step0.sh | api 起動、GET /healthz が 200 で `{"status":"ok"}`、リクエストログが出る | core.sh#1 |
| check_step1.sh | verifier demo: 正常チェーンの検証 OK・改ざん検出（MerkleRootMismatch・包含証明失敗）・表（0/100/100/50）・未知のサブコマンドは exit 2 | chain.sh#1 |
| check_step2.sh | domain::seal_policy の単体テスト（必須ケース 9 件を含む。ADR 0020 で入れ替え） | core.sh#2 |
| check_step3.sh | dev-tools 無効で /debug/pool が 404 | election.sh#1 |
| check_step3.sh | ログイン・状態（表示順・固定）・候補者取得・投票 201・再投票 409・対象外 403・名簿にない有権者は空 | election.sh#1 |
| check_step3.sh | 並列 100 リクエスト（同一 voter・同一投票用紙）で成功ちょうど 1 件（メモリのミューテックス） | election.sh#1 |
| check_step3.sh | サーバログに投票者 ID・マイナンバーが出ない | election.sh#1 |
| check_step4.sh | clippy/test（--features dev-tools）、起動・ジェネシス確認 | chain.sh#2 |
| check_step4.sh | 250 票投入 → count トリガーで height=1,2（各 100 件）、time トリガーで height=3（50 件）、以降ブロックが増えない | chain.sh#2 |
| check_step4.sh | verify OK（blocks=4 ballots=250）、/debug/tamper 後は verify NG | chain.sh#2 |
| check_step4.sh | 最小件数未満（9 票）を投入 → SIGTERM ではフラッシュしない（ブロックが増えない。ADR 0020 で反転。30 票だと停止前に時間で封印されうるので 9 票に変更）、ログに投票者 ID が出ない | chain.sh#2 |
| check_step5.sh | flow モジュールの単体テスト（必須ケース 19 件を含む）、折り返し（wrap-around）が無いことの grep | web.sh#1 |
| check_step5.sh | flow / error が UI・ブラウザ API に依存しない（原則5） | web.sh#1 |
| check_step5.sh | wasm 向け clippy | web.sh#1 |
| check_step5.sh | trunk build --release の成功、dist/ のサイズ | web.sh#3（step15 と統合。下記「除外」参照） |
| check_step6.sh | DB 起動、専用キースペースへのスキーマ投入（2 回流して冪等性を確認） | chain.sh#4 |
| check_step6.sh | infra-scylla の統合テスト | chain.sh#4 |
| check_step6.sh | 起動 1: ジェネシス確認・投票 1 件・並列 100（DB の LWT）・SIGTERM ではフラッシュしない（ADR 0020） | chain.sh#4（一部除外。下記参照） |
| check_step6.sh | 起動 2: 再起動後も投票状態・未封印の票・チェーンが保持される、10 件目で時間による封印（起点は DB の投票開始時刻）、100 件で件数による封印・verify OK | chain.sh#4 |
| check_step6.sh | 起動 3: 5 票を残して SIGKILL（クラッシュ）→ 再起動 → 5 票を追加 → リース期限切れを待って引き継ぎ → 10 件を時間で封印・保持・復旧・verify OK | chain.sh#4 |
| check_step7.sh | DB 起動・スキーマ投入、ビルド | chain.sh#5 |
| check_step7.sh | sealer 2 プロセスで 4 シャードのリースを排他的に取得 | chain.sh#5 |
| check_step7.sh | api（app.mode=db）起動 → 全シャードのジェネシスが読める | chain.sh#5 |
| check_step7.sh | 3 種類の投票用紙に 1000 票投入 | chain.sh#5 |
| check_step7.sh | 片方の sealer を kill -9 → もう片方がリースを引き継ぎ、そのシャードを封印する | chain.sh#5 |
| check_step7.sh | 全票封印、分岐なし（ログ上・DB 上とも、同じ (shard,height) の重複や欠番がない） | chain.sh#5 |
| check_step7.sh | アンカー: 全シャードの head を含む・変化がなければ増えない | chain.sh#5 |
| check_step7.sh | verify OK（4 シャード・1000 票・投票用紙別の突合・重複なし・アンカー） | chain.sh#5 |
| check_step7.sh | 正常停止でリース解放、ログに投票者 ID・USE 警告が出ない | chain.sh#5 |
| check_step8.sh | bench クレートの単体テスト | core.sh#9 |
| check_step8.sh | スモーク計測（1 構成・定常→ドレイン→飽和・DB 直接計測）と出力内容 | core.sh#9 |
| check_step9.sh | app-config のテスト、設定ファイル一式（default/dev/production.example/.gitignore/Trunk.toml の整合）、必須項目・コメント | core.sh#3 |
| check_step9.sh | 設定ファイルの値を書き換えると動作が変わる（seal.max_ballots 等・環境別ファイル） | core.sh#4 |
| check_step9.sh | 環境変数が設定ファイルより優先される、show の出所表示 | core.sh#5 |
| check_step9.sh | 秘密情報は secrets/・環境変数から読める、設定ファイルには書けない、show/get/エラーに出ない | core.sh#6 |
| check_step9.sh | 不正な設定で、api/sealer/verifier/bench が理由つきで起動失敗、verifier の --api 省略 | core.sh#7 |
| check_step9.sh | labels.* がビルド時に web へ渡る、web は app-config に依存しない | core.sh#8 |
| check_step9.sh | 旧来の環境変数名が残っていない、環境変数の直接参照がない（app-config 以外） | docs.sh#2 |
| check_step9.sh | CLAUDE.md に原則 11 の記載がある | docs.sh#2 |
| check_step9.sh | 全スクリプトの構文（bash -n） | docs.sh#5 |
| check_step10.sh | 投票 0 件で 60 秒待ってもブロック・アンカーが増えない（skip ログを含む） | chain.sh#3 |
| check_step10.sh | 10 票（最小件数）投票 → ブロック・アンカーが 1 つ増える、さらに 60 秒待っても増えない・verify OK | chain.sh#3 |
| check_step10.sh | 変化なしで SIGTERM → 最終アンカーは確認するだけ（作らない） | chain.sh#3 |
| check_step10.sh | 封印前に SIGTERM → フラッシュしない（ブロックもアンカーも作らない。ADR 0020 で反転） | chain.sh#3 |
| check_step11.sh | 47 都道府県規模データ（候補者 1 万人以上）の生成・読み込み、合区・比例ブロックの表現 | election.sh#2 |
| check_step11.sh | 東京 1 区の有権者に、関係する 9 枚だけが表示順で見える、対象外は 403、他県も同様 | election.sh#2 |
| check_step11.sh | 投票の順番が表示順どおり（先頭の未投票へ進む）、flow の該当テスト、verify | election.sh#2 |
| check_step11.sh | 壊れたデータ（重複 ID・存在しない参照・不正な ID・長すぎる ID・列不足など）がファイル・行・原因つきで検出される | election.sh#2 |
| check_step11.sh | 旧来の呼び名「コンテスト」がどこにもない | docs.sh#1 |
| check_step12.sh | credgen で 100 人分を登録（CSV の件数・列・権限 0600・文字種）、DB にはハッシュのみ | auth.sh#1 |
| check_step12.sh | 正しい ID/パスワードでログイン・投票、パスワード違い/存在しない ID/形式不正が同じ応答・時間で失敗 | auth.sh#1 |
| check_step12.sh | output_file_enabled=false の警告、2 回目はスキップ、--reissue で再発行 | auth.sh#1 |
| check_step12.sh | db_reset --votes（動作中は拒否・確認なしでは消えない・投票データのみ削除） | auth.sh#1 |
| check_step12.sh | db_reset --all（認証情報も消える）、production では拒否、memory では案内のみ | auth.sh#1 |
| check_step13.sh | 決まった投票の投入、未封印が残っていると中止（--allow-interim でも） | chain.sh#6 |
| check_step13.sh | close --now（締切の手続きのフラッシュ）→締切後の tally が期待値と一致（候補者別・選挙区別・都道府県別・種類別・順位・白票・突合） | chain.sh#6 |
| check_step13.sh | 締切前は --allow-interim がないと拒否、ありなら中間集計 | chain.sh#6 |
| check_step13.sh | 改ざんされたブロックは集計を拒否 | chain.sh#6 |
| check_step14.sh | 30 票投入・3 票ごとに封印、ブロックの一覧のページ送り（新しい順・欠落なし・limit・before_height の境界） | chain.sh#7 |
| check_step14.sh | Cache-Control（先頭からのページは no-cache、確定したページは immutable） | chain.sh#7 |
| check_step14.sh | 締切前は票の中身が API に含まれない（no-store）、verifier は「票が非公開」で終了コード 4 | chain.sh#7 |
| check_step14.sh | 締切後は票の中身・表示名が含まれる（immutable）、verify OK | chain.sh#7 |
| check_step14.sh | シャードの一覧、アンカーの一覧（先頭ブロックへのリンク）、404（no-store） | chain.sh#7 |
| check_step14.sh | trunk build --release | **除外**（下記） |
| check_step15.sh | 色がトークン以外に直接書かれていない（CSS・Rust・index.html） | web.sh#2 |
| check_step15.sh | テーマのテスト（保存値の解釈・OS 設定への追従・index.html のスクリプトの規則・トークンの構造・コントラスト比 4.5:1/3:1） | web.sh#2 |
| check_step15.sh | 選択状態を色だけで表さない部品がある（candidate/theme-option/current/voted） | web.sh#2 |
| check_step15.sh | trunk build --release、dist にスクリプト・トークンが残る | web.sh#3（step5 と統合） |

## 除外した項目と理由

1. **check_step6.sh「起動 1」の状態一覧・候補者一覧の詳細検証**（有権者に関係する 9 枚だけが表示順で見える・候補者 4 名など）。
   この検証は、api のハンドラ層（`application::VotingService` とルーティング）のロジックで、保存先（メモリ / DB）に依存しない。
   `election.sh`（旧 check_step3）で、メモリ実装に対してすでに同じ内容を検証している。DB 固有の価値がある部分
   （並列 100 リクエストでの LWT の排他制御、投票 201・再投票 409、SIGTERM でフラッシュしないこと、再起動後の永続化）は、
   `chain.sh#4` にそのまま残した。
2. **check_step14.sh の `trunk build --release`**（項目 6）。`web.sh`（旧 check_step5・check_step15）が、
   `crates/web` 全体（ビューアの画面を含む）を 1 回 `trunk build --release` でビルドし、成功を確認している。
   ビューアの API 側の検証（ページ送り・Cache-Control・締切前後の公開）はサーバの応答だけを見るもので、
   画面のビルドとは独立に検証できるため、`chain.sh` 側では画面のビルドを行わない。
3. **check_step5.sh と check_step15.sh の、それぞれの `trunk build --release`**。同じ `crates/web` を、同じ設定
   （`app-config web-env`）で 2 回ビルドしていた。`web.sh` では 1 回だけビルドし、その成果物（`crates/web/dist`）に対して、
   旧 step5 の確認（dist のサイズ）と旧 step15 の確認（インラインスクリプトの位置・CSS のトークン）の両方を行う。
4. **check_step9.sh の api/sealer/verifier のビルドから `--features dev-tools` を外した**（core.sh）。
   設定の確認（不正な値での起動失敗・秘密情報の扱いなど）は、`/debug/pool` や `/debug/tamper` を一度も呼ばない。
   `dev-tools` はビルド時間にはほぼ影響しないが、実際に使わない機能フラグを外し、何を検証しているかを明確にした。

## スイート内で統合した、重複していたセットアップ

機能や検証項目は変えていない。同じスイートに複数の旧スクリプトを集めたことで、次の重複を除けた（`check_all.sh` が
速くなった主な要因）。

- **`cargo build`**: 旧スクリプトは、それぞれが必要なバイナリを個別にビルドしていた（例: check_step6/7 は、ほぼ同じ
  `cargo build -q -p api --features dev-tools -p sealer -p verifier` を、スクリプトごとに実行）。各スイートの先頭で、
  そのスイートが必要とするバイナリをまとめて 1 回だけビルドする。
- **DB の前提確認・スキーマの冪等性確認**: `chain.sh` は、DB を使う 3 つの旧スクリプト（step6・7・13）を含むが、
  `db_check_docker` / `db_static_checks` は最初の 1 回だけ行う。スキーマの冪等性（2 回流しても成功する）の確認も、
  最初のキースペース（旧 step6 相当）だけで行い、以降のキースペース（旧 step7・13 相当）は 1 回の投入にする
  （3 つとも、投票データが混ざらないよう、専用のキースペースは個別に作る。空でないと初期状態の前提が崩れる検証
  （ジェネシスからの高さなど）があるため、キースペースの共有はしていない）。
- **`trunk build --release`**: 上記「除外した項目」3 を参照。

## 不要になった設定項目・dev-tools エンドポイント・スクリプト引数の調査（洗い出し）

再編にあたり、次の観点で洗い出した。

- **設定項目**（`crates/app-config/src/model.rs` の全フィールド）: Rust コード（api/sealer/verifier/bench/credgen/web）
  または確認スイート（シェル）のどちらかから、すべて読まれていることを確認した。未使用の項目は無かった
  （`db.backend` は Rust コードでは使わないが、`scripts/lib/common.sh` の `db_setup_vars` が `cfg_get db.backend` で
  読み、どの docker compose サービス・スキーマジョブを使うかを決めるのに使っている）。
- **dev-tools エンドポイント**（`GET /debug/pool`・`POST /debug/tamper`）: どちらも、確認スイート・`scripts/requests.*`・
  `scripts/bench.sh`・ADR で継続して使われている。未使用のものは無かった。
- **スクリプトの引数・環境変数**: 個別の旧スクリプトが確認ごとに定義していた `CHECK_API_PORT` の既定値
  （`18080`〜`18096`、旧スクリプトの数だけあった）は、スイートの数（6）分あれば足りるため、スイートごとに 1 つに
  集約した（1 つのスイートの中では、複数の旧スクリプトが同じポートを順番に使い回す。同時に 2 つのサーバが立つことはない）。
  `WITH_TIMEOUT` / `WITH_EXEC`（`scripts/check_step9.sh` の `with_config`）、`KEEP_KEYSPACE` / `STOP_DB`
  （`scripts/lib_db.sh`）は、いずれも使われており、`scripts/lib/common.sh` にそのまま引き継いだ。

## 追記: 選挙状態の遷移（election.sh#3。ADR 0019）

この再編の後に、選挙状態（scheduled → open → closing → closed。原則17・18）を追加した。対応する旧スクリプトは無い
（新規の機能）。`election.sh#3` として追加し、`core.sh` / `auth.sh` / `chain.sh` / `bench.sh` の既存の確認（起動直後に
投票する項目）は、開始時刻を過去にして起動時に自動で `open` にする（`wait_election_open`。`scripts/lib/common.sh`）。
`chain.sh#6`（tally）は、締切（時刻）による判定を、選挙状態（`closed` かどうか）による判定に置き換えた
（`scripts/election.sh close --now` で `closing` にし、締切の手続き（自動）を待って `closed` にする）。

その後、`election.sh` に次を追加した。

| 項目 | 新 |
|---|---|
| 期間の境界: 開始時刻ちょうどは受け付け、終了時刻ちょうどは拒否する（`domain::vote_gate` と api の統合テスト `the_opening_instant_is_accepted_and_the_closing_instant_is_rejected`。秒ちょうどは実時間の HTTP では狙えないため、時計を固定したテストを実行する） | election.sh#3 |
| `scripts/election.sh` を通した操作: `status` の表示（状態・期間・残り時間・シャードごとの未封印・最後のブロック・監査ログ）、open の間の `schedule` は 409、`close --now` は確認に yes 以外で中止 | election.sh#4 |
| 投票を並列に投げている最中に `close --now --yes` → 締切の手続き（自動）→ `closed`。受理（201）した票の数 = closed 後のチェーン上の票の数（`verifier tally` の検証・突合。未封印 0 件） | election.sh#4 |
| `tally` は `closed` のときだけ（open・closing は終了コード 4、`app.env=test` の `--allow-interim` は 2）、closed の後の投票は 403 | election.sh#4 |

## 追記: 封印ルールの改定（ADR 0020）

原則9 の改定（最小件数・投票開始からの経過時間・締切の手続きだけのフラッシュ）に合わせて、次を変えた。

| 項目 | 新 |
|---|---|
| 封印ルール（dev の設定 interval=10 秒・min=10）: 9 票は 20 秒待ってもブロックができない → 10 票目ですぐに封印（trigger=time）→ 5 票 → `scripts/election.sh close --now` → trigger=close の 5 件のブロック → closed | chain.sh#8（新規） |
| `domain::seal_policy` の必須ケースを、新しいルールの 9 件に入れ替え（時刻は引数。実際には待たない） | core.sh#2 |
| SIGTERM でのフラッシュを前提にした確認（chain#2 の 8、chain#3 の 5、chain#4 の起動 1）を、「SIGTERM ではフラッシュしない」確認に反転 | chain.sh#2・#3・#4 |
| 「1 票でも時間で封印」を前提にした確認（chain#3 の 2、chain#4 の起動 2・3）を、最小件数（10 票）に合わせて変更 | chain.sh#3・#4 |
| リースの引き継ぎ（chain#5）・db_reset（auth#1）は、封印ルールそのものを見る確認ではないので、`seal.min_ballots_after_interval=1` にして、少ない票・端数も時間で封印させる | chain.sh#5・auth.sh#1 |

削除した確認: 「0 件で窓が満了したら窓だけリセット」（`ResetWindowOnly`。ルールが「窓のリセットはしない」に変わった）と、
それを SIGTERM・クラッシュの直前の同期に使っていた「窓をリセットしました」のログ待ち（chain#2・#4）。

## 追記: 白票（ADR 0021）

白票（票の `candidate_id` の予約値 `blank`）と、選挙のルール `vote.allow_blank`（open の時点で固定。原則19）に合わせて、次を追加した。

| 項目 | 新 |
|---|---|
| 白票・open の時点での固定の単体テスト（必須 12 件: 予約値・seed での拒否・`Contest::accepts`・`ElectionRules::effective`・memory ストアでの固定・api の統合テスト 3 件・memory スケジューラでの固定・集計） | core.sh#10 |
| 白票で投票 → close --now の締切の手続きで封印（ブロックに `candidate_id=blank`・`blank=true`）→ `verifier tally` の選挙区ごとの白票の数・合計が投票した数と一致、候補者別の CSV に入らず、表では別の行 | core.sh#10 |
| `vote.allow_blank=false`: 候補者の一覧は `allow_blank=false`、白票の投票は 422 `blank_not_allowed`、拒否した票は数えない | core.sh#10 |
| 設定一式の必須項目に `vote.allow_blank`・`labels.blank_option`・`labels.blank_confirm`・`labels.blank_name`。白票の文言がビルド時に web へ渡る | core.sh#3・#8 |
| 画面: 候補者一覧の最後に白票（`allow_blank=false` なら出さない）・確認画面は `labels.blank_confirm`・予約値で送信・ビューアは白票を別の書式と別の行で（`web::flow` / `web::chain` の必須テスト 6 件と、画面のコードが API の `allow_blank` と設定の文言を使っていること） | web.sh#4 |

画面をブラウザで操作する確認は、ほかの画面と同じく手動（`docs/manual_check_step5.md` の「白票」）。

削除した確認: `verifier` の単体テスト `a_vote_for_someone_outside_the_contest_is_a_blank_ballot`（「投票用紙の候補者ではない票を
白票として数える」。白票の意味が変わり、そのような票は集計を中止するようになった。代わりに
`a_vote_for_someone_outside_the_contest_stops_the_tally`）。

## 追記: 再投票（ADR 0022）

投票期間中の再投票（`vote.allow_revote` / `vote.max_revotes`。slot・seq・supersedes と、締切での `revote_key` の破棄）に合わせて、
次を追加した。

| 項目 | 新 |
|---|---|
| 再投票の単体テスト（必須 12 件: 版 3 の正規化エンコード・slot ごとの最後の票・欠番/重複/上限の検出・slot の HMAC の既知ベクタ・seq とつながり・memory ストアの比較更新・api の統合テスト 3 件（A→B→白票・同時の再投票・締切前の非公開）・締切の手続きでの鍵の破棄・verifier の集計と突合） | core.sh#11 |
| memory モード: A → B → 白票（201）・再投票を明示しない 2 回目は 409 `already_voted`・上限を超えると 409 `revote_limit_reached`・状況に前回の投票内容が出ない・同時の再投票は 201 と 409 `revote_conflict`・`close --now` → 締切後は `secrets/revote_key` が存在しない（`election_audit` に `revote_key_destroyed`）・鍵の値がログに出ない・verify OK・tally は最後の票だけ（白票 1）と `revotes.csv` の A→B・B→白票・`vote.allow_revote=false` では 2 回目が 409 で slot を記録しない | core.sh#11 |
| DB（Cassandra）: A → B → 白票・上限・同時の再投票（participation の LWT）・participation と slot_state の行数と seq・締切前（`reveal_ballots=after_close`）のブロックに candidate / supersedes / slot が無く、`cast` も返さず、verify は 4 → 締切（時刻）→ sealer が鍵を破棄（ファイルが無い・監査ログに 1 行）・鍵の値がログに出ない → verify OK・tally は最後の票だけ・変更の内訳 3 件・ビューアの置き換えのリンク・締切後の再投票は 403 | chain.sh#9（新規） |
| 画面: 完了画面の「投票をやり直す」（全部投票済み・期間内だけ）・固定の順番の一覧・上限の投票用紙は選べず理由を表示・確認画面の `labels.revote_confirm`・見た票の数を添えて送る・409 のコードの区別・ビューアのリンク（`web::flow` / `web::error` / `web::chain` の必須テスト 7 件と、画面のコードが設定の文言と flow の判定を使い、候補者の情報を出さないこと） | web.sh#5（新規） |
| `db_reset.sh --votes` の対象に `slot_state` を追加 | auth.sh#1 |

画面をブラウザで操作する確認は、ほかの画面と同じく手動（`docs/manual_check_step5.md` の「投票のやり直し」）。

変更した確認: chain.sh#6 の改ざん（cqlsh の出力から票の tuple を取り出して書き換える）を、票の tuple の 4 つ目の要素（`revote`。
再投票の無い選挙では `null`）に合わせた。`reconciliation.csv` の列に「うち再投票」を足したので、列番号を合わせた。

## 追記: 確認用のサンプルデータ（ADR 0023）

`scripts/sample_data.sh`（パターン P01〜P13 のサンプルデータと、期待結果つきの CSV）に合わせて、次を追加した。

| 項目 | 新 |
|---|---|
| `app.env=production` では拒否（何も変更しない）・`app.mode=memory` では db モードでの実行を案内して終了・不正な `--phase` は終了コード 2・既定の出力先が `.gitignore` に登録済み | auth.sh#2（新規） |
| `--phase before`（`allow_revote=false`）/ `open`（`allow_revote=true`・`max_revotes=2`）/ `closed`（`allow_revote=true`・`max_revotes=1`・`allow_blank=false`）のそれぞれで実行し、CSV の権限 0600・ヘッダ・14 行・作成不可の行（before は P05〜P09、closed は P07）・要約表（パスワードを出さない）・途中の CSV が残らない・終了後に api が残らない | auth.sh#2 |
| api・sealer を起動し直して、CSV の各行で、実際にログイン → 再投票 → 投票 を試し、HTTP ステータスとエラーの種類が期待結果の列と一致する（投票用紙の数・P03 の合区も確認。CSV がそのままテスト仕様） | auth.sh#2 |
| open: api が動いている間は、DB も CSV も変えずに拒否する。closed: `verifier verify` が OK・`revote_key` は破棄済み | auth.sh#2 |
| 設定 `sample.open_hours`（1〜720）・`sample.output_dir`（空は不可）の既定値と検証 | `cargo test -p app-config`（`sample_data_settings_have_defaults_and_are_validated`） |

変更したスクリプト: `scripts/dev_up.sh` の開発用の秘密情報（固定値と `dev_default_secret`）と再投票の鍵の生成を、`scripts/lib/common.sh` の
`dev_default_secrets` / `ensure_revote_key` に移した（`sample_data.sh` と同じ署名鍵を使わないと、作ったデータを `dev_up.sh cassandra` で
開けないため）。DB 認証での有権者の操作（`db_login` / `token_of` / `ballots_of` / `ballot_field` / `ballot_count` / `cast_vote`）も
`common.sh` に置き、`sample_data.sh` と auth.sh#2 が共用する。
auth.sh#1 のローカルの `login` / `token_of`（同じ処理）は削除し、`common.sh` の `db_login` / `token_of` を使う。

直した不具合: `scripts/dev_up.sh cassandra` が、起動の途中（Cassandra の起動）で `common.sh` を読み直していたため、`dev_up.sh` 自身の
`alive` / `wait_until`（引数の形が `common.sh` の同名の関数と違う）が上書きされ、「起動の完了を待つ」で必ず失敗していた。
`common.sh` は冒頭で読み込み済みなので、読み直しを削除した（サンプルデータを画面で確認する手順が、この起動に依存するため）。

## 追記: common.sh の関数の上書きの検出

`scripts/dev_up.sh cassandra` が、途中で `common.sh` を読み直して、自身の `alive` / `wait_until`（引数の形が `common.sh` の同名の関数と
違う）を置き換えられ、起動待ちで必ず失敗していた（ADR 0023 の作業で発見。初回のコミットから存在した）。構文の検査（`bash -n`）では
見つからず、`dev_up.sh` を実行するスイートも無かったため、次を追加・変更した。

| 項目 | 新 |
|---|---|
| `common.sh` を読み込むスクリプトは、読み込みが 1 回だけで、`common.sh` と同じ名前の関数を定義しない（例外: `fail`。確認スイートの `count` / `pool_total`）。grep による静的な確認で、変更前のコードでは `bench.sh`（`now_ms` / `wait_until`）と `dev_up.sh`（`alive` / `now_ms` / `wait_until`）を検出する | docs.sh#4（新規。構文の確認は #5 に移動） |

変更したスクリプト: `dev_up.sh` の `alive` / `wait_until` を `pid_alive` / `wait_for` に改名し（引数の形が違うため）、使っていない
`now_ms` を削除した。`bench.sh` の `now_ms` / `wait_until` は `common.sh` と引数の形・動作が同じ（待つ間隔だけ 0.5 秒と 0.3 秒）なので削除し、
`common.sh` のものを使う（bench.sh は core.sh#9 が実行する）。

## 再編前後の所要時間

`scripts/check_all.sh` を、直前まで（旧 `check_step0.sh`〜`check_step15.sh`、計 16 本）と、再編後（新 `scripts/check/*.sh`、
6 スイート）のそれぞれで、続けて 2 回ずつ実行して比べた（同じ環境・同じ DB イメージ。共通完了条件の cargo fmt/clippy/test を含む）。

| | 1 回目 | 2 回目 |
|---|---|---|
| 再編前 | 13分38秒 | 13分55秒 |
| 再編後 | 13分27秒 | 13分03秒 |

再編後の方が、2 回とも短い（2 回目どうしの比較で 約 6% 短縮）。差の大半は、上の「スイート内で統合した、重複していたセットアップ」
（`cargo build` の重複除去、DB の前提確認・スキーマの冪等性確認の重複除去、`trunk build --release` の重複除去）による。
これらのスイートの実行時間の大半は、封印ポリシー・アンカーの間隔・リースの TTL などを検証するための、意図した待ち時間
（例: chain.sh の「更新がなければ追加しない」で計 120 秒、複数 sealer のリース TTL や封印待ちなど）が占めており、
これは機能を変えない範囲では削れないため、短縮幅はセットアップの重複除去分にとどまる。
