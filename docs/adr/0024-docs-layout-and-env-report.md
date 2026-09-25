# 0024: 文書の置き場所（README を入口にした docs/）と、ツールの版の生成（env_report）

## 背景
- README.md が 620 行を超え、起動・DB・認証・DB のリセット・サンプルデータ・デザイン・ビューア・選挙状態・集計・設定・選挙データ・
  白票・再投票・性能計測・検証が 1 つのファイルに並んでいた。同じ内容（封印のルール・設定の優先順・秘密情報の渡し方など）が
  README・CLAUDE.md・`config/default.toml`・スクリプトのコメントに重ねて書かれ、一方だけが直されて食い違っていた
  （例: README と CLAUDE.md の「`voting_opens_at` は未実装」「api は締切後の投票を拒否しない」は ADR 0019 の後も残っていた。
  README が参照する `docs/benchmark.md` は存在しなかった）。
- 手動確認の手順書（`docs/manual_check_step*.md`）は、選挙状態（ADR 0019）の導入後も「api を起動するだけ」の手順のままで、
  そのままでは投票できなかった。存在しない `scripts/check_step*.sh` も参照していた。
- 必要なツールの版（Rust・Trunk）は README に手で書かれ（「1.85 以上」「0.21 系」）、実際に確認に使った版と一致していなかった。
  Rust の版を固定するファイルも無かった。
- `scripts/dev_up.sh` の後に別のシェルで `scripts/election.sh open --now` を実行すると、管理用トークンが見つからずに失敗した
  （dev_up.sh は開発用の固定値を、自分が起動した api にしか渡さない）。

## 決定
- **README.md は、概要・3 分で試す手順（`dev_up.sh` → `election.sh open --now`）・文書の目次だけ**にする。内容は docs/ に 1 か所ずつ置く:
  `environment.md`（想定する環境と版）/ `configuration.md`（設定の全項目）/ `architecture-scaling.md`（構成と拡張）/
  `data-setup.md`（データの置き場所と準備の手順・選挙データの形式）/ `security.md`（優先順・秘密情報・書き換えの決まり・既知の限界）/
  `operations.md`（スクリプトの使い方とよくある失敗）/ `features.md`（画面・白票・再投票・ビューア・API・チェーンの形式）/
  `testing.md`（各スイートが何を確認しているか）。ほかの文書からは、節へのリンクで参照する。
- `config/default.toml` のコメントは、設定のスキーマとして各項目に残す（`core.sh#3` が必須にしている）。利用者向けの説明・既定値・
  **変更できるタイミング**（いつでも / open で固定 / init で固定 / init で取り込み / 期間中は変えない / ビルド時 / 実行時）は
  `docs/configuration.md` に書き、`docs.sh#7` が default.toml の全項目が書かれていることを確認する。
- **ツールの版は手で書かない。** Rust は `rust-toolchain.toml`（channel・rustfmt・clippy・wasm32 ターゲット）で、Trunk は
  `crates/web/Trunk.toml` の `trunk-version` で固定し（違う版の trunk は起動を拒否する。`dev_up.sh` が生成する設定にも引き継ぐ）、
  `scripts/env_report.sh` がそれらと `Cargo.lock`・`docker-compose.yml`・各ツールの `--version` から `docs/environment.md` の区間を作る。
  `docs.sh#6` が `env_report.sh --check`（再生成して差分が無いこと）を実行する。Docker や OS の版は機械ごとに違うので区間に入れず、
  `--host` で表示するだけにする。
- Rust は、この時点の最新安定版 1.98.1（2026-09-01）に固定する（`cargo clippy --workspace -- -D warnings` が警告なしで通ることを確認した）。
- **スクリプトは `--help` で先頭のコメントを表示し、何も起動・変更せずに終わる。** `scripts/lib/common.sh` も、直接実行したときは
  同じように表示する。`docs.sh#8` が、README・CLAUDE.md・`docs/*.md` に出てくるスクリプトが実在し、`--help` が終了コード 0 で
  何かを表示することを確認する。
- `scripts/election.sh` は、`app.env=dev` のとき、`dev_up.sh`・`sample_data.sh` と同じ開発用の固定値（`dev_default_secrets`。
  環境変数でも `secrets/` でも指定していないときだけ）を使う。
- 使われていない古いデモ `scripts/requests.sh`・`scripts/requests.http`（既定の ID が名簿に無く、選挙も open にしないので、投票は
  すべて拒否された）と、誤って git 管理されていた `.dev/trunk.toml`（`dev_up.sh` の生成物。個人の絶対パスを含む）を削除し、
  `/.dev/` を `.gitignore` に足す。
- `docs/testing.md` の、旧 `scripts/check_step*.sh` からの移行の対応表と経緯は削除し、今のスイートの節ごとの説明に置き換える
  （旧スクリプトはもう無く、経緯は git の履歴に残っている）。

## 理由
- 同じ内容を 1 か所にだけ書けば、仕様を変えたときに直す場所が 1 つになり、食い違いが起きない。README を短くすると、初めての人が
  「まず動かす」までの距離が短くなる。
- 文書とコードの一致は、人の注意ではなく `docs.sh` で機械的に確かめる（設定項目の網羅・版・スクリプトの実在と `--help`）。
  今回も、手順書が存在しないスクリプトを参照していたことを `docs.sh#8` が最初の実行で見つけた。
- 版を固定しないと、clippy の新しい lint で CI だけが落ちる・手元とで結果が違う、ということが起きる。固定した版をファイルに置けば、
  rustup と trunk 自身がその版を強制するので、「手順どおりに入れれば同じ出力」が成り立ち、文書の版の表を差分で検査できる。

## 代替案
- **README を章立てで整理して 1 ファイルのまま残す**: 目次は作れるが、長さと重複の問題は残る。却下。
- **文書の版の表を手で書き、`docs.sh` では「最低限の版以上か」だけを見る**: 版の比較の規則をスクリプトに持つことになり、
  文書と実際の版がずれても検出できない。却下。
- **Docker の版も区間に入れる**: 機械ごとに違うので、どの機械でも一致する区間にならない。`--host` の表示に分けた。
- **`default.toml` のコメントを削って、説明を `configuration.md` だけにする**: `core.sh#3` が各項目のコメントを必須にしており、
  ファイルを開いた人がその場で意味を知れる利点もある。コメントは短い要点として残し、正の説明は `configuration.md` とした。
- **`election.sh` がトークンを見つけられないときは、`dev_up.sh` が書き出したファイルを読む**: 秘密情報のファイルを増やすことになる。
  固定値は `common.sh` に 1 か所だけあるので、それを同じ規則（dev のときだけ・指定が無いときだけ）で使う方が単純。
