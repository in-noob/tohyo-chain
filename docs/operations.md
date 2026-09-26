# 運用手順

コマンドはすべてワークスペースのルートで実行する（`trunk` だけは `crates/web` で実行する）。スクリプトはどれも
`--help` で使い方を表示する。データの準備から集計までの流れは [data-setup.md](data-setup.md)、設定は [configuration.md](configuration.md)。

## 手元で起動・停止する（scripts/dev_up.sh・scripts/dev_down.sh）

api と画面（`trunk serve`）を一度に起動・停止する開発用のスクリプト。

```
scripts/dev_up.sh                        # memory（既定）: api（dev-tools 有効・sealer 内蔵）+ 画面。認証は stub
scripts/dev_up.sh cassandra              # 上記 + Cassandra（docker compose）。sealer は別プロセス。投票は DB に残る。認証は db
scripts/dev_up.sh cassandra --auth stub  # cassandra で、認証を stub（入力した ID をそのまま使う）にする
scripts/dev_down.sh                      # 停止
```

- ポートは画面 8080・api 18080（`web.port` / `api.port`）・管理用 127.0.0.1:18081（`admin.bind`）。`/api` は `trunk serve` の proxy が api へ転送する。
- 起動直後の選挙状態は `scheduled`（期間の設定が無いため）。投票を受け付けるには `scripts/election.sh open --now` か `schedule`。
- 起動が終わると、開く URL・ログインに使える ID・検証と改ざんデモのコマンド・ログの見方を表示する。
  ログは `logs/{api,sealer,trunk}.log`、PID は `.dev/pids`。
- **cassandra モード**は、既定のキースペース `vote` にスキーマを投入する（`IF NOT EXISTS`。既存のデータは残る）。
  **db 認証**では、DB の `credentials` が空なら credgen が名簿の有権者を登録し、ID とパスワードを `secrets/credentials.csv`（0600）に
  出す（前回の CSV は `.old` に退避）。登録済みなら何もしない。
- 秘密情報は、指定が無ければ**開発用の固定値**（`scripts/lib/common.sh` の `dev_default_secrets`。公開されているので本番では使わない）。
  `vote.allow_revote = true` で `secrets/revote_key` が無ければ乱数で作る。
- 改ざんデモの `POST /debug/tamper` は memory モードだけ（メモリ上のストアが対象）。cassandra モードでは `cargo run -q -p verifier -- demo`。
- `dev_down.sh` は api と sealer に SIGTERM を送る（sealer はリースを解放する）。未封印の票はフラッシュしない（原則9）。
  cassandra モードでは `docker compose stop` も行う（ボリュームは残す）。

## 個別に起動する

```
APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api --features dev-tools   # api（memory。sealer 内蔵）
cd crates/web && eval "$(cargo run -q -p app-config -- web-env)" && trunk serve            # 画面（別のターミナル）
cd crates/web && trunk build --release                                                     # 画面の配布用ビルド（dist/）
```

`app.mode=db` では、DB を起動してスキーマを投入し、api と sealer を別々に起動する（api は封印しない。sealer は複数起動できる）:

```
docker compose --profile cassandra up -d --wait cassandra     # 起動（healthy まで待つ）。ScyllaDB は cassandra → scylla
docker compose --profile cassandra run --rm schema-cassandra  # docs/schema.cql を投入（冪等。別名は SCHEMA_KEYSPACE=my_ks）
export APP__APP__MODE=db APP__SHARD__COUNT=4                  # シャード数は api と全 sealer で同じ値
export APP__SEALER__SIGNING_SEED=$(printf '07%.0s' {1..32})   # 署名鍵の種（開発用の値。全 sealer で同じ）
APP__SESSION__SECRET=dev-secret-0123456789abcdef cargo run -p api
APP__SEALER__ID=sealer-a cargo run -p sealer
APP__SEALER__ID=sealer-b cargo run -p sealer
docker compose --profile cassandra --profile scylla down -v   # 停止してデータも消す
```

docker compose が公開するポートは `DB_PORT`（既定 9042）で変えられる。コードの CQL はすべて `<keyspace>.<table>` の完全修飾名で、
接続時に `USE` はしない（[ADR 0010](adr/0010-test-isolation-per-run-keyspace.md)）。

## 選挙状態を操作する（scripts/election.sh）

