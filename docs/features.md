# 機能の詳細

## 投票の流れと API

有権者は、ログイン → 自分に関係する投票用紙を決まった順に 1 枚ずつ投票 → 完了、と進む（順番は選べない。[data-setup.md](data-setup.md#id-体系原則13)）。

| エンドポイント（公開用のポート） | 認証 | 内容 |
|---|---|---|
| `GET /healthz` | 不要 | ヘルスチェック |
| `GET /api/v1/election-status` | 不要 | 選挙状態・期間・`open` で固定したルール（`rules`） |
| `POST /api/v1/login` | 不要 | ログイン（`login_id`・`password`・`my_number`）。セッショントークンを返す |
| `GET /api/v1/ballot-status` | 要 | 有権者に関係する投票用紙を表示順に（投票済みか・`ballots_cast`。再投票を認める選挙では `revote`） |
| `GET /api/v1/contests/{election_id}/{district_id}/candidates` | 要 | 候補者の一覧と `allow_blank`（対象外は 403） |
| `POST /api/v1/contests/{election_id}/{district_id}/vote` | 要 | 投票（`candidate_id`。再投票は `revote` も） |
| `GET /api/v1/chains`・`/chains/{shard}/head`・`/chains/{shard}/blocks`・`/chains/{shard}/blocks/{height}`・`/anchors`・`/anchors/latest` | 不要 | ブロックチェーンのビューア（下記） |
| `GET /api/v1/audit/counts` | 不要 | 突合用の投票用紙ごとの件数（`chain.reveal_ballots=after_close` の締切前は `cast` などを返さない） |
| `GET /debug/pool`・`POST /debug/tamper` | 不要 | `--features dev-tools` でビルドしたときだけ（開発用。改ざんデモ） |

エラー応答は `error`（コード）と `message`（`labels.*` から作る文言）。

## 封印とアンカー

票はシャードごとの未封印のプールに入り、sealer が封印のルール（原則9。[configuration.md](configuration.md#seal封印のルール原則9adr-0020)）で
ブロックにする。ブロック内の票は `ballot_id` のハッシュ順、時刻は分単位（原則3）。

- `seal.interval_secs` ごとに、全シャードの先頭を署名でまとめた**アンカー**を作るかを判定する。**データに更新が無ければ、チェーンに何も
  追加しない**: 直前のアンカー以降、どのシャードの先頭も変わっていなければアンカーを作らない（DEBUG ログに `skip`）。ブロックも、
  最小件数未満のまま間隔が過ぎても、締切の手続きで 0 件でも作らない（[ADR 0012](adr/0012-no-append-without-change.md)）。
- 停止時・締切の手続きの最終アンカーは、変化があれば 1 つ作り、無ければ最後のアンカーが最新を指していることを確認するだけ。
- 封印の直後に間隔が過ぎたときは、アンカーは最大 1 間隔遅れて作られる。ログの `trigger` は `count` / `time` / `close`。

## 画面とテーマ

画面は白基調で、ヘッダの切り替えボタン（**ライト / ダーク / OSに合わせる**）でダークモードにできる（原則16・[ADR 0018](adr/0018-design-tokens-and-theme.md)）。

- **デザイントークン**: 色・余白・角丸・フォントサイズは、[crates/web/style.css](../crates/web/style.css) の `:root` に CSS カスタムプロパティ
  （`--color-*`・`--space-*`・`--radius-*`・`--font-size-*`）として定義し、各ルールは `var(--…)` で参照する。`[data-theme="dark"]` は色の
  トークンだけを上書きする。色をトークン以外の場所（CSS のルール・Rust・`index.html`）に直接書かない。
- **切り替え**: 初回は `prefers-color-scheme` に従う。選んだ内容（`light` / `dark` / `system`）は `localStorage` の `theme` にだけ保存する
  （セッショントークンや投票の内容はブラウザに保存しない）。`system` のときは、OS の設定の変更に再読み込みなしで追従する。
- **ちらつきの防止**: `index.html` の `<head>` 内（スタイルシートより前）のインラインスクリプトが、WASM の読み込み前に `data-theme` を設定する
  （規則は `web/src/theme.rs` と同じ）。
- **コントラスト**: 両方のテーマで WCAG AA（文字 4.5:1 以上、枠線・フォーカス 3:1 以上）。`web::theme` のテストがトークンの値から計算する。
- **選択状態を色だけで表さない**: 選択中の候補者は太い枠・太字・「✓ 選択中」、進捗は「✓ 済み / ▶ 今 / ○ これから」、テーマのボタンは
  ✓・太い枠・`aria-pressed`、エラーは先頭の「⚠」、リンクは下線。

目で見る確認は [manual_check_step15.md](manual_check_step15.md)、投票画面は [manual_check_step5.md](manual_check_step5.md)。

## 白票

有権者は、候補者の代わりに白票（どの候補者にも投票しない）を選べる（原則20・[ADR 0021](adr/0021-blank-vote.md)）。

- **画面**: 候補者一覧の最後に、白票の選択肢（`labels.blank_option`）を候補者と区切って置く。確認画面は `labels.blank_confirm`。
- **API**: `vote` の `candidate_id` に予約値 `"blank"`（小文字の完全一致。`"BLANK"` などは 422 `invalid_candidate`）。白票でも、その投票用紙は投票済みになる。
- **使わない選挙**: `vote.allow_blank = false`（`open` で固定）なら、画面に出さず、API は 422 `blank_not_allowed`。
- **チェーン・ビューア・集計**: 票の `candidate_id` に `blank` がそのまま入る（ブロックの形式は変わらない）。ビューアは `labels.blank_name` で
  候補者と区別できる書式で、集計は候補者とは別の行（CSV は白票の列）に出す。有効票 + 白票 = 合計。

## 再投票

`vote.allow_revote = true`（既定 `false`。`open` で固定）の選挙では、投票期間中に、投票済みの投票用紙に投票し直せる（上限は
`vote.max_revotes` 回。原則1・[ADR 0022](adr/0022-revote.md)）。集計は、それぞれの最後の票だけを数える。

- **仮名 slot**: `slot = HMAC-SHA256(revote_key, election_id ‖ voter_id ‖ contest_id)`（各 ID に 2 バイトの長さ接頭辞）。票には slot・
  `seq`（その slot の何番目か）・`supersedes`（1 つ前の票のハッシュ）が入り、同じ slot の票は同じシャード（`hash(slot)`）に入る。
  鍵の管理は [security.md](security.md#秘密情報)。
- **画面**: すべて投票した後の完了画面に「投票をやり直す」（`labels.revote_button`。期間内だけ）→ 投票済みの投票用紙の一覧（固定の順番。
  上限に達したものは選べず `labels.revote_limit_reached`）→ 候補者を選ぶ → 確認画面は `labels.revote_confirm`。
  **前回の投票内容は、画面にも API にも出さない**。
- **API**: `vote` の本文に `"revote": <見た票の数>`（`ballot-status` の `ballots_cast`）を足す。その値のときだけ再投票する
  （同時に 2 つ送っても 1 件だけが 201、残りは 409 `revote_conflict`）。`revote` を省くと投票済みには 409 `already_voted`。
  上限は 409 `revote_limit_reached`、認めない選挙は 409 `revote_not_allowed`、未投票は 409 `not_voted`。
- **検証・集計**: `verifier` が、slot ごとに seq が 1 から連続・supersedes が 1 つ前の票のハッシュ・上限・認めない選挙に 2 回目の票が
  無いことを確かめる。締切後の集計だけ、変更の内訳（`revotes.csv`）を出す。

## ブロックチェーンのビューア

封印されたブロックを、ログインなしで確認できる画面と API（原則14・[ADR 0017](adr/0017-chain-viewer.md)）。画面のヘッダの「ブロックチェーン」から開く。

| 画面 | 内容 |
|---|---|
| `/chain` | シャードの一覧と、それぞれの先頭ブロック。署名者の公開鍵。「このチェーンを検証するには」（verifier のコマンド） |
| `/chain/{shard}` | ブロックの一覧（新しい順。「さらに古いブロックを表示」でページ送り） |
| `/chain/{shard}/blocks/{height}` | ブロックの詳細: 高さ・ハッシュ・前のブロックへのリンク・Merkle 根・票数・封印時刻（分）・署名・公開鍵。票が公開されていれば、票の一覧（`ballot_id` のハッシュ順）と表示名、再投票の票には「#<前の票> を置き換え（A→B）」のリンク |
| `/chain/anchors` | アンカーの一覧と、各アンカーが指す先頭ブロックへのリンク |

| API | 内容 | Cache-Control |
|---|---|---|
| `GET /api/v1/chains` | シャードの一覧と先頭の要約 | `no-cache` |
| `GET /api/v1/chains/{shard}/blocks?before_height=&limit=` | ブロックの要約を新しい順に（`limit` は既定 20、1〜100）。次のページは応答の `next_before_height` を渡す | `before_height` が先頭 + 1 以下なら `immutable`、それ以外は `no-cache` |
| `GET /api/v1/chains/{shard}/blocks/{height}` | ブロックの詳細（票は `ballots`、公開しているかは `ballots_revealed`） | 票を返す応答は `public, max-age=31536000, immutable`、票を伏せた応答は `no-store` |
| `GET /api/v1/anchors?limit=` | アンカーを新しい順に | `no-cache` |
| エラー | | `no-store` |

- **`chain.reveal_ballots = "after_close"`**（要 `election.voting_closes_at`）: 締切前は、詳細に票の中身（`ballot_id`・`contest_id`・
  `candidate_id`・slot / seq / supersedes）を含めず、ヘッダーだけを返す。締切は api の時計で判定する（締切ちょうどから公開。再起動は不要）。
  ブロックのハッシュは票から再計算するので、このとき `verify` / `tally` も締切後にしかできない（終了コード 4）。
- 確定した応答だけに `immutable` を付ける。締切前の伏せた詳細に付けると、票の無い版が CDN に 1 年残るため。

## チェーンの形式

ブロックのハッシュは、固定長ビッグエンディアンのバイナリ正規化の SHA-256（原則4。serde_json などは使わない。[ADR 0002](adr/0002-hash-chain-encoding.md)）。
票の正規化バイト列は `ballot_id(16) ‖ len(2) ‖ contest_id ‖ len(2) ‖ candidate_id`（版 2。[ADR 0013](adr/0013-string-ids-and-chain-format-v2.md)）。
版 3 から、再投票のつながりを持つ票だけ、後ろに `0x01 ‖ slot(32) ‖ seq(4)`（初回）または `0x02 ‖ slot(32) ‖ seq(4) ‖ supersedes(32)` を
足す（つながりの無い票は版 2 と同じバイト列）。
版 4 から、ヘッダーは `version(2) ‖ height(8) ‖ prev_hash(32) ‖ merkle_root(32) ‖ ballot_count(4) ‖ sealed_at_minute(8) ‖ election_hash(32)`
（118 バイト）。`election_hash` は選挙定義のハッシュで、ジェネシスが持ち、以後のブロックは引き継ぐ（[ADR 0025](adr/0025-election-definition-hash.md)）。
対象は、seed から読み込んだ後の選挙・選挙の種類・選挙区・候補者（名前・政党・略歴を含む）を ID 順に並べたもの（有権者名簿は含めない）で、
ファイルの行・列の順番や改行コードが違っても、内容が同じなら同じ値になる。`GET /api/v1/election-status` とチェーンの API のヘッダーで公開し、
ビューアにも表示する。
旧いスキーマ・旧いチェーンとは互換性がない（接続時に検出して、作り直しの手順つきで失敗する）。
