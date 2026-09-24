# 0011: 設定を config/ の TOML に集約し、app-config クレートで読み込み・検証する

## 背景
設定が、api・sealer・infra-scylla・bench の各クレートの環境変数読み取りと、スクリプトの `export` に散らばっていた。
同じ意味の値（`SHARD_COUNT`、`SEAL_MAX_*`、`SCYLLA_*` など）を、api と sealer が別々に読み、既定値・検証・エラー文言が
重複していた。エラーは「環境変数 X が不正」とだけ言い、どこに書いた値かが分からなかった。秘密情報の扱いも、
環境変数に限られていた。今後、認証（DB・Argon2）・投票期間・票の公開制御・画面の文言など、設定項目が増える。

## 決定
- **`config/` 配下の TOML に集約する**（CLAUDE.md の原則 11）:
  `default.toml`（全項目・既定値・日本語のコメント。項目の型の定義も兼ねる）、`dev.toml`、`production.example.toml`、
  手元用の `local.toml`（gitignore）。
- **読み込みと検証は `crates/app-config`**（ライブラリ + 確認用 CLI）。api / sealer / verifier / bench / スクリプトは
  すべてここから読む（環境変数の直接参照をしない。`check_step9.sh` が確認する）。
- **層と優先順位（後勝ち）**: `default.toml`（バイナリに埋め込む）→ `config/<app.env>.toml`（production は必須）→
  `config/local.toml` → `secrets/`（1 ファイル 1 値、gitignore）→ 環境変数 `APP__<セクション>__<項目>`。
  環境変数の値は、`default.toml` の値の型で解釈する（整数・真偽値・文字列・カンマ区切りの配列）。
- **項目ごとに出所を記録する**。不正な値は、未知の項目（タイプミス）・型違い・範囲外・構文エラーを含めて、
  「どのファイル（環境変数）の、どの項目が、なぜ不正か」を、すべてまとめて報告して終了する。
- **秘密情報**（`session.secret`、`sealer.signing_seed`）は `Secret<T>` に包み、`Debug` / `show` では `***`。
  設定ファイルに書くとエラー（値はメッセージに含めない）。環境変数か `secrets/` から読む。`get` でも取り出せない。
- **未実装の機能に対応する項目**（初版では `auth.mode=db`、`credentials.output_file_enabled`、投票期間、
  `chain.reveal_ballots=after_close`。のちに DB 認証・credgen の出力・`voting_closes_at`・`after_close` は実装済み。
  `voting_opens_at` も [ADR 0019](0019-election-lifecycle.md) で実装済みになり、`ensure_supported` の
  「未実装」判定は今は無い）は、項目と検証だけを先に用意し、指定されたら `Loaded::ensure_supported` が
  「この版では未実装」で起動を失敗させる（黙って無視しない）。
- **web は app-config に依存しない**（原則 5）。ビルド時に必要な値（`labels.*`）だけを、`app-config web-env` が出力する
  環境変数（`APP_WEB_SITE_TITLE` など）で受け取り、`option_env!` で読む。`trunk serve` のポートと proxy 先は、
  `dev_up.sh` が設定から Trunk 用の設定ファイル（`.dev/trunk.toml`）を生成して渡す（`--proxy-backend` は `Trunk.toml` の proxy を
  上書きせず追加するため、同じ経路が二重に登録されて落ちる）。`crates/web/Trunk.toml`（手で実行するときの既定）と
  `default.toml` の `web.port` / `api.port` の一致は、`check_step9.sh` が確認する。
- **確認スクリプトは、設定を分離する**（`scripts/lib_cfg.sh` の `cfg_init`）: 設定ファイルの場所を存在しない
  ディレクトリに向け、`APP__…` を消し、`app.env=test` にする。手元の設定に結果が左右されない。
- **旧来の環境変数名（`SESSION_SECRET`、`SEAL_MAX_BALLOTS`、`STORAGE`、`SCYLLA_*`、`API_PORT` など）は廃止**する。
  過去の ADR の環境変数名は、README の対応表で読み替える。`api.port` の既定は 8080 から 18080 に変えた。
- `infra-scylla` は、接続時にキースペース名を検査する（設定でも検査するが、CQL の識別子に埋め込むので防御として残す）。
- **依存の追加**: `toml`（1.1.6。追加時点の最新の安定版）。TOML の解析だけに使い、`serde` の派生には使わない
  （項目ごとの出所と、全件まとめたエラーを出すため、値は `toml::Value` から自前で読む）。

## 理由
- 設定の意味・既定値・検証を 1 か所に置けば、api と sealer で食い違わない。`default.toml` を見れば全項目が分かる。
- 出所つきのエラーは、環境変数・ローカル上書き・環境別ファイルが重なる構成で、原因の特定を早くする。
- 秘密情報を型と読み込み経路で分けておけば、ログ・`show`・エラーへの漏洩を、構造として防げる。
- 未実装の項目を黙って受け付けると、設定と実際の動作が食い違う（たとえば締切を指定したのに、無視される）。

## 代替案
- **`serde` + `config` クレート**: 出所つきのエラーと、全件まとめた報告を、そのままでは出せない。項目数（約 30）なら、
  自前の読み取りのほうが単純で、テストしやすい。
- **`default.toml` を実行時に読む**: ファイルとバイナリの二重の定義になり、どちらが有効か分からなくなる。
  埋め込めば、ディレクトリに依存せずに動く（テストの作業ディレクトリがクレートごとに違うことも解決する）。
- **環境変数の旧名を別名として残す**: 「集約」にならず、優先順位が複雑になる。却下。
- **未実装の項目を今回は作らない**: 後から追加すると、設定の互換性と検証を作り直す。項目だけ先に定義した。
