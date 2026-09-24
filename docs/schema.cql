-- 投票システムのスキーマ（テンプレート）。ScyllaDB と Cassandra で共通の CQL だけを使う（どちらの schema ジョブも
-- このファイルを流す）。Scylla 固有の構文（tablets の指定など）を入れないこと。
-- （scripts/lib_db.sh の静的検査が、Scylla 固有のキーワードの混入を検査する。）
--
-- キースペース名は `{{KEYSPACE}}` のプレースホルダで、投入するときに実際の名前へ置き換える:
--   - 手動で使う既定のキースペース: `vote`（docker compose run --rm schema-cassandra）
--   - 確認スクリプト（check_step6〜8）: 実行ごとの専用の名前（例: vote_s6_<UNIXTIME>_<PID>）。
--     共用のキースペース（vote）には触れない。
--   - Rust 側は `infra_scylla::schema`（統合テスト・計測ツール）が同じテンプレートを描画する。
-- すべて IF NOT EXISTS なので、何度流しても安全。
--
-- 秘密投票（CLAUDE.md 原則1）のため、participation と ballot_pool は別のテーブルで、
-- 両者を結ぶキーを持たない。どちらのテーブルにも「投票者 + 投票内容」を同じ行に置かない。
--
-- 開発用は単一ノード（RF=1）。本番は NetworkTopologyStrategy で RF>=3 にすること。

CREATE KEYSPACE IF NOT EXISTS {{KEYSPACE}}
  WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};

-- 投票済みの事実だけを持つ。書き込みは LWT（INSERT ... IF NOT EXISTS）で二重投票を防ぐ。
-- ballot_id・候補者・時刻・到着順は持たない。
-- attempt: リクエストごとの乱数。LWT がタイムアウトして結果が不明なときの再試行で、
-- 既存の行が「自分の先の試行が適用されたもの」かを判別するために使う（票とは結び付かない）。
-- seq: この投票用紙に受理した票の数（再投票を認める選挙だけ。初回の投票で 1。認めない選挙では NULL）。
--   再投票は LWT の比較更新（UPDATE ... SET seq = n+1 IF seq = n）で、同時に送られても 1 件だけ成功させる（ADR 0022）。
--   slot・票のハッシュは持たない（それらは slot_state。participation と slot_state を結ぶキーは無い）。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.participation (
  voter_id   text,
  contest_id text,
  attempt    blob,
  seq        int,
  PRIMARY KEY ((voter_id), contest_id)
);

-- 再投票の状態（ADR 0022。再投票を認める選挙だけ使う）。キーは voter_id ではなく、再投票の仮名
-- slot = HMAC-SHA256(revote_key, election_id || voter_id || contest_id)（revote_key は secrets/ にだけ置き、締切の手続きで破棄する）。
-- seq: 最後に受理した票の seq。last_ballot_hash: その票のハッシュ（次の再投票の supersedes）。
-- 書き込み時刻は「分の先頭 + seq」（USING TIMESTAMP）: participation の LWT とマイクロ秒で突き合わせられないように。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.slot_state (
  slot             blob,
  seq              int,
  last_ballot_hash blob,
  PRIMARY KEY (slot)
);

-- 未封印の票。投票者を特定する列を持たない。
-- received_minute: 受理した時刻を「分」に丸めた値（UNIX 分。原則3）。並びは分の古い順、
--   同じ分の中は seq の順、次に ballot_id の順（到着順の粒度は分）。
-- seq: 再投票のつながりの seq（slot の無い票は 0）。同じ slot の次の版は、前の版より後に並ぶので、件数による封印
--   （先頭から max_ballots 件）でも、次の版が前の版より先に封印されない（ADR 0022）。
-- revote: 再投票のつながり（slot・seq・supersedes）の正規化バイト列（domain::encoding::encode_revote。無ければ NULL）。
-- 書き込み時刻（WRITETIME）も分に丸めて指定する（USING TIMESTAMP）。participation の行と
-- マイクロ秒の書き込み時刻で突き合わせられないようにするため。
-- ballot_id: UUIDv4 の 16 バイト。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.ballot_pool (
  shard           int,
  received_minute bigint,
  seq             int,
  ballot_id       blob,
  contest_id      text,
  candidate_id    text,
  revote          blob,
  PRIMARY KEY ((shard), received_minute, seq, ballot_id)
) WITH CLUSTERING ORDER BY (received_minute ASC, seq ASC, ballot_id ASC);

-- 封印済みブロック（公開データ）。書き込みは INSERT ... IF NOT EXISTS。
-- ballots: ballot_id のハッシュ昇順に並んだ票 (ballot_id, contest_id, candidate_id, revote)。
-- contest_id / candidate_id は文字列の ID（例: `2026-general/shugiin_smd.13.01` / `shugiin_smd.13.01.c3`。
-- 形式は domain::ids）。revote は再投票のつながりの正規化バイト列（無ければ空の 0x。cqlsh が tuple の中の NULL の blob を
-- 表示できないため）。ブロックの形式の版 3（ADR 0022）。
-- 再投票に対応する前のスキーマ（3 要素の tuple・ballot_pool に seq が無い）は、ALTER TABLE では直せないので、
-- scripts/db_reset.sh --all で作り直す（api / sealer は接続時に検出して、手順つきで起動を拒否する）。
-- 最新のブロックを LIMIT 1 で引けるよう、height の降順で持つ。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.blocks (
  shard            int,
  height           bigint,
  format_version   int,
  prev_hash        blob,
  merkle_root      blob,
  ballot_count     int,
  sealed_at_minute bigint,
  block_hash       blob,
  signature        blob,
  ballots          list<frozen<tuple<blob, text, text, blob>>>,
  PRIMARY KEY ((shard), height)
) WITH CLUSTERING ORDER BY (height DESC);

