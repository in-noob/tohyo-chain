# 確認スイート（scripts/check/）

確認は、機能ごとの 6 つのスイートにまとめている（原則10）。各スイートの完了条件はスクリプト自身にあり、**exit 0 で終わることが完了の定義**。
`#番号` はスイートの中の節で、スクリプトの先頭のコメントと、実行時の `== 番号.` の見出しに対応する。

```
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
scripts/check_all.sh            # 上の 3 つと、scripts/check/*.sh の全スイートを順に実行し、結果と所要時間の表を出す
scripts/check/chain.sh          # 1 つのスイートだけ
```

| スイート | 目安の時間 | 必要なもの |
|---|---|---|
| `core.sh` | 約 5 分 | Docker |
| `chain.sh` | 約 10 分 | Docker |
| `election.sh` | 約 1.5 分 | — |
| `auth.sh` | 約 4 分 | Docker |
| `web.sh` | 数十秒（初回は wasm のビルドで数分） | Trunk・wasm ターゲット |
| `docs.sh` | 数秒 | [environment.md](environment.md) の手順どおりの Rust と Trunk |

## 共通の仕組み

- **共通処理**は [scripts/lib/common.sh](../scripts/lib/common.sh)（設定・DB・専用キースペース・選挙データ・起動と停止・待ち合わせ・
  アサーション）。各スイートは 1 回だけ読み込み、同じ名前の関数を定義しない（docs.sh#4）。関数の一覧は `scripts/lib/common.sh --help`。
- **設定の分離**: 各スイートは、手元の `config/local.toml`・`secrets/`・環境変数 `APP__…` の影響を受けないよう、設定ファイルを読まない
  `app.env=test` で動き、スイートが渡す `APP__…` だけを使う（`APP__DB__BACKEND` だけは引き継ぐ）。
- **DB の分離**: DB を使う節は、実行ごとに専用のキースペース（`vote_chain4_<UNIXTIME>_<PID>` など）を作り、終了時（成功でも失敗でも）に
  `DROP` する（[ADR 0010](adr/0010-test-isolation-per-run-keyspace.md)）。手動で使うキースペース `vote` には触れない。
  `KEEP_KEYSPACE=1` なら残して名前を表示する（失敗の調査用）。DB のコンテナは、起動していなければ起動するだけで、止めない（`STOP_DB=1` のときだけ止める）。
  DB の起動に失敗したときは、コンテナのログの FATAL / ERROR 行をそのまま表示する。使う DB は `db.backend`（ScyllaDB は `APP__DB__BACKEND=scylla`）。
- 各節は `名前() ( ... )` のサブシェルで動くので、`trap` や変数が節の間で干渉しない。
- infra-scylla の統合テストは `#[ignore]`。DB の起動後に `cargo test -p infra-scylla -- --ignored`（chain.sh#4 が実行する）。
- 画面をブラウザで操作する確認は手動: [manual_check_step5.md](manual_check_step5.md)（投票画面・白票・投票のやり直し）・
  [manual_check_step14.md](manual_check_step14.md)（ビューア）・[manual_check_step15.md](manual_check_step15.md)（テーマ）。

## core.sh — 起動・設定・封印ルールの単体テスト・性能計測ツール・白票・再投票（memory）

| 節 | 確認すること |
|---|---|
| #1 | api が起動し、`GET /healthz` が 200 で `{"status":"ok"}`、リクエストログが出る |
| #2 | `domain::seal_policy` の単体テスト（原則9 の必須ケース 9 件。時刻は引数で、実際には待たない） |
| #3 | 設定ファイル一式の整合: default / dev / production.example の必須項目、default.toml の全項目に日本語のコメント、`.gitignore`、`Trunk.toml` と `web.port` / `api.port` の一致 |
| #4 | 設定ファイルの値を書き換えると動作が変わる（`seal.max_ballots` など・環境別のファイル） |
| #5 | 環境変数が設定ファイルより優先される。`app-config show` が値の出所を表示する |
| #6 | 秘密情報は `secrets/`・環境変数から読める。設定ファイルには書けない。`show` / `get` / エラー / ログに出ない |
| #7 | 不正な設定では、api / sealer / verifier / bench が「どのファイルのどの項目がなぜ不正か」を表示して起動に失敗する |
| #8 | `labels.*` がビルド時に web へ渡る。web は app-config に依存しない |
| #9 | 性能計測ツール一式（`crates/bench` の単体テストと、`scripts/bench.sh` の短縮した設定でのスモーク計測・出力の内容） |
| #10 | 白票: 白票で投票 → 締切の手続きで封印（ブロックに `candidate_id=blank`）→ `tally` の白票の数が一致し、候補者別には入らない。`vote.allow_blank=false` では 422 `blank_not_allowed`。open での固定の単体テスト |
| #11 | 再投票: A → B → 白票（201）・`revote` なしの 2 回目は 409 `already_voted`・上限で 409 `revote_limit_reached`・同時の再投票は 1 件だけ成功・前回の内容が出ない・締切後に `secrets/revote_key` が消え `revote_key_destroyed` が記録される・鍵の値がログに出ない・`tally` は最後の票だけ・`vote.allow_revote=false` では 409 で slot を記録しない。必須の単体テスト 12 件 |