選挙状態は `scheduled → open → closing → closed` の順にしか進まない（原則17・18・[ADR 0019](adr/0019-election-lifecycle.md)）。
管理用リスナー（`admin.bind`）にトークン（`APP__ADMIN__TOKEN` か `secrets/admin_token`。`app.env=dev` で指定が無ければ開発用の固定値）で接続する。

```
scripts/election.sh status                                              # 状態・期間・残り時間・シャードごとの未封印・最後のブロック・監査ログの直近 5 件
scripts/election.sh schedule --opens-at <RFC3339> --closes-at <RFC3339> # 期間を設定する（scheduled の間だけ）
scripts/election.sh open --now [--yes]                                  # scheduled → open（確認あり）
scripts/election.sh close --now [--yes]                                 # open → closing（締切の手続きは自動で進む）
```

- 開始時刻になると `open`、終了時刻になると `closing` に自動で移り、締切の手続きの後に `closed` になる。自動の遷移と締切の手続きは、
  アンカーのリースを持つ sealer（memory モードでは api 内蔵のスケジューラ）が行う。手続きの中身と待ち時間は
  [architecture-scaling.md](architecture-scaling.md#締切の手続きの待ち時間)。
- 投票を受け付けるのは、`open` かつ「開始時刻 <= 現在時刻 < 終了時刻」のときだけ。開始前・締切の手続き中・終了後は、それぞれ別の
  メッセージ（`labels.voting_*_message`）で拒否する。ログイン画面・進捗画面にも、期間と状態を表示する（`GET /api/v1/election-status`）。

## ID とパスワードを発行する（credgen）

`auth.mode=db`（要 `app.mode=db`）では、事前に登録した ID とパスワードだけがログインできる（[ADR 0015](adr/0015-credentials-and-db-auth.md)）。
有権者は `seed/<election_id>/voters.csv` から読む。

```
export APP__APP__MODE=db APP__AUTH__MODE=db
APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true cargo run -q -p credgen             # 新しい有権者だけ登録し、secrets/credentials.csv（0600）に出す
APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true cargo run -q -p credgen -- --reissue # 登録済みの有権者も再発行（古い ID・パスワードは無効）
cargo run -q -p credgen -- --confirm-no-output                                   # CSV に出さずに登録（平文はどこにも残らない）
```

- CSV の列は `login_id, password, 都道府県, 選挙区`。既存のファイルは上書きしない（別の出力先は `APP__CREDENTIALS__OUTPUT_PATH`）。
- `output_file_enabled=false` のときは「平文のパスワードは二度と取り出せません」と警告し、`--confirm-no-output` が無ければ何もしない（終了コード 2）。
- 2 回目の実行は既定でスキップ。`--reissue` でも内部の voter_id は変えないので、投票済みの記録は保たれる。
- ID とパスワードは暗号論的乱数で、見間違えやすい文字（`0/O`・`1/I/l`）を除いた 32 文字から作る。ログイン時、大文字小文字・空白・ハイフンは無視する。
- ログイン API: `POST /api/v1/login` `{"login_id": "...", "password": "...", "my_number": "..."}`（`my_number` は受け取って破棄する）。

## DB をリセットする（scripts/db_reset.sh）

```
scripts/db_reset.sh            # = --votes: 投票済み記録・再投票の状態・票のプール・ブロック・アンカー・リースを削除（選挙の定義と認証情報は残す）
scripts/db_reset.sh --all      # キースペースを削除して、docs/schema.cql から作り直す（認証情報も消える）
scripts/db_reset.sh --yes      # 確認を省略
```

- 対象は設定の `db.keyspace`。`app.mode` も設定から読むので、`dev_up.sh cassandra` の DB を消すときは `APP__APP__MODE=db` を付ける。
- 削除する件数を表示して、`yes` と入力するまで消さない。api・sealer が動いていたら止めるよう促して終了する
  （動いている最中に消すと、sealer が持っているチェーンの状態と DB が食い違うため）。
- `app.env=production` では何もせずにエラー、`app.mode=memory` では「再起動するとリセットされます」と表示して終了する。

## 確認用のサンプルデータを作る（scripts/sample_data.sh）

開発環境の DB に、ユーザー単位の確認パターン（P01〜P13）を一括で登録し、ID・パスワードと**期待結果**を CSV に出す
（`app.mode=db` 用。[ADR 0023](adr/0023-sample-data.md)）。

```
scripts/dev_down.sh                                            # api / sealer が動いていたら止める（動いていると拒否される）
APP__APP__MODE=db scripts/sample_data.sh                       # = --phase open: 投票期間中（今から sample.open_hours 時間）
APP__APP__MODE=db scripts/sample_data.sh --phase before        # 投票の開始前
APP__APP__MODE=db scripts/sample_data.sh --phase closed        # 投票の終了後（事前の投票の後に close --now で締め切る）
APP__ELECTION__SEED_DIR=out/sample/seed scripts/dev_up.sh cassandra   # 作ったデータを画面で確認する
```

既存のスクリプトと CLI を順に使う: `db_reset.sh --all` → `seedgen`（32 都道府県・各 1 選挙区・候補者 2 人。32 は参議院の合区が
現れる最小の数）→ api・sealer の起動 → `election.sh schedule` → `credgen`（P13 は `--reissue`）→ API での事前の投票 → 封印 →
CSV の出力 → api・sealer の停止。

| ID | パターン | 事前の状態 |
|---|---|---|
| P01 | 通常（未投票、投票用紙が複数） | 9 枚すべて未投票 |
| P02 | 投票用紙が1枚だけ | 最高裁判所裁判官国民審査（全国）だけ |
| P03 | 合区の選挙区に属する | 参議院選挙区が「鳥取県・島根県選挙区（合区）」 |
| P04 | 投票できる投票用紙がない | 名簿の選挙区が空（ログインはできる） |
| P05 | 一部の投票用紙だけ投票済み | 1 枚目だけ投票済み |
| P06 | すべて投票済み | 9 枚すべて投票済み |
| P07 | 白票で投票済み | 1 枚目に白票（`vote.allow_blank=false` では作成不可） |
| P08 | 再投票済み（A→B） | 1 枚目を候補者 1 → 候補者 2 にやり直し済み（`vote.allow_revote=false` では作成不可） |
| P09 | 再投票の上限に到達 | 1 枚目を `vote.max_revotes` 回やり直し済み（同上） |
| P10 | パスワード誤り | 正しいログイン ID と、誤ったパスワード |
| P11 | 存在しないID | 形式は正しいが、登録されていないログイン ID |
| P12 | IDの形式が不正 | 空白と記号を含むログイン ID |
| P13 | 再発行済み | 2 行: 再発行前の ID とパスワード（失敗）/ 再発行後の ID とパスワード（成功） |

- 出力: `<sample.output_dir>/credentials_patterns.csv`（既定 `out/sample/`。平文のパスワードを含む。0600）。列は
  `pattern_id, pattern_name, login_id, password, 都道府県, 選挙区, 投票用紙の数, 期待結果_ログイン, 期待結果_投票, 期待結果_再投票, 備考`。
  最後に、パスワードを除いた要約表を表示する。`--phase before` では P05〜P09 は作れないので「作成不可（開始前のため）」と書く。
- **期待結果は、実際の選挙状態・`open` で固定したルール・期間から計算する**。セルは「<操作> → <結果>」の形で、そのまま手順になる
  （例 `2枚目に候補者1で投票 → 成功（201）`、`1枚目を候補者2でやり直し → 拒否（409 revote_limit_reached）`）。
  試す順は **ログイン → 再投票 → 投票**（投票すると未投票が投票済みに変わり、再投票の結果が変わるため）。確認で状態が変わるので、
  やり直すときはスクリプトを再実行する。
- `scripts/check/auth.sh#2` が、3 つの phase で実行し、CSV の各行を実際に試して期待結果と一致することを確認する（CSV がそのままテスト仕様）。

## 集計する（scripts/tally.sh）

```
scripts/tally.sh                          # 稼働中の api（http://localhost:<api.port>）から取得して集計
scripts/tally.sh --api URL --out DIR      # api の URL・出力先（既定 out/tally）を変える
scripts/tally.sh --all                    # 選挙区ごとの表を省略せず全件表示（既定は先頭 30 選挙区まで）
scripts/tally.sh --allow-interim          # closed になる前の中間集計を許す（app.env=dev のときだけ）
```

**集計の前に次を行い、どれかが通らなければ集計しない**（[ADR 0016](adr/0016-tally.md)）:

1. チェーン全体の検証（再投票のつながりを含む）と、投票済み記録との突合（投票用紙ごとの件数・`ballot_id` の重複・アンカー）。
   失敗したら終了コード 3。選挙データに無い投票用紙の票や、投票用紙の候補者でも白票でもない票があるときも 3。
2. 未封印の票が残っていないこと。残っていれば件数を表示して終了コード 4（残りは締切の手続きの中でだけ封印されるので、`closed` を待つ）。
3. 選挙状態が `closed` であること。それより前は `--allow-interim` が無ければ終了コード 4（中間集計の漏洩を防ぐ）。

出力はターミナルの表と、`out/tally/<日時（UTC。例 20260921T101500Z）>/` の CSV・JSON（上書きしない）:

| 内容 | 出力 |
|---|---|
| 選挙区ごとの候補者別得票数（多い順。同数は同順位、0 票も表示）・白票（候補者の後の別の行）・合計・投票済み者数 | 表示、`districts.csv`、`candidates.csv`、`tally.json` |
| 都道府県別の合計（複数の都道府県にまたがる選挙区は按分せず、選挙区ごとに 1 行） | 表示、`prefectures.csv`、`tally.json` |
| 選挙の種類別の合計 | 表示、`types.csv`、`tally.json` |
| 突合の結果（シャード・ブロック・票数、投票用紙ごとの一致、重複、アンカー） | 表示、`reconciliation.csv`、`tally.json` |
| 再投票の件数と変更の内訳（前の投票先 → 次の投票先。`closed` の後だけ） | 表示、`revotes.csv`、`tally.json` の `revotes` |

- 再投票は slot ごとに最後の票だけを数える。都道府県別・選挙の種類別の「投票済み数」は投票用紙の枚数の合計（人数ではない。
  秘密投票のため、重複を除いた人数は出せない）。
- 終了コード: 0 = 集計した / 1 = 実行時エラー / 2 = 使い方の誤り（`app.env=dev` 以外での `--allow-interim` を含む）/
  3 = 検証・突合の失敗 / 4 = まだ実行できない（未封印あり・`closed` ではない・`chain.reveal_ballots=after_close` の締切前で票が非公開）。

## 検証する（verifier）

```
cargo run -q -p verifier -- verify --api http://localhost:18080    # 稼働中の API から取得して検証（全シャード・突合・アンカー）
cargo run -q -p verifier -- verify --api URL --public-key HEX      # 署名の公開鍵を固定して検証（既定は API が返す値を使う）
cargo run -q -p verifier -- demo                                   # ダミー票で、検証と改ざん検出を実演（オフライン）
```

公開 API と、手元の選挙データ（設定の `election.seed_dir` / `election.election_id`。`APP__ELECTION__SEED_DIR` で変えられる）だけを
使うので、運用者以外の誰でも実行できる。全シャードのジェネシスの選挙定義のハッシュを、手元の選挙データと照合する（違えば NG・終了コード 3）。
公開鍵は、選挙の前に別の経路（公報など）で公開した値を `--public-key` で
渡すと、API を運用する側が鍵ごと差し替えた場合も検出できる。`chain.reveal_ballots=after_close` の締切前は、票が非公開なので
検証できない（終了コード 4）。

## 性能計測（scripts/bench.sh）

DB・sealer・api を構成ごとに起動して負荷をかけ、結果を集計する（ツール本体は `crates/bench`。速度最適化の `perf` プロファイルでビルドする）。
結果の読み方は [architecture-scaling.md](architecture-scaling.md#ベンチマーク結果の読み方)。

```
scripts/bench.sh run    --configs "4:2:2,8:2:4" --out bench/results/x   # 構成 = シャード数:sealer 数:api 台数
scripts/bench.sh store  --shards "1,4,8"         --out bench/results/x   # api を介さない DB 直接の計測
scripts/bench.sh report --out bench/results/x                            # 集計し直す（tables.md）
```

- 本番相当の封印設定（100 件 / 600 秒 / 10 件）では、ドレインに 1 構成あたり最長 約 10 分かかる。Cassandra のヒープは計測用に
  4G / 1G（`CASSANDRA_MAX_HEAP` / `CASSANDRA_HEAP_NEW`。ペアで指定）。
- DB は専用の Compose プロジェクト `vote-bench`（ポート 19042）で動かすので、手元の DB（`vote`）には触れない。
- 有権者と投票用紙は `seedgen` の名簿から作る（`BENCH_VOTERS`（既定 20000）人 × 9 枚。尽きたら止まる）。環境変数の一覧は `scripts/bench.sh --help`。

## よくある失敗への対処

これまでに実際に起きた事例。

| 症状 | 原因 | 対処 |
|---|---|---|
| ScyllaDB のコンテナが起動直後に `SIGABRT` で終了する（`mmap` が `ENOMEM`） | 仮想アドレス空間が 39 ビットの環境（ChromeOS の Linux など）では、ScyllaDB が起動時に予約する 32TiB を確保できない。版や設定では直らない | Cassandra を使う（`db.backend = "cassandra"`。ローカルの既定）。ScyllaDB は x86_64 の環境で使う（[environment.md](environment.md)） |
| `trunk serve` / `trunk build` が `could not find the root package of the target crate` で失敗する | ワークスペースのルート（仮想マニフェスト。パッケージが無い）で実行した。Trunk はカレントディレクトリの `Cargo.toml` からパッケージを探す | `cd crates/web` してから実行する（`index.html`・`Trunk.toml` もそこにある） |
| `trunk` が版の不一致で起動を拒否する | `crates/web/Trunk.toml` の `trunk-version` と、入っている Trunk の版が違う | [environment.md](environment.md#入れ方) の手順で、指定の版を入れ直す |
| `scripts/election.sh` が「admin.token が未設定です」で終了する | `app.env` が `dev` 以外で、トークンを渡していない（`dev_up.sh` が api に渡す値は、別のシェルには引き継がれない） | `APP__ADMIN__TOKEN` か `secrets/admin_token` で、api と同じ値を渡す（`app.env=dev` なら開発用の固定値を自動で使う） |
| `dev_up.sh cassandra` が「起動の完了を待つ」で必ず失敗していた | スクリプトが途中で `scripts/lib/common.sh` を読み直し、同じ名前の関数が置き換わっていた（修正済み） | `scripts/check/docs.sh#4` が、common.sh の読み込みが 1 回だけで、同じ名前の関数を定義していないことを確認する |
| api / sealer が「旧いスキーマ」「再投票に対応する前のスキーマ」で起動を拒否する | 表の形が変わる前に作ったキースペースを使っている（`ALTER TABLE` では直せない） | `scripts/dev_down.sh` → `APP__APP__MODE=db scripts/db_reset.sh --all` で作り直す |
| api / sealer が「選挙定義のハッシュ…が一致しません」で起動を拒否する。`verify` が NG・`tally` が終了コード 3 になる | init の後に seed（選挙・選挙区・候補者の名前・政党・略歴など）を書き換えた。または、別の選挙データ（`APP__ELECTION__SEED_DIR`）を読んでいる | seed を init の時点の内容に戻す（git なら `git diff seed/`）。`sample_data.sh` のデータなら `APP__ELECTION__SEED_DIR=out/sample/seed` を渡す。選挙をやり直すなら `db_reset.sh --all`（`--votes` では登録が消えない） |
| sealer / api が、シャード数や署名鍵の食い違いで起動を拒否する | `shard.count` か `sealer.signing_seed` が、DB に最初に登録された値と違う（init で固定） | 登録済みの値に合わせる。変えたいときは `db_reset.sh --all` で作り直す（投票データも消える） |
| 設定の検証で `auth.mode` について「db には app.mode=db が必要です」と出る | `config/dev.toml` や `config/local.toml` に `auth.mode = "db"` を書いた（memory モードでも読まれる） | 環境変数 `APP__AUTH__MODE` で渡す（`dev_up.sh --auth db`） |
| api が起動しない（再投票の鍵が無い） | `vote.allow_revote = true` なのに `secrets/revote_key` が無い（締切の手続きで破棄された後を含む） | 新しい選挙なら鍵を作る: `(umask 077; od -An -tx1 -N32 /dev/urandom \| tr -d ' \n' > secrets/revote_key)`。`dev_up.sh` は自動で作る |
| `db_reset.sh` / `sample_data.sh` が「api や sealer が動いています」で終了する | 動いている最中に DB を消すと、sealer の状態と DB が食い違う | `scripts/dev_down.sh` で止めてから実行する |
| 確認スイートが DB の起動で失敗する | Docker のデーモンが動いていない、またはコンテナが FATAL で終了した | スイートが表示するコンテナのログの FATAL / ERROR 行を見る。デーモンを起動する（`docker info` で確認） |