-- sealer のリース（分散ロック）。同じ lease_name を同時に持てる owner は 1 つだけ。
-- lease_name: シャードごとの `shard-<n>`、アンカー作成の担当を決める `anchor`。
-- 書き込みは TTL 付きの LWT（取得: INSERT IF NOT EXISTS USING TTL、更新: UPDATE USING TTL ... IF owner = ?）。
-- 保持者が落ちると、TTL が切れて行が消え、別の sealer が取得できる。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.sealer_lease (
  lease_name text,
  owner      text,
  PRIMARY KEY (lease_name)
);

-- アンカー: 全シャードの head をまとめて署名した、チェーン全体のスナップショット（公開データ）。
-- seq は 1 から。`INSERT IF NOT EXISTS` で同じ seq を 2 つ書けないようにする。
-- heads: (shard, height, block_hash) の列（shard の昇順）。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.anchors (
  scope            text,
  seq              bigint,
  anchor_minute    bigint,
  prev_anchor_hash blob,
  heads            list<frozen<tuple<int, bigint, blob>>>,
  anchor_hash      blob,
  signature        blob,
  PRIMARY KEY ((scope), seq)
) WITH CLUSTERING ORDER BY (seq DESC);

-- ブロック署名の公開鍵。最初に登録された鍵だけが有効（INSERT IF NOT EXISTS）。
-- 別の鍵の sealer は起動を拒否される。api はここから公開鍵を読む（秘密の種は持たない）。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.signer_keys (
  key_id     text,
  public_key blob,
  PRIMARY KEY (key_id)
);

-- 有権者の認証情報（auth.mode=db。郵送する ID・パスワードの事前登録。credgen が登録する）。
-- login_id・パスワードのハッシュ（Argon2id の PHC 文字列）・voter_id だけを持つ。voter_id は内部用のランダムな値で、
-- login_id とは無関係。平文のパスワードは、どこにも保存しない。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.credentials (
  login_id      text,
  password_hash text,
  voter_id      text,
  PRIMARY KEY (login_id)
);

-- 有権者名簿（auth.mode=db のとき、api が引く）: 内部の voter_id → 属する選挙区の ID のリスト。
-- 選挙区（どの投票用紙に投票できるか）だけで、投票内容（候補者）は持たない。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.voter_roll (
  voter_id  text,
  districts list<text>,
  PRIMARY KEY (voter_id)
);

-- credgen だけが使う台帳: 名簿の有権者の ID（seed の voters.csv の voter_id）→ 内部の voter_id と、現在の login_id。
-- 同じ有権者に 2 回実行したときの、スキップ／再発行の判定に使う。api は読まない。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.voter_registry (
  external_id text,
  voter_id    text,
  login_id    text,
  PRIMARY KEY (external_id)
);

-- クラスタ共通の設定。shard_count が api と sealer で食い違うと、票が封印されなくなるため、
-- 最初に登録された値と一致しないプロセスは起動を拒否する。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.cluster_config (
  key   text,
  value text,
  PRIMARY KEY (key)
);

-- 選挙状態（scheduled → open → closing → closed。原則17）。単一行（scope 固定）。
-- 初回接続時（スキーマ投入後の init）に、設定の期間を取り込む（INSERT IF NOT EXISTS）。
-- その後は、この行が正（設定ファイルの期間と食い違っていたら、起動時に警告してこちらを使う）。
-- opens_at / closes_at は UNIX 秒（NULL なら無期限・未指定）。opened_at は open に遷移した時刻
-- （封印の経過時間の起点 = 投票開始時刻に使う。原則9。scheduled の間は NULL）。closing_started_at は
-- closing に遷移した時刻（締切の手続きの待ち時間の起点。closing 以外では NULL）。
-- allow_blank / allow_revote / max_revotes は、open に遷移した時点で固定した選挙のルール（白票の可否・再投票の可否と
-- 上限。設定 vote.*。原則19）。open への遷移と同じ LWT で書き、以降は変えない（scheduled の間は NULL）。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.election_state (
  scope              text,
  phase              text,
  opens_at           bigint,
  closes_at          bigint,
  opened_at          bigint,
  closing_started_at bigint,
  allow_blank        boolean,
  allow_revote       boolean,
  max_revotes        int,
  PRIMARY KEY (scope)
);

-- 選挙状態の変更ログ（原則17: 変更のたびに、日時・変更前・変更後・実行した主体を記録する）。
-- id は、同じ秒に複数回変更されても行が競合しないための乱数（票とは無関係）。
-- event: 記録の種類。transition（状態の遷移）/ revote_key_destroyed（締切の手続きの中で revote_key を破棄した。
--   from_phase = to_phase = そのときの状態。ADR 0022）。
CREATE TABLE IF NOT EXISTS {{KEYSPACE}}.election_audit (
  scope        text,
  at_unix_secs bigint,
  id           blob,
  from_phase   text,
  to_phase     text,
  actor        text,
  event        text,
  PRIMARY KEY ((scope), at_unix_secs, id)
) WITH CLUSTERING ORDER BY (at_unix_secs DESC, id DESC);
