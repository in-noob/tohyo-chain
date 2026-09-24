# tohyo-chain — 投票システム プロトタイプ

Web 投票システムのプロトタイプ。水平スケール可能な API と、ハッシュチェーン（簡易ブロックチェーン）による
改ざん検知を検証する。設計方針・守るべき原則は [CLAUDE.md](CLAUDE.md)、設計判断は [docs/adr/](docs/adr/) を参照。

## 構成（Cargo ワークスペース）

| クレート | 役割 |
|---|---|
| `crates/domain` | ハッシュチェーン・Merkle 木・署名・封印ポリシー・選挙マスタと ID 体系（純粋なロジック）|
| `crates/application` | ユースケースとポート（トレイト）、セッショントークン |
| `crates/seed` | 選挙データ（`seed/<election_id>/` の TOML・CSV）の読み込みと検証（ファイル・行・原因つきのエラー）|
| `crates/credgen` | ログイン ID とパスワードの事前登録ツール（`auth.mode=db` 用。Argon2id のハッシュだけを DB に保存し、平文は CSV に出力する）|
| `crates/seedgen` | ダミーの選挙データの生成ツール（47 都道府県規模）と、既存データの検証（`--check`）|
| `crates/infra-memory` | インメモリのストア（開発・テスト用。再起動で消える）|
| `crates/app-config` | 設定の読み込みと検証（`config/*.toml`・`secrets/`・環境変数 `APP__…`）と、確認用 CLI（`show` / `get` / `validate` / `web-env`）|
| `crates/infra-scylla` | ScyllaDB のストア（`app.mode=db`。LWT で二重投票を防ぐ）|
| `crates/sealer` | 票をブロックに封印する。独立した実行ファイル（`app.mode=db`。リースで担当シャードを決める）と、api 内で動かすライブラリ（`STORAGE=memory`）|
| `crates/api` | axum の HTTP API |
| `crates/verifier` | チェーンの検証・集計 CLI（`demo` / `verify` / `tally`）|
| `crates/shared-types` | API と画面で共有する DTO |
| `crates/web` | 投票画面（Leptos CSR + Trunk）|

## 前提

- Rust 1.85 以上（edition 2024）
- 画面（`crates/web`）のビルドには次が必要:
  ```
  rustup target add wasm32-unknown-unknown
  cargo install trunk --locked
  ```

## 手元で動かす

画面を操作して確認するための、開発用の起動スクリプト。api と画面（`trunk serve`）を一度に起動・停止する。

```
scripts/dev_up.sh                        # memory（既定）: api（dev-tools 有効・sealer 内蔵・封印間隔 10 秒）+ 画面。認証は stub
scripts/dev_up.sh cassandra              # 上記 + Cassandra（docker compose）。sealer は別プロセス。投票は DB に残る。認証は db（既定）
scripts/dev_up.sh cassandra --auth stub  # cassandra で、認証を stub（入力した ID をそのまま使う）にする
scripts/dev_down.sh                      # 停止
```

- **認証（`--auth stub|db`）**: 既定は、memory が **stub**、cassandra が **db**（`auth.mode`。memory では db を使えない）。
  環境変数 `APP__AUTH__MODE` でも指定できる（`--auth` が優先）。`auth.mode` は `config/dev.toml` に書けない（memory モードでも読まれ、
  memory と db 認証の組み合わせは設定の検証で拒否されるため）ので、スクリプトが環境変数で渡す。
- **db 認証のとき**、起動の途中で、DB の `credentials` が空なら、**`credgen` が名簿の有権者を登録**し、ID とパスワードを
  `secrets/credentials.csv`（`credentials.output_path`。git 管理外・権限 0600）に出力する（`config/dev.toml` の
  `credentials.output_file_enabled = true`）。登録済みなら何もしない。パスワードは、画面にもログにも出さない。
  起動完了のメッセージに、CSV の場所と `head -3 secrets/credentials.csv` が表示される。
  - DB の認証情報が消えたとき（`dev_down.sh` → `APP__APP__MODE=db scripts/db_reset.sh --all --yes` の後など）は、次の `dev_up.sh cassandra` で自動で登録し直す
    （前回の CSV は `.old` に退避）。`db_reset.sh` は設定の `app.mode` を見るので、`dev_up.sh cassandra` が環境変数で渡している `app.mode=db` を、同じように指定する
    （`dev_down.sh` が止めた DB は、`db_reset.sh` が起動する）。
  - パスワードを忘れた・CSV を失くしたとき: `APP__CREDENTIALS__OUTPUT_PATH=<新しい CSV> cargo run -q -p credgen -- --reissue`。

- 起動が終わると、ブラウザで開く URL（http://localhost:8080）、ログインに使える ID の例と規則、チェーンの先頭を見る `curl`、
  `verify` と改ざんデモのコマンド、ログの `tail` コマンドが表示される。
- ポートは **画面 8080、api 18080**。`/api` は [crates/web/Trunk.toml](crates/web/Trunk.toml) の proxy が api へ転送する（同一オリジンなので CORS は不要）。
- ログは `logs/{api,sealer,trunk}.log`、PID は `.dev/pids`（どちらも git 管理外）。
- `dev_down.sh` は、api と sealer に SIGTERM を送って止める（sealer はリースも解放する）。未封印の票はフラッシュしない
  （原則9。残りは、次の起動の後に封印ルールで封印されるか、締切の手続きの中で封印される。cassandra モードでは DB に残る）。
  cassandra モードでは `docker compose stop` も行う（ボリュームは削除しないので、データは残る）。
- **cassandra モード**は、既定のキースペース `vote` にスキーマを投入する（`IF NOT EXISTS`。既存のデータは残る）。
  `shard.count`（既定 1）と署名鍵が `vote` に登録済みの値と食い違うと、起動を拒否される（ログに理由が出る）。
- 改ざんデモの `POST /debug/tamper` は、**memory モードだけ**で使える（メモリ上のストアが対象）。cassandra モードでは
  `cargo run -q -p verifier -- demo`（オフラインのデモ）を使う。
