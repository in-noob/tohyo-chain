# 0007: ScyllaDB 版のストア（infra-scylla）と docker-compose

## 背景
Step 6 で、api の保存先をメモリから ScyllaDB に切り替えられるようにする（`STORAGE=scylla`）。
メモリ版では再起動で投票状況もチェーンも消え、複数プロセス間で共有できなかった。
秘密投票（原則1・3）と二重投票防止を、分散 DB の上でも守る必要がある。sealer はまだ api プロセス内で動く。

## 決定
- **スキーマ**（`docs/schema.cql`。`IF NOT EXISTS` のみで冪等）:
  - `participation ((voter_id), contest_id)`: 投票済みの事実だけ（列は `attempt` のみ）。
  - `ballot_pool ((shard), received_minute, ballot_id)`: `contest_id` / `candidate_id`。投票者を特定する列なし。
  - `blocks ((shard), height DESC)`: ヘッダ各項目・`block_hash`・`signature`・`ballots`（`list<frozen<tuple<blob,int,int>>>`）。
- **投票**: participation を LWT（`INSERT ... IF NOT EXISTS`）で先に記録し、負ければ `AlreadyVoted`。
  その後に票をプールへ INSERT し、失敗したら participation を `IF attempt = ?` の LWT で取り消す（補償）。
  票を先に入れると、途中で落ちたときに二重投票になり得るため、participation を先にする。
- **`attempt`**: リクエストごとの乱数（`ballot_id` とは無関係）。LWT がタイムアウトして結果が不明なとき、
  再試行が「適用されなかった」で返っても、既存の行の `attempt` が自分のものなら先の試行が適用済みだったと判別できる。
  これがないと、票が失われるか二重に入る。
- **書き込み時刻の秘匿**: Scylla/Cassandra は列ごとに書き込み時刻（`WRITETIME`、マイクロ秒）を持ち、
  participation の行と票の行を時刻で突き合わせられる。プールの INSERT は `USING TIMESTAMP` に**分に丸めた値**を
  指定する（LWT には `USING TIMESTAMP` を付けられないので、participation 側は丸められない）。
  票の側が分に丸まっていれば、participation の正確な時刻から特定できるのは「その分に入った票の集合」までになる。
- **プールの削除にも書き込み時刻を明示**: DELETE の書き込み時刻が INSERT より小さいと、削除は黙って無効になる
  （票がプールに残り、次のブロックにも入る）。INSERT した api と削除する sealer の時計がずれると起き得るので、
  削除は行の `received_minute` から決めた「INSERT の書き込み時刻 + 1µs」を使う。実 DB のテストで見つかった。
- **プールの並び**: `received_minute`（分）の古い順、同じ分の中は `ballot_id` の順。到着順の粒度は「分」になる。
  原則3（時刻は分単位）の帰結であり、同じ分の中で到着順が推測できないのは秘密投票の面でも望ましい。
  ブロック内の票は従来どおり `seal_block` が `ballot_id` のハッシュ順に並べる。
- **ブロックの書き込み**（`SealStore::commit`）: (1) 連続性（高さ・`prev_hash`）とプールの票の存在を検証、
  (2) `INSERT ... IF NOT EXISTS`（LWT）でブロックを追加、(3) プールから票を削除（同一パーティションのバッチ）。
  (2) で負けても、既存の `block_hash` が自分のものなら再試行とみなして成功にする。
- **復旧**: `SealStore::recover(shard)`（既定は何もしない）を追加し、sealer の `init` が起動時に呼ぶ。
  (2) と (3) の間で落ちていれば、先頭ブロックの票がプールに残っているので取り除く（冪等）。
  `init` は、ジェネシス作成で別のプロセスに競り負けた（`Conflict` でチェーンがある）場合も成功として扱う。
- **一貫性**: 通常の読み書きは LOCAL_QUORUM、LWT は SERIAL。participation / blocks の照会は SERIAL で行う。
  開発用は SimpleStrategy・RF=1（本番は NetworkTopologyStrategy・RF>=3）。
- **`STORAGE=scylla` では `SEALER_SIGNING_SEED` を必須**にする。チェーンが再起動をまたいで残るので、
  署名鍵が変わると既存のブロックと新しいブロックを同じ公開鍵で検証できなくなる。
- **`/debug/tamper` は memory 専用**（scylla では 501）。

## docker-compose

> 注: ローカルの既定 DB と compose の構成は [ADR 0008](0008-local-db-cassandra.md) で変更した（既定は Cassandra、プロファイルで選ぶ）。以下は Step 6 時点の記録。

- `docker-compose.yml`: ScyllaDB 1 ノード（開発用。`--smp 1 --memory 1G --overprovisioned 1 --developer-mode 1`）と、
  `docs/schema.cql` を流す `schema` ジョブ（healthy になってから 1 回実行して終了）。
- `docker-compose.cassandra.yml`（重ねて使うオーバーライド）: ScyllaDB が起動できない環境向けの代替。
  **ScyllaDB は起動時に 32TiB の仮想アドレス空間を予約する**ため、仮想アドレス空間が狭い環境（39 ビットの
  カーネルの VM など）では SIGABRT で起動できない（バージョンを変えても同じ）。今回の開発環境がこれに当たり、
  ScyllaDB 自体では動作確認できていない。同じ CQL（LWT・`USING TIMESTAMP`・tuple・クラスタリング順）が
  使える Apache Cassandra 5.0 で、api・infra-scylla・スクリプトを検証した。

## 理由
- LWT は「投票済みか」の判定と記録を分散環境でも原子的に行える。participation を先に書き、負けた側は何も残さない。
- 書き込み時刻まで含めて分に丸めないと、テーブルを分けても時刻で突き合わせられる。
- 復旧と冪等な再試行があれば、Cassandra 系の DB にない複数テーブル間のトランザクションなしで、途中で落ちても
  票の二重封印・消失を避けられる。

## 代替案
- **票を先に書き、participation を後にする**: 途中で落ちると二重投票の余地が残るため不採用。
- **ブロックの票を別テーブル（`block_ballots`）に持つ**: 読み出しが増えるだけで利点が少ない。1 ブロック（最大 100 票、
  約 2.4KB）は 1 行に収まるので不採用。
- **プールの並びに timeuuid を使う**: 到着時刻がマイクロ秒精度で残り、原則3に反するため不採用。
- **`voted` フラグを ballot_pool 側に持たせる**: 2 つのテーブルを結ぶキーになるため不採用。

## 既知の制限
- 補償（participation の取り消し）にも失敗すると「投票済みだが票がない」状態が残り得る。秘密投票のため
  participation と票を突き合わせて復旧することはできない（ログに ERROR を残す）。
- `voter_id` は平文で保存している（認証はスコープ外のスタブ）。実運用では匿名化した ID にする。
- sealer は api プロセス内なので、scylla モードでも api は 1 インスタンスで動かす前提。複数だと sealer が競合する
  （LWT が破損は防ぐが、`Conflict` のログが出る）。sealer のプロセス分離は別のステップで行う。
- 今回の検証は Cassandra 5.0 で行った。ScyllaDB では LWT の実装（Paxos）が異なるため、性能・競合時のタイムアウトの
  出方は異なり得る。ScyllaDB が起動できる環境で `scripts/check_step6.sh` を実行して確認すること。
