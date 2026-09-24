# 0019: 選挙状態の遷移と投票の受付期間

## 背景
投票の受付期間（`election.voting_opens_at` / `voting_closes_at`）は、値の形式（RFC 3339・秒まで・オフセット必須・開始 < 終了）は
検証していたが、api も sealer も、その値を使って何かを制御することはなかった（`voting_opens_at` は「未実装」でエラーにしていた）。
選挙を秒単位のスケジュールで開始・終了し、締切の手続き（残りの票の封印・最終アンカー）を確実に、かつ一度だけ行う手段が必要になった
（原則17・18・19）。

## 決定

### 状態と保存先
- 状態は `scheduled → open → closing → closed` の順にしか進まない（`domain::ElectionPhase`。原則17）。DB
  （`app.mode=memory` ではプロセス内の `InMemoryStore`）の単一行（`election_state`）を正とする。同じ構造（1 つの
  ストアが複数のポートを実装する既存の方針。`SealStore`/`ChainRead`/`LeaseStore` と同じ）で、`InMemoryStore` /
  `ScyllaStore` の両方が新しいポート `application::ElectionStateStore` を実装する。
- `election_audit`（変更のたびに、日時・変更前・変更後・実行した主体を記録。原則17）は、クラスタリングキー
  `(at_unix_secs, id)`（`id` は乱数。同じ秒に複数回変更されても競合しない）で、監査目的だけの追記専用テーブル。
- **init 時の取り込みと食い違いの扱い**: `ElectionStateStore::ensure_initialized` は `INSERT ... IF NOT EXISTS` で
  1 回だけ、設定の期間を取り込む（`cluster_config`（shard_count）と同じ、`ScyllaStore::check_cluster_config` の
  パターン）。既にある場合は、その値を返すだけ。呼び出し側（api の `build()`、sealer の `main()`）が、返ってきた
  値と自分の設定を比較し、食い違っていれば警告して DB の値を使う（`cluster_config` は不一致をエラーにするが、
  ここは選挙の運用中に設定ファイルだけ書き換えても選挙が壊れないよう、警告にとどめる）。
- **1 段の遷移だけを許す**: `ElectionStateStore::transition(from, to, ...)` は、ストアの実装内で
  `ElectionPhase::can_advance_to` を確認し、1 段の順序どおりでなければ何もせず `false` を返す（CAS が失敗したのと
  同じ扱い）。呼び出し側の不具合で、原則17の順序を飛び越えることを防ぐ。

### 自動遷移と締切の手続き
- **駆動する主体**: アンカーのリースを持っている sealer（`sealer::Coordinator::election_duty`）だけが、
  `scheduled → open`・`open → closing`・`closing → closed` を判定・実行する。`app.mode=memory` にはリースも複数
  プロセスも無いので、api 内蔵の唯一の sealer タスク（`sealer::runner::spawn` に組み込んだ `election_tick`）が
  そのまま担う。判定そのもの（`domain::automatic_transition`）は、`seal_policy::decide` と同じ方針の純粋関数
  （時刻は引数）。
- **closing の間のフラッシュは全 sealer が行う**: 「アンカー担当だけが判定する」のは状態遷移そのもの
  （schedule/open/closing/closed の付け替え）だけで、**フラッシュは、closing を観測した全 sealer が、
  それぞれ自分の持っているシャードを直ちに行う**（`Coordinator::flush_if_closing`。アンカー担当かどうかを問わない）。
  シャードごとに単一の sealer だけが書ける前提（原則の既存の制約）を保ったまま、`closing` を検知した全プロセスが
  自分の担当分をすぐに封印できるようにするため。
- **締切の手続き**（アンカー担当だけが行う）: `closing` を検知 → 待ち時間（`election.state_cache_secs +
  api.request_timeout_secs`。各 api の短期キャッシュと処理中のリクエストの分） → 全シャードの未封印が 0 件を
  確認（0 件でなければ、次の周期に再確認するだけで、待ち続ける） → 直前のアンカー以降に変化があれば最終アンカー
  （既存の `finalize_anchor`。「データに更新がなければ追加しない」の方針をそのまま使う） → `closing → closed`。
- **待ち時間の起点**: closing を検知した瞬間の単調時計 + 猶予。プロセスの再起動やアンカー担当の引き継ぎが
  起きた場合は、新しい担当が検知した時点から待ち時間を数え直す（自己修復的だが、猶予がその分延びることがある。
  プロトタイプでは許容する）。