- 設定は「[設定](#設定)」の仕組みで読む（`config/dev.toml` が効く。手元の上書きは `config/local.toml`）。ポート（`api.port` / `web.port`）と画面の文言（`labels.*`）も設定から渡す。
  起動前に設定を検証し、不正なら、どのファイルのどの項目がなぜ不正かを表示して終了する。
- スクリプト内のシークレットと署名鍵は、**開発用の固定値**（`APP__SESSION__SECRET` や `secrets/` で指定していれば、そちらを使う）。公開されているので、本番では絶対に使わないこと。
- 前提: `trunk`（`cargo install trunk --locked`）と wasm ターゲット（下の「前提」）。初回は wasm のビルドで数分かかる。
  cassandra モードには Docker（Compose プラグイン）も必要。

## 起動（開発）: 個別に起動する

API（ポートは `api.port`、既定 18080。画面の proxy 先と合わせる）。`session.secret` は必須（16 バイト以上。秘密情報なので、環境変数か `secrets/` で渡す）:

```
APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api --features dev-tools
```

画面（ポート 8080。`/api` は `trunk serve` のプロキシが 18080 の API へ転送する）:

```
cd crates/web && trunk serve
```

ブラウザで http://localhost:8080 を開く。

> **`trunk` コマンドは必ず `crates/web` で実行する。**
> ワークスペースのルートは仮想マニフェスト（パッケージなし）で、Trunk はカレントディレクトリの `Cargo.toml` から
> ルートパッケージを探すため、ルートで実行すると `could not find the root package of the target crate` で失敗する。
> `index.html`・`Trunk.toml`（proxy 設定を含む）も `crates/web/` 直下にある。成果物は `crates/web/dist/`（git 管理外）。

リリースビルド: `cd crates/web && trunk build --release`

## 起動（DB を使う: `app.mode=db`）

ローカル開発の既定の DB は **Apache Cassandra 5.0** で、ScyllaDB は明示的に指定したときだけ使う。
どちらも同じ `docs/schema.cql`（ScyllaDB / Cassandra 共通の CQL）を使い、api は同じドライバ（`scylla` クレート）で接続する。

> **39bit の仮想アドレス空間（VA）の ARM 環境（ChromeOS の Linux など）では ScyllaDB が動かないため、
> ローカルは Cassandra を使う。性能計測は ScyllaDB で、別の環境で行う。**
>
> 理由: ScyllaDB は起動時に 32TiB の仮想アドレス空間を予約するが、39bit VA のカーネルでは連続 512GiB 以上を
> 確保できず、起動直後に `SIGABRT`（`mmap` が `ENOMEM`）で終了する（バージョンを変えても、設定を変えても同じ）。
> Cassandra（JVM）はこの環境で動く。Cassandra での確認は ScyllaDB 自体の確認や性能の指標にはならない
> （LWT の実装や競合時の挙動が異なる）。

```
docker compose --profile cassandra up -d --wait cassandra     # 起動（healthy になるまで待つ）
docker compose --profile cassandra run --rm schema-cassandra  # docs/schema.cql を投入（冪等）
# ここで api と sealer を起動する（下の説明を参照）
docker compose --profile cassandra --profile scylla down -v   # 停止してデータも消す
```

`docs/schema.cql` はキースペース名を `{{KEYSPACE}}` にしたテンプレートで、上の schema ジョブは既定で `vote` に投入する
（別名にするなら `SCHEMA_KEYSPACE=my_ks docker compose ... run --rm schema-cassandra`。api / sealer は `APP__DB__KEYSPACE=my_ks`）。
コードの CQL はすべて `<keyspace>.<table>` の完全修飾名で、接続時に `USE` はしない（[ADR 0010](docs/adr/0010-test-isolation-per-run-keyspace.md)）。

ScyllaDB を使うときは、`cassandra` を `scylla`、`schema-cassandra` を `schema-scylla` に読み替える
（`--profile scylla`。`db.backend=scylla`）。接続先は `db.nodes`（既定 127.0.0.1:9042）。docker compose が公開するポートは `DB_PORT` で変更できる。

`app.mode=db`（DB を使うモード。Cassandra でも ScyllaDB でも同じ）では、
**api は封印しない**。封印は独立した **sealer プロセス**が行う（複数起動できる）:

```
export APP__APP__MODE=db
export APP__SHARD__COUNT=4      # api と全 sealer で同じ値にする（食い違うと DB が接続を拒否する）
export APP__SEALER__SIGNING_SEED=$(printf '07%.0s' {1..32})   # 秘密情報（開発用の値。全 sealer で同じ）
APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api
APP__SEALER__ID=sealer-a cargo run -p sealer
APP__SEALER__ID=sealer-b cargo run -p sealer
```

- sealer は、DB のリース（TTL 付きの LWT）を取ったシャードだけを封印する。1 つが落ちると、リースの期限（TTL）が切れた後に、
  残りの sealer がそのシャードを引き継ぐ。リースを失ったシャードは直ちに処理を止める。SIGTERM では、票をフラッシュせずに
  （原則9）、アンカー担当なら最終アンカーを済ませ、リースを解放してから終了する。
- `sealer.signing_seed`（64 桁の hex。秘密情報）は sealer が持つ。**全 sealer で同じ値**にする（別の鍵だと DB が登録を拒否する）。
  api は署名の公開鍵を DB から読むので、秘密の種を持たない。
- **封印のルール**（原則9。シャードごとに独立。[ADR 0020](docs/adr/0020-seal-policy-min-ballots.md)）: 未封印が `seal.max_ballots`（100）件に
  達したら 100 件ですぐに封印（`trigger=count`）。前回の封印（または投票開始）から `seal.interval_secs`（600）秒以上経ち、かつ未封印が
  `seal.min_ballots_after_interval`（10）件以上なら全件を封印（`trigger=time`）。それ以外は待つ（0 件でも待つ。窓はリセットしない）。
  残りを件数に関係なく封印するのは、締切の手続き（選挙状態 `closing`）の中でだけ（`trigger=close`）。SIGTERM ではフラッシュしない。
  投票開始時刻は、選挙状態が `open` に移った時刻（DB の `election_state.opened_at`）と設定の開始時刻の遅い方。
- `seal.interval_secs`（既定 600 秒 = 10 分）ごとに、全シャードの head を署名でまとめた**アンカー**を作るかどうかを判定する。
  **データに更新がない場合は、ブロックチェーンに何も追加しない**: 直前のアンカー以降、どのシャードの先頭ブロックも変わっていなければ、
  アンカーは作らない（タイマーだけ進め、DEBUG ログに `skip` と出す。ジェネシスだけの状態も「更新なし」）。ブロックも、
  0 件（または最小件数未満）のまま間隔が過ぎても、締切の手続きで 0 件でも作らない。停止時・締切の手続きの最終アンカーは、変化がなければ、
  最後のアンカーが最新の状態を指していることを確認するだけで、作らない（変化があれば、追いつかせるために 1 つ作る）。
  ブロックが封印された直後に間隔が過ぎたときは、アンカーは最大 1 間隔遅れて作られる。
  （ログと `GET /api/v1/anchors/latest`）。

## ログイン ID とパスワードの事前登録（`auth.mode=db`）

`auth.mode=db`（要 `app.mode=db`）では、**事前に登録した ID とパスワードの組み合わせ**だけがログインできる
（[ADR 0015](docs/adr/0015-credentials-and-db-auth.md)）。`auth.mode=stub`（既定）は、これまでどおり入力した ID をそのまま採用する。
登録は CLI `credgen` が行う。有権者は、`seedgen` の名簿（`seed/<election_id>/voters.csv`）から読む。

```
docker compose --profile cassandra up -d --wait cassandra && docker compose --profile cassandra run --rm schema-cassandra
export APP__APP__MODE=db APP__AUTH__MODE=db
export APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true          # 平文のパスワードを CSV に出す（郵送のための唯一の機会）
cargo run -q -p credgen                                     # 新規の有権者だけ登録。出力: secrets/credentials.csv（権限 0600）
cargo run -q -p credgen -- --reissue                        # 登録済みの有権者も、新しい ID とパスワードで再発行（古いものは無効）
APP__CREDENTIALS__OUTPUT_FILE_ENABLED=false cargo run -q -p credgen -- --confirm-no-output   # CSV に出さずに登録（平文はどこにも残らない）
```

- ログイン ID とパスワードは、OS の暗号論的乱数から作る。郵送を前提に、見間違えやすい文字（`0/O`・`1/I/l`）を使わない
  （`ABCDEFGHJKLMNPQRSTUVWXYZ23456789` の 32 文字）。長さは `credentials.login_id_length`（既定 10）・`credentials.password_length`（既定 16）。
  ログイン時、大文字小文字・空白・ハイフンは無視する（書き写しの揺れ）。
- DB の `credentials` テーブルには、`login_id`・**パスワードの Argon2id ハッシュ**・`voter_id`（内部用のランダムな値。ログイン ID とは別）だけを保存する。
  平文は DB にもログにも残さない。`voter_roll`（voter_id → 選挙区）は、`auth.mode=db` の api が名簿として使う。`voter_registry`
  （名簿の ID → voter_id・ログイン ID）は、credgen の再実行（スキップ / 再発行）のための台帳。
- **CSV**（`credentials.output_file_enabled=true` のときだけ。`credentials.output_path`、既定 `secrets/credentials.csv`）: 列は
  `login_id, password, 都道府県, 選挙区`（居住地の都道府県、選挙区は表示順に `;` 区切り）。ファイルの権限は 0600、既存ファイルは上書きしない。
  出力先は `.gitignore` に登録済み。**平文のパスワードを含むので、郵送の準備が済んだら安全に削除する**。
- `output_file_enabled=false` のときは、「平文のパスワードは二度と取り出せません」と警告し、`--confirm-no-output` を付けない限り何も処理しない（終了コード 2）。
- 同じ有権者に 2 回実行すると、既定では**スキップ**（何も変えない）。`--reissue` なら**再発行**（内部の `voter_id` は変えないので、投票済みの記録は保たれる）。
- ログインの失敗（存在しない ID・パスワード違い・形式が不正な ID・パスワード欠落）は、**同じ 401 と同じメッセージ**で、応答時間も揃える
  （存在しない ID でも、設定と同じパラメータのダミーハッシュで Argon2id の照合を 1 回行う）。
- ログイン API: `POST /api/v1/login` `{"login_id": "...", "password": "...", "my_number": "..."}`（旧名 `voter_id` も受け付ける）。
  `my_number`（マイナンバー）は画面と API の項目として存在するだけで、**受け取って破棄する**（保存も、ログ出力もしない）。
- 対象外（ログイン失敗の回数制限・ロックアウト、パスワードの期限・変更、メール等での配布）は、この版には無い。

## DB のリセット（`scripts/db_reset.sh`）

```
scripts/db_reset.sh            # = --votes: 投票済み記録・票のプール・ブロック・アンカー・リースを削除（選挙の定義と認証情報は残す）
scripts/db_reset.sh --all      # キースペースを削除して、スキーマから作り直す（認証情報も消える）
scripts/db_reset.sh --yes      # 確認を省略
```

削除する件数を表示して、`yes` と入力するまで削除しない。**api や sealer が動いているときは、止めるよう促して終了する**
（動いている最中に消すと、sealer が持っているチェーンの状態と DB が食い違うため）。`app.env=production` では、何も削除せずにエラーで終了する。
`app.mode=memory` では「再起動するとリセットされます」と表示して終了する。対象は設定の `db.keyspace`（`APP__DB__KEYSPACE`）。

## デザインとテーマ（ライト / ダーク）

画面は白基調で、ヘッダーの切り替えボタン（**ライト / ダーク / OSに合わせる**）でダークモードにできる
（[ADR 0018](docs/adr/0018-design-tokens-and-theme.md)）。

- **デザイントークン**: 色・余白・角丸・フォントサイズは、[crates/web/style.css](crates/web/style.css) の `:root` に、CSS カスタムプロパティ
  （`--color-*`・`--space-*`・`--radius-*`・`--font-size-*`）として定義し、各ルールは `var(--…)` で参照する。
  `[data-theme="dark"]` は、**色のトークンだけ**を上書きする。**色は、トークン以外の場所（CSS のルール・Rust・`index.html`）に直接書かない**。
  色を変えたいときは、トークンの値だけを直す。
- **切り替え**: 初回は OS の設定（`prefers-color-scheme`）に従う。選んだ内容（`light` / `dark` / `system`）は、`localStorage` の `theme` に保存する
  （保存するのは、テーマの設定だけ。セッショントークンや投票の内容は、ブラウザに保存しない）。`system` のときは、OS の設定の変更に、再読み込みなしで追従する。
- **ちらつきの防止**: WASM の読み込みより前に正しい配色を適用するため、`index.html` の `<head>` 内（スタイルシートより前）の小さなインラインスクリプトが、
  `<html data-theme="light|dark">` を先に設定する（規則は `web/src/theme.rs` と同じ）。
- **コントラスト**: 両方のテーマで、文字と背景のコントラスト比が WCAG AA（通常の文字で 4.5:1 以上）。枠線・フォーカスは 3:1 以上。
  `web::theme` のテストが、`style.css` のトークンの値から計算して確認する（色を変えて基準に届かないと、どのトークンの組み合わせかを示して失敗する）。
- **選択状態を色だけで表さない**: 選択中の候補者は、太い枠・太字・「✓ 選択中」、進捗は「✓ 済み / ▶ 今 / ○ これから」と、「今」の左の太い線、
  テーマの切り替えボタンは、✓・太い枠・`aria-pressed`、エラーは先頭の「⚠」、リンクは下線でも示す。
- 投票・確認・完了画面とビューア（`/chain` 以下）は、すべて同じトークンを使う。目で見る確認は [docs/manual_check_step15.md](docs/manual_check_step15.md)（自動の確認は `scripts/check/web.sh`）。

## ブロックチェーンのビューア（`/chain`）

封印されたブロックを、ログインなしで確認できる画面と API（[ADR 0017](docs/adr/0017-chain-viewer.md)）。
`trunk serve` の画面（http://localhost:8080）の、ヘッダの「ブロックチェーン」から開く。

| 画面 | 内容 |
|---|---|
| `/chain` | シャードの一覧と、それぞれの先頭ブロック。署名者の公開鍵。「このチェーンを検証するには」（verifier のコマンド）|
| `/chain/{shard}` | ブロックの一覧（新しい順。「さらに古いブロックを表示」でページ送り）|
| `/chain/{shard}/blocks/{height}` | ブロックの詳細: 高さ・ブロックハッシュ・前のブロックのハッシュ（前のブロックへのリンク）・Merkle 根・票数・封印時刻（分単位）・署名・署名者の公開鍵。締切後（`reveal_ballots=always` なら常に）は、票の一覧（`ballot_id` のハッシュ順）と、選挙区・候補者の表示名 |
| `/chain/anchors` | アンカーの一覧と、各アンカーが指す各シャードの先頭ブロックへのリンク |

**API**（認証なし）:

| エンドポイント | 内容 | Cache-Control |
|---|---|---|
| `GET /api/v1/chains` | シャードの一覧と、それぞれの先頭ブロックの要約 | `no-cache` |
| `GET /api/v1/chains/{shard}/blocks?before_height=&limit=` | ブロックの要約（票は含まない）を新しい順に。`before_height` より低い高さを `limit` 件（既定 20、1〜100 にそろえる。空文字は指定なし）。次のページは、応答の `next_before_height` を `before_height` に渡す（最後は無い）| `before_height` が先頭の高さ + 1 以下なら `immutable`、それ以外（先頭からのページ・先頭より先）は `no-cache` |
| `GET /api/v1/chains/{shard}/blocks/{height}` | ブロックの詳細（票は `ballots`、公開しているかは `ballots_revealed`）| 票を返す応答は `public, max-age=31536000, immutable`。**票を伏せた応答は `no-store`** |
| `GET /api/v1/anchors?limit=` | アンカーを新しい順に（既定 20、1〜100）。各 `heads` の `shard`・`height` で、ブロックの詳細へたどれる | `no-cache` |
| エラー（404 など）| | `no-store` |

- 封印済みのブロックは内容が変わらないので、確定した応答には `Cache-Control: public, max-age=31536000, immutable` を付ける
  （CDN でキャッシュできる）。ただし、**内容がこの先変わる応答には付けない**: 締切前の詳細は票が伏せられていて、締切後に中身が変わる
  ので、`immutable` にすると、票なしの版が CDN に 1 年残ってしまう。
- **`chain.reveal_ballots=after_close`**（要 `election.voting_closes_at`）: 締切前は、詳細に票の中身（`ballot_id`・`contest_id`・
  `candidate_id`）を含めず、ヘッダー（ハッシュ・Merkle 根・票数・署名など）だけを返す（`ballots_revealed: false`）。締切は、api の時計で判定する
  （締切ちょうどから公開。api の再起動は不要）。**ブロックのハッシュは票から再計算するので、このとき、`verifier verify` /
  `tally` も、締切後にしかできない**（締切前は「票が非公開のため、締切後に実行してください」と表示して、終了コード 4）。
  既定の `always` は、これまでどおり、常に公開。
- 手動の確認項目は [docs/manual_check_step14.md](docs/manual_check_step14.md)（自動の確認は `scripts/check/chain.sh`）。

## 選挙状態（選挙のスケジュール。scripts/election.sh）

選挙状態は `scheduled → open → closing → closed` の順にしか進まない（[CLAUDE.md](CLAUDE.md) の原則17・18、
[ADR 0019](docs/adr/0019-election-lifecycle.md)）。DB（`app.mode=memory` ではプロセス内）が正で、変更は
管理用のポート（`admin.bind`。既定 `127.0.0.1:18081`）か `scripts/election.sh` からだけ行う。

```
scripts/election.sh status                                              # 状態・期間・残り時間・シャードごとの未封印件数・最後のブロック・監査ログの直近 5 件
scripts/election.sh schedule --opens-at <RFC3339> --closes-at <RFC3339> # scheduled の間だけ、期間を設定する
scripts/election.sh open --now [--yes]                                  # scheduled -> open（確認あり。--yes で省略）
scripts/election.sh close --now [--yes]                                 # open -> closing（確認あり。締切の手続きは自動で進む）
```

- 開始時刻（`election.voting_opens_at`）に達すると、アンカーのリースを持つ sealer（`app.mode=memory` では
  api 内蔵のスケジューラ）が自動で `open` にする。終了時刻（`election.voting_closes_at`）に達すると、自動で
  `closing` にし、締切の手続き（待ち時間 → 全シャードのフラッシュ → 未封印 0 件の確認 → 変化があれば最終
  アンカー → `closed`）を行う。待ち時間は `election.state_cache_secs + api.request_timeout_secs`
  （各 api が状態を短時間キャッシュしていることと、処理中のリクエストを考慮したもの）。
- 投票を受け付けるのは、状態が `open` かつ「開始時刻 <= 現在時刻 < 終了時刻」のときだけ（原則18）。開始前・
  締切の手続き中・終了後は、それぞれ別のメッセージ（`labels.voting_not_started_message` /
  `voting_closing_message` / `voting_closed_message`）で拒否する。ログイン画面・進捗画面にも、
  期間と今の状態を表示する（`GET /api/v1/election-status`。認証不要）。
- 管理用のエンドポイントは、公開用のポート（`api.port`）には無い（ロードバランサーには公開用ポートだけを
  登録する前提）。トークンは `secrets/admin_token`（または環境変数 `APP__ADMIN__TOKEN`）。未設定なら、
  管理用リスナーは起動しない。

## 集計（`verifier tally` / `scripts/tally.sh`）

```
scripts/tally.sh                          # 稼働中の api（http://localhost:<api.port>）から取得して集計
scripts/tally.sh --api URL --out DIR      # api の URL・出力先（既定 out/tally）を変える
scripts/tally.sh --all                    # 選挙区ごとの表を、省略せず全件表示（既定は先頭 30 選挙区まで）
scripts/tally.sh --allow-interim          # closed になる前の中間集計を許す（app.env=dev のときだけ）
```

**集計の前に、必ず次を行い、どれかが通らなければ集計しない**（改ざんされたデータを集計しても意味がないため）:

1. チェーン全体の検証と、投票済み記録（participation）との突合 — 失敗したら中止（終了コード 3）。突合は、投票用紙ごとの
   件数の一致、`ballot_id` の重複、アンカーの確認。
2. 未封印の票が残っていないこと — 残っていれば、件数を表示して中止（終了コード 4）。`--allow-interim` でも通らない。
   残りの票は、締切の手続き（選挙状態 `closing`。[選挙状態](#選挙状態選挙のスケジュールscriptselectionsh)を参照）の中でだけ
   封印される（sealer の停止ではフラッシュしない）ので、`closed` になるのを待ってから、もう一度実行する。
3. 選挙状態（`scripts/election.sh status` で確認できる）が `closed` であること — それより前は、`--allow-interim` が
   なければ中止（終了コード 4）。`--allow-interim` 自体は `app.env=dev` のときだけ使える（それ以外では終了コード 2）。
   中間集計の漏洩を防ぐため。

**出力**: ターミナルに表形式で表示し、`out/tally/{日時（UTC。例 20260921T101500Z）}/` に CSV と JSON を出力する
（`out/` は `.gitignore` 済み。すでにあるディレクトリ・ファイルは上書きしない）。

| 内容 | 出力 |
|---|---|
| 選挙区ごとの候補者別得票数（多い順。同数は同順位、0 票の候補者も表示）・白票（候補者とは別の行）・合計・投票済み者数 | 表示、`districts.csv`（選挙区の合計）、`candidates.csv`（候補者別）、`tally.json` |
| 都道府県別の合計 | 表示、`prefectures.csv`、`tally.json` |
| 選挙の種類別の合計 | 表示、`types.csv`、`tally.json` |
| 突合の結果（シャード・ブロック・票数、投票用紙ごとの一致、重複、アンカー） | 表示、`reconciliation.csv`、`tally.json` |

- 表示名は、seed（選挙区・候補者・政党・選挙の種類・都道府県）と、設定 `labels.ballot_item`（表の見出し）・`labels.blank_name`
  （白票の行・列の名前。既定「白票」）から取る。
- **白票**は、票の `candidate_id` が予約値 `blank` の票（[白票](#白票)を参照）。候補者とは別に数え、表では候補者の後の、順位の
  無い別の行に、CSV では `districts.csv` などの白票の列に出す（`candidates.csv` には入れない）。有効票（候補者への票）+ 白票 = 合計。
- チェーンに、選挙データに無い投票用紙の票や、投票用紙の候補者でも白票でもない票（API は受け付けないので、選挙データの取り違えか
  不具合）があれば、集計を中止する（終了コード 3）。
- **都道府県別**: 単独の都道府県の選挙区は、その都道府県へ。複数の都道府県にまたがる選挙区（合区・比例ブロック・全国）は、按分できないので、
  選挙区ごとに 1 行にする（「複数の都道府県」と表示）。
- **投票済み者数**は選挙区ごとの値。都道府県別・選挙の種類別の「投票済み数」は、投票用紙の枚数の合計（同じ有権者が複数の
  投票用紙に投票するので人数ではない。秘密投票のため、重複を除いた人数は出せない）。
- 終了コード: 0 = 集計した / 1 = 実行時エラー / 2 = 使い方の誤り（`--allow-interim` を `app.env=dev` 以外で指定した場合を含む） /
  3 = 検証・突合の失敗（集計せず） / 4 = まだ実行できない（未封印・選挙状態が `closed` ではない。`chain.reveal_ballots=after_close`
  の締切前で、票が非公開のときも）。

## 設定

設定値は `config/` 配下の TOML に集約する（[CLAUDE.md](CLAUDE.md) の原則 11、[ADR 0011](docs/adr/0011-config-crate.md)）。
読み込みと検証は [crates/app-config](crates/app-config) が行い、api / sealer / verifier / bench / スクリプトはすべてここから読む
（web は、ビルド時に必要な値 `labels.*` だけを受け取る）。

| ファイル | 内容 |
|---|---|
| [config/default.toml](config/default.toml) | **全項目と既定値**（各項目に日本語のコメント）。バイナリに埋め込まれる（編集したら再ビルド）|
| [config/dev.toml](config/dev.toml) | 開発用（`app.env=dev`）。default との差分だけ（封印間隔 10 秒など）|
| [config/production.example.toml](config/production.example.toml) | 本番用の例。`config/production.toml` にコピーして使う（`app.env=production` では必須）|
| `config/local.toml` | 手元だけの上書き（**gitignore**）|
| `secrets/` | 秘密情報。1 ファイル 1 値（**gitignore**）: `secrets/session_secret`、`secrets/sealer_signing_seed`、`secrets/admin_token` |

**優先順位（後のものが勝つ）**: `default.toml` → `config/<app.env>.toml` → `config/local.toml` → `secrets/` → 環境変数。
環境変数は `APP__<セクション>__<項目>`（大文字。例: `APP__SEAL__MAX_BALLOTS=5`、`APP__DB__NODES=host1:9042,host2:9042`）。
環境（`app.env`: `dev` / `production` / `test`）は、`APP__APP__ENV` > `config/local.toml` > 既定（`dev`）の順に決まる。
設定ファイルの場所は `APP_CONFIG_DIR`（既定 `config`）、`APP_SECRETS_DIR`（既定 `secrets`）で変えられる。

**秘密情報**（`session.secret`、`sealer.signing_seed`、`admin.token`）は、設定ファイルに書けない（書くと起動時にエラー）。
`APP__SESSION__SECRET` / `APP__SEALER__SIGNING_SEED` / `APP__ADMIN__TOKEN` か、`secrets/` のファイルで渡す。
`show` や `get`、ログ、エラーメッセージには値が出ない。

**不正な値**は、起動時に「どのファイル（環境変数）のどの項目がなぜ不正か」を、すべてまとめて表示して終了する
（未知の項目・型違い・範囲外・TOML の構文エラーを含む）。例:

```
設定が不正です（1 件）:
  - config/local.toml: seal.max_ballots = 0 は不正です（1 以上 4294967295 以下が必要です）
```

`auth.mode=db` は `app.mode=db` が必要（認証情報と名簿が DB にあるため）。
投票の受付期間（`election.voting_opens_at` / `voting_closes_at`）・選挙状態は「選挙状態」の節を参照。

```
cargo run -q -p app-config -- show          # 最終的に有効な設定（値の出所つき。秘密情報は ***）
cargo run -q -p app-config -- get api.port  # 1 つの値（スクリプト用。秘密情報は取得できない）
cargo run -q -p app-config -- validate      # 検証だけ（未実装の機能が指定されていたら失敗）
eval "$(cargo run -q -p app-config -- web-env)"   # 画面の文言（labels.*）を、ビルド用の環境変数として設定
```

主な項目（全項目は [config/default.toml](config/default.toml)）:

| 項目 | 既定 | 説明 |
|---|---|---|
| `app.env` / `app.mode` | `dev` / `memory` | 環境 / 保存先（`memory` = api 内のメモリ、`db` = DB。`db` では sealer は別プロセス）|
| `api.port` / `web.port` | 18080 / 8080 | api の待ち受けポート / 画面（trunk serve）のポート。`crates/web/Trunk.toml` と一致させる |
| `db.backend` / `db.nodes` / `db.keyspace` | `cassandra` / `["127.0.0.1:9042"]` / `vote` | DB の種類 / 接続先 / キースペース |
| `seal.max_ballots` / `seal.interval_secs` / `seal.min_ballots_after_interval` | 100 / 600 / 10 | 封印のルール（原則9）: この件数ですぐに封印 / 前回の封印（または投票開始）からこの秒数以上経ち、かつ最小件数以上なら全件を封印（アンカーの判定の間隔も `interval_secs`。変化がなければアンカーは作らない）|
| `sealer.id` / `sealer.lease_ttl_secs` | 空（ランダム）/ 30 | sealer の識別名（一意に）/ リースの TTL（3 以上）|
| `shard.count` | 1 | シャード数。api と全 sealer で同じ値にする |
| `session.ttl_secs` | 3600 | セッションの有効秒数 |
| `auth.mode` / `auth.argon2.*` | `stub` / 19456 KiB・2 回・並列 1 | 認証方式（`stub` = 入力 ID をそのまま採用、`db` = 事前登録の ID とパスワード。要 `app.mode=db`）/ Argon2id のパラメータ |
| `credentials.*` | 出力しない・16 文字・10 文字 | credgen の設定: `output_file_enabled`（平文を CSV に出すか）/ `output_path`（既定 `secrets/credentials.csv`）/ `password_length`（8〜128）/ `login_id_length`（8〜32）|
| `election.seed_dir` / `election.election_id` | `seed` / `2026-general` | 選挙データのディレクトリと、読み込む選挙の ID（`<seed_dir>/<election_id>/`）。[選挙データ](#選挙データ)を参照 |
| `election.voting_opens_at` / `voting_closes_at` | 空 | 投票の開始（RFC 3339。**未実装**）/ 締切（RFC 3339。`verifier tally` の「締切後の集計か」の判定と、`chain.reveal_ballots=after_close` の公開の判定に使う。空だと `--allow-interim` なしでは集計できない。api は、締切後の投票を拒否しない）|
| `vote.allow_blank` | `true` | 白票を選べるか（[白票](#白票)）。選挙状態が `open` に移った時点の値を固定し、それ以降は設定を変えても使わない（原則19）|
| `chain.reveal_ballots` | `always` | ブロックの詳細で票の中身を公開するタイミング（`always` = 常に / `after_close` = `election.voting_closes_at` 以後だけ。要 `voting_closes_at`）。[ビューア](#ブロックチェーンのビューアchain)を参照 |
| `labels.*` | 現行の文言 | 画面・API のエラー・集計の文言（`site_title` / `done_message` / `login_heading` / `ballot_item`（既定「投票用紙」）/ `progress`（既定「{total}枚中{current}枚目」）/ `blank_option`（白票の選択肢。既定「白票（どの候補者にも投票しない）」）/ `blank_confirm`（白票の確認の文言。既定「白票として投票します。よろしいですか？」）/ `blank_name`（集計・ビューア・エラーでの白票の呼び名。既定「白票」））|

**旧来の環境変数名からの移行**（旧名は廃止した）:

| 旧 | 新 |
|---|---|
| `SESSION_SECRET` | `APP__SESSION__SECRET`（または `secrets/session_secret`）|
| `SEALER_SIGNING_SEED` | `APP__SEALER__SIGNING_SEED`（または `secrets/sealer_signing_seed`）|
| `STORAGE=memory` / `scylla` | `APP__APP__MODE=memory` / `db` |
| `API_PORT` | `APP__API__PORT` |
| `SCYLLA_NODES` / `SCYLLA_KEYSPACE` / `SCYLLA_URI` | `APP__DB__NODES` / `APP__DB__KEYSPACE` / `APP__DB__NODES` |
| `DB_BACKEND` | `APP__DB__BACKEND` |
| `SHARD_COUNT` | `APP__SHARD__COUNT` |
| `SEAL_MAX_BALLOTS` / `SEAL_MAX_INTERVAL_SECS` | `APP__SEAL__MAX_BALLOTS` / `APP__SEAL__INTERVAL_SECS`（あわせて `APP__SEAL__MIN_BALLOTS_AFTER_INTERVAL` が増えた）|
| `SEALER_ID` / `SEALER_LEASE_TTL_SECS` | `APP__SEALER__ID` / `APP__SEALER__LEASE_TTL_SECS` |
| `SESSION_TTL_SECS` | `APP__SESSION__TTL_SECS` |
| `ELECTION_SEED_PATH`（`election.json` のパス）| `APP__ELECTION__SEED_DIR`（ディレクトリ）+ `APP__ELECTION__ELECTION_ID`（選挙の ID）。旧 `seed/election.json` は廃止 |

`api.port` の既定は 8080 から 18080 に変わった（画面の proxy 先と揃えたため）。`DB_PORT`（docker compose が公開するホスト側のポート）は、
docker compose の変数で、設定ではない（既定は `db.nodes` の先頭のポート）。

## 選挙データ

47 都道府県・複数の選挙の種類・大量の候補者を扱える構成。選挙ごとに `seed/<election_id>/` の下に置く
（読む選挙は `election.seed_dir` と `election.election_id`）。旧 `seed/election.json` は廃止した。

```
seed/2026-general/
  election.toml                    選挙の定義（id・名前）と、選挙の種類（code・表示名・表示順 order・投票方式 method）
  districts.csv                    district_id, election_type, name, prefectures（`;` 区切りの 2 桁コード）, order
  candidates/<election_type>.csv   candidate_id, district_id, name, party, profile
  voters.csv                       voter_id, districts（`;` 区切りの district_id。有権者ごとの、属する選挙区のリスト）
```

CSV は表計算ソフトで編集できる（UTF-8・ヘッダ行あり・列の順序は問わない）。`seed/2026-general/` は手書きの小さなサンプル
（有権者 `alice` = 東京 1 区、`carol` = 東京 2 区、`bob` = 大阪 1 区、`dave` = 鳥取県）。

**ID 体系**（CLAUDE.md の原則 13。文字種は小文字の英数字・`_`・`-`。区切りは `.`（選挙区・候補者）と `/`（投票用紙）。読み込み時に、形式と最大長を検証する）:

| ID | 形式 | 例 | 最大長 |
|---|---|---|---|
| `election_id` | 選挙の単位 | `2026-general` | 32 |
| `election_type` | 選挙の種類のコード | `shugiin_smd` `shugiin_pr` `sangiin_district` `sangiin_pr` `governor` `pref_assembly` `municipal_head` `municipal_assembly` `supreme_court_review` | 32 |
| `district_id` | 選挙区（先頭のセグメントが選挙の種類。都道府県は JIS X 0401 の 2 桁）| `shugiin_smd.13.01`（東京 1 区）| 64 |
| `contest_id` | `{election_id}/{district_id}`（投票用紙 1 枚）| `2026-general/shugiin_smd.13.01` | 97 |
| `candidate_id` | `{district_id}.c{連番}`（連番の桁数は固定しない）。予約値 `blank`（白票）は、選挙データの候補者には使えない（読み込み時にエラー）| `shugiin_smd.13.01.c3` | 80 |

- 合区のように 1 つの選挙区が複数の都道府県にまたがる場合も、**ID は変えず**、選挙区の属性 `prefectures`（リスト）で持つ
  （例: `sangiin_district.31_32` の `prefectures` は `31;32`）。区割りの変更などで将来変わり得る意味は、ID に埋め込まない。
- 投票方式は `enum`（今は `single_choice` だけ）。候補者の氏名・政党・略歴は属性（将来の候補者詳細画面で使う）。
- 投票用紙の**表示順**は、選挙の種類の `order`、次に選挙区の `order`。**投票する順番は固定**で、利用者は選べない
  （API の `ballot-status` が、有権者に関係する投票用紙だけを表示順に返し、画面は先頭の未投票へ自動で進む）。
- 有権者は、`voters.csv` の名簿にある選挙区の投票用紙にだけ投票できる（対象外は 403）。名簿に無い ID はログインできるが、
  投票用紙は 1 枚もない。`auth.mode=stub` では、名簿は、メモリ・DB のどちらのモードでも、起動時にこのファイルから読む（読み取り専用のキャッシュ）。
  `auth.mode=db` では、名簿は DB の `voter_roll`（credgen が voters.csv から登録する。内部の voter_id → 選挙区）を、リクエストごとに読む。

**不正なデータは、読み込み時（api の起動時・`seedgen --check`）に、ファイル・行・原因つきで検出する**（ID の重複、存在しない
選挙区・選挙の種類の参照、不正な ID・都道府県コード、列の不足、同じ種類の選挙区が複数ある有権者、候補者のいない選挙区など）:

```
選挙データが不正です（1 件）:
  - seed/2026-general/candidates/governor.csv:11: 候補者 governor.99.c1 が、存在しない選挙区 governor.99 を参照しています
```

**ダミーデータの生成** `seedgen`（47 都道府県規模・候補者 1 万人以上。生成後に同じ検証を通す）:

```
cargo run -q -p seedgen -- --out /tmp/seed-big --prefectures 47 --districts-per-pref 6 \
    --candidates-per-district 8 --voters 2000        # /tmp/seed-big/2026-general/ に生成
APP__ELECTION__SEED_DIR=/tmp/seed-big APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api
cargo run -q -p seedgen -- --check seed              # 既存のデータ（手で編集した CSV など）の検証だけ
```

有権者は `voter-1` … `voter-N`（`--voter-prefix` で変更）。都道府県数・小選挙区の数・候補者数・有権者数のほか、
`--municipalities-per-pref`・`--pref-assembly-districts-per-pref`・`--seed`（同じ種なら同じデータ）・`--force` も指定できる。

**API**（有権者に関係する投票用紙だけが見える）: `GET /api/v1/ballot-status`（表示順・固定）、
`GET|POST /api/v1/contests/{election_id}/{district_id}/candidates|vote`（対象外は 403）。旧 `GET /api/v1/status` は廃止した。
エラー応答は `error`（コード）と `message`（`labels.ballot_item` から作る文言）。

### 白票

有権者は、候補者の代わりに**白票**（どの候補者にも投票しない）を選べる。

- **画面**: 候補者一覧の最後に、白票の選択肢（`labels.blank_option`）を、候補者と区切って置く。確認画面では、候補者の名前の型
  （「「○○」に投票します」）ではなく、`labels.blank_confirm`（「白票として投票します。よろしいですか？」）を表示する。
- **API**: `POST /api/v1/contests/{election_id}/{district_id}/vote` の `candidate_id` に、予約値 `"blank"` を指定する（小文字の
  完全一致。`"BLANK"` などは存在しない候補者として 422 `invalid_candidate`）。`GET …/candidates` は、候補者の一覧（白票は含めない）と、
  白票を選べるか（`allow_blank`）を返す。白票でも、その投票用紙は投票済みになる（再投票は 409）。
- **白票を使わない選挙**: `vote.allow_blank = false` にすると、画面に白票の選択肢を出さず、API も 422 `blank_not_allowed` で拒否する。
  この値は選挙のルールなので、選挙状態が `open` に移った時点で、open に移したプロセス（db モードは sealer、memory モードと
  `open --now` は api）の設定の値を選挙状態（DB の `election_state.allow_blank`。memory モードはプロセス内）に固定し、それ以降は
  設定を変えて再起動しても、固定した値を使う（食い違いは起動時に警告する。原則19・[ADR 0021](docs/adr/0021-blank-vote.md)）。
- **チェーン・ビューア・集計**: 票の `candidate_id` に `blank` がそのまま入る（ブロックの形式は変わらない）。ビューアは、白票を
  `labels.blank_name` で、候補者と区別できる書式で表示し、ブロックの「投票先別の票数」でも候補者の後の別の行にする。集計は上記の
  [集計](#集計verifier-tally--scriptstallysh)を参照。
- この列を追加する前に作った DB のキースペースは、`ALTER TABLE <keyspace>.election_state ADD allow_blank boolean;` を一度実行するか、
  `scripts/db_reset.sh --all` で作り直す。

**チェーンの形式（版 2）**: 票の `contest_id` / `candidate_id` は文字列で、票の正規化バイト列は
`ballot_id(16) ‖ len(2) ‖ contest_id ‖ len(2) ‖ candidate_id`（[ADR 0013](docs/adr/0013-string-ids-and-chain-format-v2.md)）。
DB のスキーマも文字列（`text`）になった。旧いスキーマ・旧いチェーンとは互換性がないので、既存のキースペース（手動で使う `vote` など）は、
`DROP KEYSPACE` して `docs/schema.cql` を投入し直す（接続時に、旧いスキーマなら、その手順つきで失敗する）。

## 性能計測

`scripts/bench.sh` が、DB・sealer・api を構成ごとに起動して負荷をかけ、結果を集計する（ツール本体は `crates/bench`）。
結果と分析は [docs/benchmark.md](docs/benchmark.md)。サーバ側は速度最適化の `perf` プロファイル（`target/perf`）でビルドする。

```
scripts/bench.sh run   --configs "4:2:2,8:2:4" --out bench/results/x   # 構成 = シャード数:sealer 数:api 台数
scripts/bench.sh store --shards "1,4,8"         --out bench/results/x   # api を介さない DB 直接の計測
scripts/bench.sh report --out bench/results/x                           # 集計し直す（tables.md）
```

本番相当の封印設定（`seal.max_ballots=100`、`seal.interval_secs=600`、`seal.min_ballots_after_interval=10`）では、ドレイン
（どのシャードの未封印も 10 件未満になる = 時間ではもう封印されない、まで待つ）に
1 構成あたり約 10 分かかる。Cassandra のヒープは計測用に 4G / 1G（`CASSANDRA_MAX_HEAP` / `CASSANDRA_HEAP_NEW`。ペアで指定）にする。
短縮した設定での動作確認は `scripts/check/core.sh`（性能計測ツールの節）。
投票する有権者と投票用紙は、`seedgen` で生成した選挙データの名簿から作る（`BENCH_VOTERS`（既定 20000）人 × 9 枚の投票計画。尽きたら止まる）。

## 検証

すべてワークスペースのルートで実行する。

```
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
scripts/check_all.sh        # 上記に加えて、scripts/check/*.sh の全スイートを順番に実行し、結果と所要時間の表を出す
```

確認は、機能ごとの 6 つのスイート（`scripts/check/{core,chain,election,auth,web,docs}.sh`）に分かれている。各スイートの
完了条件はスクリプト自身にあり、exit 0 で終わることが完了の定義（対応表は [docs/testing.md](docs/testing.md)）。
共通処理（設定・DB・専用キースペース・選挙データ・起動停止・アサーション）は `scripts/lib/common.sh` に集約されている。

| スイート | 内容 | 目安の時間 |
|---|---|---|
| `core.sh` | api の起動・`domain::seal_policy` の単体テスト・設定（ファイルの反映・環境変数の優先・秘密情報・不正な設定での起動失敗・`labels.*` の web への反映）・性能計測ツール一式（`bench.sh`）・白票（投票 → 封印 → tally の白票の数・`vote.allow_blank=false` での拒否） | 約 5 分（Docker が必要） |
| `chain.sh` | `verifier demo`・封印ポリシー（トリガー・verify・改ざん検出）・「更新がなければ追加しない」・DB 永続化とクラッシュ復旧・複数 sealer のリース引き継ぎ・`verifier tally`（集計）・ブロックチェーンのビューア API・封印ルール（原則9: 最小件数・close --now での締切の封印） | 約 8〜9 分（Docker が必要） |
| `election.sh` | 投票フロー（ログイン・状態・候補者・投票・再投票拒否・並列・対象外・秘密投票）・47 都道府県規模の選挙データ（生成・表示範囲・投票順・壊れたデータの検出）・選挙状態の遷移と投票の受付期間（schedule → 自動 open → 自動 closing → closed・期間の境界・締切直前の票の封印・公開用ポートと管理用リスナーの分離） | 約 1.5 分 |
| `auth.sh` | credgen（ID・パスワードの事前登録）・DB 認証・`db_reset.sh` | 約 1.5 分（Docker が必要） |
| `web.sh` | 画面遷移ロジック（flow）・純粋性と依存方向・wasm 向け clippy・デザイントークン・テーマ・`trunk build --release`・白票（選択肢・確認の文言・ビューアの別の行） | 数秒〜数十秒 |
| `docs.sh` | 旧来の呼び名・環境変数名・封印ルールの旧名が残っていないこと、全スクリプトの構文（`bash -n`） | 1 秒未満 |

`core.sh` と `chain.sh`、`auth.sh` は Docker（Compose プラグイン）が必要で、DB を起動する。DB の起動に失敗したときは、
コンテナのログの FATAL / ERROR 行をそのまま表示して終了する。使う DB は `db.backend`（既定 `cassandra`。ScyllaDB は
`APP__DB__BACKEND=scylla scripts/check/chain.sh` のように指定する）。

**テストの分離**: DB を使う各スイートは、実行ごとに専用のキースペース（`vote_chain4_<UNIXTIME>_<PID>` など）を作って
api / sealer / verifier / 統合テストに `APP__DB__KEYSPACE` などで渡し、終了時（成功でも失敗でも）に `DROP` する。
`KEEP_KEYSPACE=1` なら残して名前を表示する（失敗の調査用。不要になったら `DROP KEYSPACE <名前>`）。DB のコンテナは
起動していなければ起動するだけで、停止（`STOP_DB=1` のときだけ）もボリュームの破棄もしない。手動で使うキースペース
`vote` には一切触れない。`chain.sh` は、DB を使う複数の確認（永続化・複数 sealer・集計）で、DB の前提確認とスキーマの
冪等性の確認（2 回流しても成功する）を最初の 1 回にまとめている（2 つめ以降の専用キースペースは、投入は 1 回でよい）。
`core.sh` の性能計測と `bench.sh` は、別の Compose プロジェクト `vote-bench`（ポート 19042）で DB を動かす。
infra-scylla の統合テストは `#[ignore]` で、DB 起動後に `cargo test -p infra-scylla -- --ignored` で実行する
（接続先は `db.nodes` の先頭。`APP__DB__NODES` で変えられる）（テストごとに専用のキースペースを作って DROP する）。

**設定の分離**: 各スイートは、手元の `config/local.toml`・`secrets/`・環境変数 `APP__…` の影響を受けないよう、
設定を分離して実行する（既定値 + スイートが渡す `APP__…` だけ。`APP__DB__BACKEND` だけは引き継ぐ）。

チェーンの検証ツール:

```
cargo run -q -p verifier -- demo                                   # ダミー票で検証と改ざん検出を実演
cargo run -q -p verifier -- verify --api http://localhost:18080    # 稼働中の API から取得して検証（全シャード・突合・アンカー）
scripts/tally.sh                                                   # 検証・突合に成功したときだけ集計（上の「集計」を参照）
```
