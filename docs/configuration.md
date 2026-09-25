# 設定ファイル

設定は `config/` 配下の TOML に集約する（原則11・[ADR 0011](adr/0011-config-crate.md)）。読み込みと検証は
[crates/app-config](../crates/app-config) が行い、api・sealer・verifier・credgen・bench・スクリプトはすべてここから読む。
画面（web）は app-config に依存せず、ビルド時に `labels.*` だけを環境変数で受け取る。

## ファイルの種類

| 場所 | 内容 | git |
|---|---|---|
| [config/default.toml](../config/default.toml) | **全項目と既定値**（設定のスキーマ）。バイナリに埋め込まれるので、編集したら再ビルドする | 管理する |
| [config/dev.toml](../config/dev.toml) | `app.env = "dev"` の差分（封印の間隔を 10 秒にする・credgen が CSV を出す） | 管理する |
| [config/production.example.toml](../config/production.example.toml) | 本番の例。`config/production.toml` にコピーして使う（`app.env = "production"` では必須） | 例だけ管理する |
| `config/test.toml` | `app.env = "test"` の差分（無くてよい。確認スイートは設定ファイルを読まない `test` で動く） | — |
| `config/local.toml` | 自分の環境だけの上書き | 管理しない |
| `secrets/` | 秘密情報。1 ファイル 1 値（ファイルの中身が値）。一覧と管理は [security.md](security.md#秘密情報) | 管理しない |
| 環境変数 `APP__<セクション>__<項目>` | 大文字。例 `APP__SEAL__MAX_BALLOTS=5`、`APP__AUTH__ARGON2__MEMORY_KIB=65536`。リストはカンマ区切り（`APP__DB__NODES=host1:9042,host2:9042`） | — |

## 読み込まれる優先順

後のものが勝つ:

1. `config/default.toml`（埋め込み）
2. `config/<app.env>.toml`
3. `config/local.toml`
4. `secrets/`
5. 環境変数 `APP__…`

`app.env` そのものは、`APP__APP__ENV` → `config/local.toml` → 既定（`dev`）の順に決まる。ディレクトリの場所は
`APP_CONFIG_DIR`（既定 `config`）と `APP_SECRETS_DIR`（既定 `secrets`）で変えられる。

- 秘密情報（`session.secret`・`sealer.signing_seed`・`admin.token`）を設定ファイルに書くと、起動時にエラーになる。
  環境変数か `secrets/` で渡す。再投票の鍵（`secrets/revote_key`）は、ファイルでだけ渡せる。
- 不正な値（未知の項目・型違い・範囲外・TOML の構文エラー）は、起動時に「どのファイル（環境変数）のどの項目がなぜ不正か」を
  まとめて表示して終了する。例:

  ```
  設定が不正です（1 件）:
    - config/local.toml: seal.max_ballots = 0 は不正です（1 以上 4294967295 以下が必要です）
  ```
- 旧来の環境変数名（`SESSION_SECRET`・`STORAGE` など、`APP__` の付かないもの）は廃止した（ADR 0011）。

確認用の CLI:

```
cargo run -q -p app-config -- show              # 実効値と出所（どのファイル・環境変数か）。秘密情報は ***
cargo run -q -p app-config -- get api.port      # 1 つの値（スクリプト用。秘密情報は取得できない）
cargo run -q -p app-config -- validate          # 検証だけ
eval "$(cargo run -q -p app-config -- web-env)" # 画面の文言（labels.*）を、trunk のビルド用の環境変数にする
```

## 変更できるタイミング

各項目の「変更」列は、次のどれか。

| 記号 | 意味 |
|---|---|
| いつでも | 設定を変えて、読むプロセス（api・sealer など）を再起動すれば反映される |
| open で固定 | 選挙状態が `open` に移った時点で、`open` に移したプロセス（sealer / api 内蔵のスケジューラ / `open --now` を受けた api）の値を DB に固定し、以後は設定を変えても使わない（原則19）。食い違いは起動時に警告する |
| init で固定 | DB を初期化した後（最初の接続）は変えられない。変えると接続を拒否されるか、別のデータを指すことになる |
| init で取り込み | 最初の接続で DB に取り込み、以後は DB が正（設定と食い違えば警告して DB の値を使う）。変更は `scripts/election.sh schedule`（`scheduled` の間だけ） |
| 期間中は変えない | 固定はされないが、投票期間中に変えると原則に反する（運用で守る） |
| ビルド時 | 画面（wasm）に埋め込まれる。変えたら画面をビルドし直す |
| 実行時 | その CLI・スクリプトを実行するときだけ読む |

## 項目

### `[app]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `app.env` | `dev` | いつでも | 実行環境（`dev` / `production` / `test`）。`config/<env>.toml` を読む。`production` では `db_reset.sh`・`sample_data.sh` が拒否し、`--allow-interim` も使えない |
| `app.mode` | `memory` | いつでも | 保存先。`memory` = api のメモリ（再起動で票もチェーンも消える。sealer は api に内蔵）/ `db` = Cassandra・ScyllaDB（sealer は別プロセス） |

### `[api]` と `[web]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `api.port` | 18080 | いつでも | 公開用のポート（ロードバランサーに登録する）。`web.port` と重ねない |
| `api.request_timeout_secs` | 10 | いつでも | リクエストのタイムアウト（1 以上）。締切の手続きの待ち時間（`election.state_cache_secs` + この値）にも使う |
| `web.port` | 8080 | いつでも | 開発用の画面（`trunk serve`）のポート。`crates/web/Trunk.toml` と一致させる（`scripts/dev_up.sh` は設定から Trunk の設定を生成する） |

### `[db]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `db.backend` | `cassandra` | いつでも | `cassandra`（ローカルの既定）/ `scylla`。docker compose のプロファイルとスキーマ投入ジョブの選択に使う（[environment.md](environment.md)） |
| `db.nodes` | `["127.0.0.1:9042"]` | いつでも | 接続先ノード（`host:port`） |
| `db.keyspace` | `vote` | init で固定 | キースペース（英字で始まり、英数字と `_`、48 文字以内）。`docs/schema.cql` をこの名前で投入しておく。変えると別のデータを指す |

### `[seal]`（封印のルール。原則9・[ADR 0020](adr/0020-seal-policy-min-ballots.md)）

シャードごとに独立して判定する。残りを件数に関係なく封印するのは、締切の手続き（`closing`）の中だけで、SIGTERM ではフラッシュしない。

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `seal.max_ballots` | 100 | いつでも | 未封印がこの件数に達したら、この件数ですぐに封印する（`trigger=count`） |
| `seal.interval_secs` | 600 | いつでも | 前回の封印（または投票開始）からこの秒数以上経ち、かつ未封印が `min_ballots_after_interval` 件以上なら全件を封印する（`trigger=time`）。アンカーを作るかの判定の間隔も同じ |
| `seal.min_ballots_after_interval` | 10 | いつでも | 時間による封印に必要な最小件数。`max_ballots` より大きいと、時間では封印されない |

原則19 は封印ルールも `open` で固定するとしているが、未実装（再起動で変わる）。投票期間中は変えない。

### `[sealer]` と `[shard]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `sealer.id` | 空 | いつでも | sealer の識別名（リースの持ち主）。プロセスごとに一意にする。空なら起動ごとにランダム |
| `sealer.lease_ttl_secs` | 30 | いつでも | リースの TTL（3 以上）。TTL の 1/3 ごとに更新する。落ちた sealer の担当は、この秒数の後に引き継がれる |
| `shard.count` | 1 | init で固定 | シャード数（1 以上）。票は `hash(ballot_id)`（再投票ありなら `hash(slot)`）で振り分ける。api と全 sealer で同じ値にする（食い違うと DB が接続を拒否する） |

### `[auth]`・`[session]`・`[credentials]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `auth.mode` | `stub` | いつでも | `stub` = 入力した ID をそのまま投票者 ID にする（開発用。名簿は seed の `voters.csv`）/ `db` = 事前登録した ID とパスワードで認証する（`app.mode = "db"` が必要。名簿は DB の `voter_roll`）。`config/dev.toml` には書けない（memory モードでも読まれ、memory と db 認証の組み合わせは拒否されるため。`scripts/dev_up.sh` が環境変数で渡す） |
| `auth.argon2.memory_kib` | 19456 | いつでも | Argon2id のメモリ（KiB。8 × `parallelism` 以上）。credgen の登録と、存在しない ID のダミー照合に使う（登録済みのハッシュは、ハッシュに含まれるパラメータで照合する） |
| `auth.argon2.iterations` | 2 | いつでも | Argon2id の反復回数（1 以上） |
| `auth.argon2.parallelism` | 1 | いつでも | Argon2id の並列度（1 以上） |
| `session.ttl_secs` | 3600 | いつでも | セッション（HMAC 署名トークン）の有効秒数 |
| `credentials.output_file_enabled` | `false` | 実行時 | credgen が平文のパスワードを CSV に出すか。`false` なら警告し、`--confirm-no-output` が無ければ処理しない |
| `credentials.output_path` | `secrets/credentials.csv` | 実行時 | CSV の出力先（権限 0600・上書きしない）。git 管理外の場所にする |
| `credentials.password_length` | 16 | 実行時 | 発行するパスワードの長さ（8〜128） |
| `credentials.login_id_length` | 10 | 実行時 | 発行するログイン ID の長さ（8〜32） |

### `[election]`（選挙データと期間。原則17・18・[ADR 0019](adr/0019-election-lifecycle.md)）

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `election.seed_dir` | `seed` | init で固定 | 選挙データのディレクトリ（形式は [data-setup.md](data-setup.md#選挙データseed)） |
| `election.election_id` | `2026-general` | init で固定 | 読み込む選挙（`<seed_dir>/<election_id>/`） |
| `election.voting_opens_at` | 空 | init で取り込み | 投票の開始（RFC 3339、秒まで、オフセット必須。例 `2026-10-01T09:00:00+09:00`）。空なら `scheduled` のまま止まり、`scripts/election.sh` で開始する |
| `election.voting_closes_at` | 空 | init で取り込み | 投票の締切（開始より後）。この時刻から投票を拒否し、`chain.reveal_ballots = "after_close"` の票の公開もこの時刻から |
| `election.display_timezone` | `Asia/Tokyo` | いつでも | 画面に表示するタイムゾーン（`Asia/Tokyo` / `UTC`） |
| `election.state_cache_secs` | 5 | いつでも | 各 api が選挙状態をキャッシュする秒数（0 以上）。締切の手続きの待ち時間の計算に使う |

### `[vote]`（選挙のルール）

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `vote.allow_blank` | `true` | open で固定 | 白票を選べるか（[features.md](features.md#白票)）。`false` なら画面に出さず、API は 422 `blank_not_allowed` |
| `vote.allow_revote` | `false` | open で固定 | 投票期間中の再投票を認めるか（[features.md](features.md#再投票)）。`true` なら `secrets/revote_key` が必要 |
| `vote.max_revotes` | 5 | open で固定 | 再投票の上限回数（1〜100。初回を含めない） |

### `[chain]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `chain.reveal_ballots` | `always` | 期間中は変えない | ブロックの詳細で票の中身を公開するタイミング。`always` = 常に / `after_close` = `election.voting_closes_at` 以後だけ（要 `voting_closes_at`。原則14）。投票期間中に `always` へ変えると、締切前に票の中身が公開される |

### `[admin]`

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `admin.bind` | `127.0.0.1:18081` | いつでも | 管理用リスナー（選挙状態の操作）。公開用のポートとは別で、外部に出さない（[architecture-scaling.md](architecture-scaling.md)）。トークン `admin.token` が無ければ起動しない |

### `[sample]`（`scripts/sample_data.sh`）

| 項目 | 既定 | 変更 | 説明 |
|---|---|---|---|
| `sample.open_hours` | 24 | 実行時 | 投票期間の長さ（時間。1〜720）。`--phase open` は今から、`--phase before` は今から `open_hours` 時間後に開始 |
| `sample.output_dir` | `out/sample` | 実行時 | 選挙データ（`<output_dir>/seed`）と、パターンごとの ID・パスワードの CSV の出力先。git 管理外にする |

### `[labels]`（利用者に見える文言。原則12）

画面はビルド時に、api（エラーメッセージ）と verifier（集計の表示）は起動時に読む。変更は「ビルド時」（画面）かつ「いつでも」（api・verifier）。

| 項目 | 既定 | 説明 |
|---|---|---|
| `labels.site_title` | 投票システム（プロトタイプ） | ヘッダのサイト名 |
| `labels.done_message` | 投票を受け付けました | 投票完了画面の文言（受理したことだけを示す） |
| `labels.login_heading` | ログイン | ログイン画面の見出し |
| `labels.ballot_item` | 投票用紙 | 投票用紙 1 枚の呼び名（画面・API のエラー・集計の見出し） |
| `labels.progress` | {total}枚中{current}枚目 | 進捗の表示（`{total}` と `{current}` の両方が必須） |
| `labels.voting_not_started_message` | 投票の受付はまだ開始していません | 開始前に投票したとき |
| `labels.voting_closing_message` | 投票の受付を締め切っています。しばらくお待ちください | 締切の手続き中（`closing`）に投票したとき |
| `labels.voting_closed_message` | 投票の受付は終了しました | 終了後に投票したとき |
| `labels.blank_option` | 白票（どの候補者にも投票しない） | 候補者一覧の最後に置く白票の選択肢 |
| `labels.blank_confirm` | 白票として投票します。よろしいですか？ | 白票を選んだときの確認画面の文言 |
| `labels.blank_name` | 白票 | 集計・ビューア・API のエラーでの白票の呼び名 |
| `labels.revote_button` | 投票をやり直す | 完了画面の再投票のボタン |
| `labels.revote_confirm` | 前回の投票内容を変更します | 再投票の確認画面の文言（前回の内容は出さない） |
| `labels.revote_limit_reached` | やり直しの上限（{max}回）に達しています | 上限に達した理由（`{max}` が必須） |