## chain.sh — ハッシュチェーン（封印・永続化・複数 sealer・集計・ビューア・再投票（DB））

| 節 | 確認すること |
|---|---|
| #1 | `verifier demo`: 正常なチェーンの検証と、改ざんの検出（Merkle 根の不一致・包含証明の失敗）。未知のサブコマンドは終了コード 2 |
| #2 | memory の sealer: 250 票 → 件数で 100・100、時間で 50 のブロック・以後増えない・`verify` OK・`/debug/tamper` の後は NG。最小件数未満の 9 票は SIGTERM でフラッシュしない。ログに投票者 ID が出ない |
| #3 | 更新が無ければ追加しない: 0 票で待ってもブロック・アンカーが増えない（`skip` のログ）→ 10 票でブロックとアンカーが 1 つずつ増え、以後は増えない。変化なしの SIGTERM は最終アンカーを作らない |
| #4 | DB の永続化と復旧: スキーマの投入（2 回流して冪等）・infra-scylla の統合テスト・並列 100 リクエストで 1 件だけ成功（LWT）・再起動をまたいで投票状態・未封印・チェーンを保持・SIGKILL（クラッシュ）→ リース切れの後に引き継いで封印・`verify` OK |
| #5 | 複数 sealer: 2 プロセスで 4 シャードのリースを排他的に取得・1000 票・片方を kill -9 → もう片方が引き継ぐ・分岐と欠番が無い・アンカーが全シャードを含み変化が無ければ増えない・`verify` OK・正常停止でリースを解放 |
| #6 | `verifier tally`: 未封印が残っていれば中止・`close --now` → `closed` の後の集計が期待値と一致（候補者別・選挙区別・都道府県別・種類別・順位・白票・突合）・`closed` 前は `--allow-interim` が無ければ拒否・改ざんしたブロックは集計を拒否（終了コード 3） |
| #7 | ビューアの API: ページ送り（新しい順・欠落なし・`limit`・`before_height` の境界）・Cache-Control（`no-cache` / `immutable` / `no-store`）・`reveal_ballots=after_close` の締切前は票が含まれず verifier は終了コード 4・締切後は票と表示名が含まれる・404 |
| #8 | 封印ルール（dev の設定 interval=10 秒・min=10）: 9 票は 20 秒待っても封印されない → 10 票目で封印（`trigger=time`）→ 5 票 → `close --now` → `trigger=close` の 5 件のブロック → `closed` |
| #9 | 再投票（DB）: A → B → 白票・上限・同時の再投票（participation の LWT）・`participation` と `slot_state` の行数と seq・締切前は candidate / supersedes / slot が見えず `cast` も返さない・締切で sealer が鍵を破棄・`verify` OK・`tally` は最後の票だけ・変更の内訳・ビューアの置き換えのリンク・締切後の再投票は 403 |

## election.sh — 投票フローと選挙データ・選挙状態

| 節 | 確認すること |
|---|---|
| #1 | 投票フロー（`seed/2026-general`）: dev-tools なしで `/debug/pool` が 404・ログイン・状態（表示順・固定）・候補者・投票 201・2 回目は 409・対象外は 403・名簿に無い有権者は空・並列 100 リクエストで 1 件だけ成功・ログに投票者 ID とマイナンバーが出ない |
| #2 | 47 都道府県規模の選挙データ（`seedgen`。候補者 1 万人以上）: 生成と読み込み・合区と比例ブロック・有権者ごとに関係する投票用紙だけが表示順で見える・投票順の固定・壊れたデータ（重複 ID・存在しない参照・不正な ID・長すぎる ID・列不足など）がファイル・行・原因つきで検出される |
| #3 | 選挙状態の遷移と受付期間（原則17・18）: schedule → 自動で open → 自動で closing → closed・開始時刻ちょうどは受け付け終了時刻ちょうどは拒否（時計を固定した単体テスト）・開始前 / 期間内 / closing / closed の受付・closing 直前の票が最終ブロックまでに封印される・公開用のポートと管理用リスナーが分かれている |
| #4 | `scripts/election.sh` を通した操作: `status` の表示・open の間の `schedule` は 409・`close --now` は yes 以外で中止・並列に投票している最中の `close --now --yes` → 受理した票がすべて封印される（`tally` の検証・突合）・`tally` は `closed` のときだけ・`closed` の後の投票は 403 |

