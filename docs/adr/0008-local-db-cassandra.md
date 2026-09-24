# 0008: ローカル開発の既定 DB を Cassandra にする

## 背景
Step 6（ADR 0007）で、ストアを ScyllaDB 版（infra-scylla）にし、docker-compose と check_step6 を用意した。
しかし開発環境（ChromeOS の Linux、aarch64、仮想アドレス空間（VA）39 ビット）では、ScyllaDB が起動できなかった。
strace で、起動時の `mmap(NULL, 32TiB, PROT_NONE, MAP_NORESERVE)` が `ENOMEM` で失敗し、`abort()` することを確認した
（この環境で確保できる連続領域は 64GiB は成功、512GiB は失敗。ScyllaDB 6.2 と 2026.2.7 のどちらも同じ）。
一方、Apache Cassandra 5.0（JVM）はこの環境で動き、同じ CQL とドライバで検証できた。

## 決定
- **ローカルの既定 DB を Cassandra 5.0 にする**。ScyllaDB は明示的に指定したときだけ使う。
  - `docker-compose.yml` に `cassandra` と `scylla` の 2 サービスを置き、Compose のプロファイル（`--profile cassandra|scylla`）で選ぶ。
    スキーマ投入ジョブも `schema-cassandra` / `schema-scylla` に分ける。従来の上書きファイル（`docker-compose.cassandra.yml`）は廃止した。
  - `scripts/check_step6.sh` の `DB_BACKEND` の既定は `cassandra`。`scylla` は明示したときだけ。
- **Cassandra のヒープ設定は `MAX_HEAP_SIZE=1G` と `HEAP_NEWSIZE=256M` をセットで指定する**（片方だけでは
  cassandra-env.sh が起動を拒否する）。check_step6 が、この 2 つが揃っていることを静的に検査する。
- **ヘルスチェック**は、`cqlsh -e 'SELECT now() FROM system.local'` が成功するまで待つ（Cassandra / ScyllaDB 共通）。
- **`docs/schema.cql` は両方で共通**。Scylla 固有の構文は使っておらず、check_step6 が固有のキーワード
  （`tablets` など）の混入を検査する。
- **DB の起動失敗時**は、`docker logs` の FATAL / ERROR 行をそのまま表示する。原因の推測は出力しない。
- **性能計測は ScyllaDB で、別の環境で行う**。Cassandra での確認は、機能（LWT による二重投票防止、ブロックの書き込み、
  再起動・クラッシュ後の保持・復旧）の確認であり、ScyllaDB の性能や競合時の挙動の指標にはならない。

## 理由
- 開発環境で毎回動かせる DB がないと、Step 6 以降の統合テストとスクリプトを回せない。
- 使う CQL が共通なら、アプリケーションのコード・スキーマ・スクリプトは 1 つで、DB の切り替えは設定だけで済む。
- 本番の想定は引き続き ScyllaDB（CLAUDE.md の技術スタック）。ローカルの既定を変えるだけで、目標は変えない。

## 代替案
- **ScyllaDB のまま、この環境では check_step6 をスキップする**: 開発環境で Step 6 以降を検証できなくなるため不採用。
- **上書きファイル方式（従来）**: 既定が ScyllaDB のままで、Cassandra を使うには毎回 2 つのファイルを重ねる必要があった。不採用。
- **ScyllaDB を別の VM・クラウドで動かして接続する**: 環境の用意が要り、ローカルで完結しない。性能計測の用途では推奨する。

## 留意点
- ScyllaDB では、新しいキースペースの既定（tablets など）や LWT の実装が Cassandra と異なる。共通の CQL でも
  そのまま同じ振る舞いになるとは限らない。ScyllaDB を使える環境で、`DB_BACKEND=scylla scripts/check_step6.sh` を
  実行して確認すること（この開発環境では未確認）。
