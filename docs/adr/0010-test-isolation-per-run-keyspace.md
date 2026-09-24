# 0010: 確認スクリプトの分離 — 実行ごとの専用キースペースと完全修飾テーブル名

## 背景
`check_step6.sh` 〜 `check_step8.sh` は、どれも共用のキースペース `vote` を使い、開始時に DB を `down -v` していた。
そのため次の問題があった。

- Step 7 / 8（負荷計測を含む）のデータが `vote` に残り、次の `check_all.sh` の Step 6 が
  「起動直後はジェネシス（高さ 0）のはず」で失敗する。
- 確認スクリプトが、開発者が手動で `vote` に入れたデータを消す（`down -v`）。
- ドライバの `use_keyspace`（`USE <keyspace>`）を使っていたため、Cassandra が
  「USE <keyspace> with prepared statements」の警告を出す（テーブル名が曖昧になる）。

## 決定
- **実行ごとの専用キースペース**: `scripts/lib_db.sh` の `ks_init` が `vote_<tag>_<UNIXTIME>_<PID>`
  （例: `vote_s6_1789912345_123456`）を決め、`SCYLLA_KEYSPACE`（api / sealer / verifier）と
  `TEST_KEYSPACE_PREFIX` / `BENCH_KEYSPACE_PREFIX`（統合テスト・計測ツール）で渡す。
  名前は「先頭が英字、英数字と `_` のみ、48 文字以内」（`infra_scylla::env::parse_keyspace` が検査）。
- **スキーマはテンプレート**: `docs/schema.cql` はキースペース名を `{{KEYSPACE}}` にし、投入時に置換する
  （compose の schema ジョブは `SCHEMA_KEYSPACE`（既定 `vote`）で `sed`、Rust は `infra_scylla::schema::statements`）。
  固定の `vote` を書かないことは、単体テストと `db_static_checks` で確認する。
- **後始末**: スクリプトの `trap` が、成功でも失敗でも、専用キースペース（と、その接頭辞で始まる統合テスト用のもの）を
  `DROP KEYSPACE` する。`KEEP_KEYSPACE=1` のときは残して名前を表示する（失敗時の調査用）。
- **共用 DB には触れない**: 確認スクリプトは DB を「起動していなければ起動する」だけで、停止（`STOP_DB=1` のときだけ）も
  ボリュームの破棄もしない。既定のキースペース `vote` は一切読み書きしない。
- **性能計測は別プロジェクト**: `bench.sh`（と `check_step8.sh`）は、Compose プロジェクト `vote-bench`・ポート 19042 で
  DB を起動し、構成ごとに `down -v` でクリーンにする。共用の `vote-prototype`（ポート 9042）とは共有しない。
  `db_fresh` は `vote-prototype` では実行を拒否する。
- **完全修飾テーブル名**: すべての CQL を `<keyspace>.<table>` で書く（キースペース名は設定値から組み立てる）。
  接続時に `USE` しない。代わりに、接続時に `system_schema.keyspaces` でキースペースの存在を確認し、
  なければ分かりやすいエラーで終了する。

## 理由
- 実行ごとに名前を分ければ、前回の残りや並行して動く別の実行と干渉しない（原則として、同じ DB を使っても状態が共有されない）。
- キースペースの作成・削除は、コンテナの起動・破棄より桁違いに速く、Step ごとに DB を作り直す必要がない。
- 完全修飾名は、プリペアドステートメントがどのキースペースのテーブルを指すかを曖昧にしない
  （Cassandra が警告する理由そのもの）。

## 代替案
- **Step ごとに `down -v`**（従来）: 手動データを消し、他のプロセスの DB を巻き込む。却下。
- **テーブルの中身を `TRUNCATE`**: 共用の `vote` を触る。並行実行にも弱い。却下。
- **`USE` を残して警告を無視**: 警告が常時出て、本当の異常が埋もれる。却下。