## auth.sh — ID とパスワードの事前登録・DB 認証・DB のリセット・サンプルデータ

| 節 | 確認すること |
|---|---|
| #1 | credgen で 100 人を登録（CSV の件数・列・権限 0600・文字種。DB にはハッシュだけ）・正しい ID とパスワードでログインして投票・パスワード違い / 存在しない ID / 形式不正が同じ応答と同程度の時間で失敗・`output_file_enabled=false` の警告・2 回目はスキップ・`--reissue` で再発行・`db_reset.sh --votes`（動作中は拒否・確認なしでは消えない・投票データだけ）・`--all`・production では拒否・memory では案内だけ |
| #2 | `scripts/sample_data.sh`: production では拒否・memory では案内・不正な `--phase` は 2・`before` / `open` / `closed` のそれぞれで CSV（0600・ヘッダ・14 行・作成不可の行）と要約表（パスワードを出さない）を確認し、CSV の各行で実際にログイン → 再投票 → 投票 を試して期待結果と一致する・`open` は api の動作中に拒否・`closed` は `verify` OK で鍵が破棄済み |

## web.sh — 画面

| 節 | 確認すること |
|---|---|
| #1 | 画面遷移ロジック（`web::flow`）の単体テスト（必須ケースを含む）・折り返しが無いこと・flow / error が UI とブラウザ API に依存しない（原則5）・wasm 向けの clippy |
| #2 | デザイントークン: 色がトークン以外（CSS のルール・Rust・`index.html`）に直接書かれていない・テーマのテスト（保存値の解釈・OS 設定への追従・インラインスクリプトの規則・トークンの構造・コントラスト比）・選択状態を色だけで表さない部品がある |
| #3 | `trunk build --release` が成功し、`dist/` のサイズ・インラインスクリプトの位置・CSS のトークンが保たれる（1 回だけビルドして両方を確認する） |
| #4 | 白票の画面: 候補者一覧の最後に白票（`allow_blank=false` なら出さない）・確認画面の `labels.blank_confirm`・予約値で送信・ビューアの別の行 |
| #5 | 投票のやり直しの画面: 完了画面のボタン（許可・期間内だけ）・固定の順番の一覧・上限の投票用紙は選べず理由を表示・確認画面の `labels.revote_confirm`・見た票の数を添えて送る・409 の区別・ビューアの置き換えのリンク・候補者の情報を出さない |

## docs.sh — 呼び名・名前・文書とコードの一貫性

| 節 | 確認すること |
|---|---|
| #1 | 旧来の呼び名（contest を片仮名にした語）が、コード・設定・選挙データ・スクリプト・README・CLAUDE.md・`docs/*.md` のどこにも無い（原則12） |
| #2 | 旧来の環境変数名（`APP__` の付かない `SESSION_SECRET` など）が残っていない・app-config 以外のコードが環境変数を直接読んでいない・CLAUDE.md に原則11 がある |
| #3 | 封印ルールの旧名（ADR 0020 の前の設定名・判定・ログ）が、コード・設定・スクリプト・README・CLAUDE.md・`docs/*.md` に残っていない |
| #4 | `scripts/lib/common.sh` を読み込むスクリプトが、読み込みは 1 回だけで、common.sh と同じ名前の関数を定義しない（許可した上書き `fail`・`count`・`pool_total` を除く） |
| #5 | すべてのシェルスクリプトの構文（`bash -n`） |
| #6 | `scripts/env_report.sh --check`: [environment.md](environment.md) の版の区間が、手順どおりに再生成した内容と一致する |
| #7 | `config/default.toml` のすべての項目（`セクション.項目`）が [configuration.md](configuration.md) に書かれている |
| #8 | README.md・CLAUDE.md・`docs/*.md` に出てくるスクリプト名（`scripts/….sh`）がすべて実在し、`--help` が終了コード 0 で使い方を表示する（何も起動・変更しない） |
