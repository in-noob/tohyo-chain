# 0005: sealer・公開チェーン API・票フォーマットへの contest_id 追加

> **改訂**（[ADR 0020](0020-seal-policy-min-ballots.md)）: 停止時（SIGTERM）のフラッシュ（`decide_flush`・`trigger=flush`）は廃止した。
> 残りの票は、締切の手続き（選挙状態 closing）の中でだけ封印する（`trigger=close`）。封印の経過時間は壁時計で測る。

## 背景
Step 4 で、未封印の票をブロックに封印する sealer と、封印済みチェーンを公開して検証する仕組みを実装する。
封印済みブロックは公開・不変のデータなので、フォーマットは今のうちに固める必要がある。
従来の `Ballot`（`ballot_id` + `candidate_id`）は `contest_id` を持たず、シャード単位のチェーンから
コンテストごとの集計ができなかった。

## 決定
- **票に `contest_id` を追加**（ADR 0002 の票エンコードの更新）:
  `ballot_id(16) ‖ contest_id(4) ‖ candidate_id(4)` の 24 バイト（ビッグエンディアン、固定長）。
  ヘッダのエンコードとブロックハッシュの計算は変更なし。
- **sealer はライブラリ**（`crates/sealer`）。ストアには `application::SealStore` ポート越しにアクセスし、DB 実装に依存しない。
  `STORAGE=memory` では api プロセス内の tokio タスクとして `sealer::spawn` で動かす。
- **ポート**: `ChainRead`（head / block の読み取り。公開 API 用）と、それを拡張する `SealStore`
  （`pending_len` / `peek_pending` / `commit`）。`commit(shard, block, consumed)` は
  **ブロックの追加とプール先頭 `consumed` 件の削除を不可分に行い**、高さ・`prev_hash` の連続性と、
  ブロックの票がプール先頭の票と一致することを検証する（不一致なら `Conflict` で何も変更しない）。
- **封印の判定**は `domain::seal_policy::decide` に委ねる。窓の経過は単調時計（`Instant` ベース）で
  `Duration` の精度で測り、切り捨てた秒を渡す。設定が 10 秒なら実際に 10 秒経つまで満了しない。
  `sealed_at_minute` だけは壁時計を分に丸めて使う。
- **シャードごとに独立**した窓とチェーン（高さもシャードごと）。1 シャードの失敗が他に波及しない。
- **フラッシュ**: 通常の判定で件数到達分を封印した後、残りを `decide_flush` で `trigger=flush` の 1 ブロックにする
  （ブロックの票数が `SEAL_MAX_BALLOTS` を超えないため）。
- **SIGTERM / SIGINT の順序**: HTTP の新規受付を止め、処理中のリクエストを完了（上限 10 秒）→ sealer をフラッシュ停止 →
  終了。受理済みの票を取りこぼさない。サーバが異常終了した場合も sealer のフラッシュは行う。
- **公開 API（認証なし）**: `GET /api/v1/chains/{shard}/head`、`GET /api/v1/chains/{shard}/blocks/{height}`。
  ハッシュ・署名・公開鍵は小文字 hex。ブロックの票は `ballot_id` のハッシュ順で、投票者を特定する情報を含まない。
- **署名鍵**: `SEALER_SIGNING_SEED`（64 桁の hex、任意）。未設定なら起動ごとにランダム生成する（警告ログ）。
  公開鍵は head 応答で配るが、verifier は `--public-key` で固定できる（API を信頼しない検証）。
- **verifier `verify`**: シャード 0 から head が 404 になるまで取得して `verify_chain` で検証する。
  終了コード 0（OK）/ 1（実行時エラー）/ 2（使い方）/ 3（不整合を検出）。
- **改ざんデモ**: `POST /debug/tamper`（feature "dev-tools" 限定）が、メモリ上の封印済みの票 1 件の `candidate_id` を書き換える。
  応答は書き換えた位置のみ。
- **ログ**: 封印ごとに INFO `shard=0 height=N count=M trigger=count|time|flush`。端末以外では ANSI 装飾を付けない
  （フィールド名に制御文字が混ざると grep で検査できないため）。

## 理由
- 公開チェーンから集計するには、票がどのコンテストのものかが必要。封印前に決めないと、後から変更できない。
- 追加と削除を分けると、間に落ちたときに票の二重封印・消失が起きる。不可分にして、連続性検証で sealer のバグも検出する。
- 単調時計なら、NTP 補正などによる壁時計の巻き戻りで窓が誤って満了・停止しない。
- 公開鍵の固定を可能にすると、API サーバが偽のチェーンと偽の鍵を返しても検出できる。

## 代替案
- **コンテストごとにチェーンを分ける（`chains/{shard}` を `{shard}/{contest}` に）**: 票フォーマットは変わらないが、
  ブロックが小さくなり封印の窓・件数がコンテスト × シャードに分散して、ポリシーの意味が変わる。不採用。
- **`peek` と `remove` を別操作にする**: 上記の二重封印・消失の恐れがあるため不採用。
- **壁時計で窓を測る**: `decide` に秒をそのまま渡せて単純だが、時計の巻き戻りに弱い。不採用。
- **verifier が API の鍵を常に信頼する**: 単純だが、API 改ざんに無防備。`--public-key` を併設した。

## 留意点（暫定）
- `STORAGE=memory` のみ対応。DB 版では sealer を別プロセスとして動かし、シャードごとに単一の sealer を保証する
  仕組み（リース等）が必要になる。
- 同一 `ballot_id` の重複（UUIDv4 では実質起こらない）が起きると、そのシャードの封印が `DuplicateBallot` で止まる。
  DB 版では投入時に一意性を保証する。
- check_step4 は、250 票が 1 シャードに集まるよう `SHARD_COUNT=1` で実行する。