### 投票の受付（原則18）
- 判定は `domain::vote_gate(phase, period, now)`（純粋関数）。状態が `open` でも、`open --now` で期間前に手動で
  開けた場合や、締切の自動遷移がまだ効いていない場合を区別するため、**状態と期間の両方**を見る。
  開始前・締切の手続き中・終了後で、別のメッセージ（`labels.voting_not_started_message` / `voting_closing_message` /
  `voting_closed_message`）を返す。
- ゲートは `crates/api/src/routes.rs` の `vote` ハンドラでだけ行う（`application::VotingService` には手を入れない）。
  `VotingService` に選挙状態への依存を持たせると、既存のテスト・呼び出し側（`ballot_status`/`candidates`/
  `audit_counts`）にも波及するため、投票の受付だけに閉じる。
- **短期キャッシュ**（`ElectionGate`。`election.state_cache_secs`）: 投票のたびに DB を読むと負荷が大きいので、
  短時間だけキャッシュする。締切の手続きの待ち時間が、このキャッシュの最大の古さ（+ `api.request_timeout_secs`）を
  考慮しているのは、キャッシュが古い状態を返している間に受理したリクエストの処理が終わるのを待つため。

### 管理操作
- **admin.bind**（既定 `127.0.0.1:18081`）に、公開用のポート（`api.port`）とは別の `TcpListener` を立てる
  （原則: 公開用のポートに管理用のエンドポイントを置かない。ロードバランサーには公開用ポートだけを登録する前提）。
  トークン（`admin.token`。秘密情報）が未設定なら、管理用リスナー自体を起動しない。
- **open --now / close --now は、1 段の CAS だけ**（`Scheduled→Open` / `Open→Closing`）。締切の手続き（待ち時間・
  フラッシュ・最終アンカー・`Closed` への遷移）は、トリガーが自動（時刻）でも手動（`close --now`）でも、常に同じ
  自動の仕組みが行う。手動操作を「締切の手続きも含めて行う特別な経路」にしないことで、経路を 1 つに保つ。
- `scripts/election.sh`（bash + curl。`scripts/tally.sh` と同じ「薄いラッパー」の方針。新しいクレートは作らない）
  が、この管理用 API を呼ぶ。`schedule` の RFC 3339 の解釈は、`app_config::parse_rfc3339` をそのまま公開して使う
  （検証済みの実装を、admin ハンドラと二重に持たない）。

### 集計（tally）の重複コードの削除
- `verifier tally` の前提確認（`tally::gate::check`）を、締切（時刻）による判定から、選挙状態（`ElectionPhase`。
  `GET /api/v1/election-status` から取得）による判定に置き換えた。`closed` だけが確定した集計で、それより前は
  `--allow-interim` が無ければ中止する。`--allow-interim` 自体は `app.env=dev` のときだけ受け付ける（それ以外は
  使い方の誤りとして終了コード 2）。時刻による重複した判定コードを削除した（`tally::gate::Closing` 型・
  `Refusal::BeforeClose` を削除）。

## 理由
- 状態を DB の単一行に持ち、遷移をポート（トレイト）越しの CAS にしたことで、既存の `LeaseStore`/`SealStore` と
  同じ「複数プロセスが同時に触っても安全」という性質をそのまま得られる。
- 「フラッシュは全 sealer」「状態遷移はアンカー担当だけ」と役割を分けたことで、シャードの単一書き込み者の制約
  （既存の設計）を変えずに、closing の間の停止時間を最小化できる。
- 投票の受付ゲートを handler 層に閉じたことで、`VotingService` のテスト・呼び出し側への影響を避けられた。

## 代替案
- 選挙状態を `election.toml`（設定ファイル）で管理する: 複数プロセス間での一貫性（今どの状態か）を設定ファイルの
  再配布に頼ることになり、原則17の「DB を正とする」が満たせないため不採用。
- 締切の手続きを api が行う: api はステートレスで、水平にスケールする前提（原則6）。締切の手続き（1 度だけ実行
  したい処理）を複数の api インスタンスのどれが行うかの排他制御が新たに必要になる。sealer には、既にアンカーの
  リースという「1 つだけ」の仕組みがあるので、それに乗せた。
- `open --now` / `close --now` 自体に締切の手続きをやらせる（admin ハンドラの中で待ち時間・フラッシュ・アンカーまで
  行う）: admin ハンドラは HTTP リクエストの応答時間内で完結させたい一方、締切の手続きの待ち時間は数秒〜数十秒に
  なり得るため、リクエスト/応答の外（sealer の定期処理）で行う方が自然。
