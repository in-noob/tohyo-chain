# データの作り方と置き場所

## どのデータがどこにできるか

| 場所 | 中身 | 作るもの | git |
|---|---|---|---|
| `seed/<election_id>/` | 選挙の定義（選挙の種類・選挙区・候補者・有権者名簿）。下の「[選挙データ](#選挙データseed)」 | 手で編集 / `seedgen` | 管理する |
| DB のキースペース（`db.keyspace`、既定 `vote`） | 投票・チェーン・認証情報・選挙状態（下の表） | api・sealer・credgen | — |
| `secrets/` | 秘密情報（[security.md](security.md#秘密情報)）と、credgen が出力する平文のパスワードの CSV `secrets/credentials.csv`（`credentials.output_path`） | 手で / credgen / `scripts/dev_up.sh` | 管理しない |
| `out/sample/` | 確認用のサンプルデータ: 選挙データ `out/sample/seed/` と、パターンごとの ID・パスワード・期待結果 `credentials_patterns.csv`（0600）（`sample.output_dir`） | `scripts/sample_data.sh` | 管理しない |
| `out/tally/<日時（UTC）>/` | 集計結果（CSV と `tally.json`）。既存のディレクトリは上書きしない | `scripts/tally.sh` | 管理しない |
| `logs/` | `scripts/dev_up.sh` が起動したプロセスのログ（`api.log`・`sealer.log`・`trunk.log`）。PID は `.dev/pids` | `scripts/dev_up.sh` | 管理しない |
| `bench/results/<名前>/` | 性能計測の結果（[architecture-scaling.md](architecture-scaling.md#ベンチマーク結果の読み方)） | `scripts/bench.sh` | 残すものだけ管理する |
| `crates/web/dist/` | 画面のビルド成果物 | `trunk build` | 管理しない |

`app.mode = "memory"` では、DB の代わりに api のメモリに持ち、再起動で消える（選挙データ・`secrets/` は同じ）。

### DB のテーブル

スキーマは [docs/schema.cql](schema.cql)（キースペース名は `{{KEYSPACE}}` のテンプレート）。

| 分類 | テーブル | 中身 |
|---|---|---|
| 投票 | `participation` | 投票済みの事実（voter_id × 投票用紙。投票先は持たない）。LWT で二重投票を防ぐ |
| | `slot_state` | 再投票の状態（キーは仮名 slot。voter_id は持たない） |
| | `ballot_pool` | 未封印の票（投票者を特定する列は持たない。受理時刻は分単位） |
| チェーン（公開） | `blocks` | 封印済みのブロック |
| | `anchors` | 全シャードの先頭をまとめた署名つきのアンカー |
| | `signer_keys` | ブロック署名の公開鍵（最初に登録された鍵だけが有効） |
| | `sealer_lease` | sealer のリース（シャードごと・アンカー担当） |
| | `cluster_config` | シャード数など、全プロセスで一致させる値 |
| 選挙状態 | `election_state` | 状態・期間・`open` で固定した選挙のルール（単一行） |
| | `election_audit` | 状態の変更履歴・再投票の鍵の破棄の記録 |
| 認証（`auth.mode=db`） | `credentials` | ログイン ID・パスワードの Argon2id ハッシュ・内部の voter_id |
| | `voter_roll` | 内部の voter_id → 投票できる選挙区 |
| | `voter_registry` | credgen の台帳（名簿の ID → 内部の voter_id・現在のログイン ID。api は読まない） |

## 準備から集計までの手順（`app.mode=db`）

各コマンドの詳しい使い方は [operations.md](operations.md)。手元で試すだけなら `scripts/dev_up.sh cassandra` が 3〜6 をまとめて行い、
確認用のパターンをまとめて作るなら `scripts/sample_data.sh` を使う。

1. **選挙データを編集する**: `seed/<election_id>/` の TOML・CSV を編集する（大規模なダミーは `seedgen` で生成）。
   **init の前にだけ**編集できる（理由と、後から変えたときに何が起きるかは [security.md](security.md#データを書き換えるときの決まり)）。
2. **検証する**: `cargo run -q -p seedgen -- --check seed`（ファイル・行・原因つきで、すべての誤りを表示する）。
3. **init（DB の初期化）**: DB を起動してスキーマを投入し（`scripts/db_reset.sh --all`、または `docker compose` の schema ジョブ）、
   api と sealer を初めて起動する。最初の接続で、シャード数と選挙定義のハッシュと署名鍵の公開鍵（`cluster_config`・`signer_keys`）と、
   設定の期間（`election.voting_opens_at` / `voting_closes_at`）が DB に登録され、ジェネシスブロックに選挙定義のハッシュが入る。以後は DB が正。
   この後に seed を変えると、api と sealer は起動を拒否する（`db_reset.sh --votes` の後も。選挙をやり直すなら `--all`）。
4. **期間を設定する**: 設定で渡していなければ、`scripts/election.sh schedule --opens-at <RFC3339> --closes-at <RFC3339>`
   （状態が `scheduled` の間だけ）。
5. **ID とパスワードを登録する**（`auth.mode=db`）: `APP__CREDENTIALS__OUTPUT_FILE_ENABLED=true cargo run -q -p credgen`。
   名簿（`voters.csv`）の有権者ごとに発行し、平文は `secrets/credentials.csv` にだけ出す（郵送の準備が済んだら削除する）。
6. **開始**: 開始時刻になると、アンカー担当の sealer が自動で `open` にする（手動なら `scripts/election.sh open --now`）。
   このとき選挙のルール（`vote.*`）が固定される。
7. **終了**: 終了時刻になると自動で `closing` になり、締切の手続きの後に `closed` になる（手動なら `scripts/election.sh close --now`）。
   `scripts/election.sh status` で `closed` を確認する。
8. **集計**: `scripts/tally.sh`（検証・突合に成功したときだけ集計する）。
9. **検証**: 誰でも `cargo run -q -p verifier -- verify --api <URL>` で、公開 API からチェーンを検証できる。

## 選挙データ（seed）

47 都道府県・複数の選挙の種類・大量の候補者を扱える構成。選挙ごとに `seed/<election_id>/` の下に置く
（読む選挙は `election.seed_dir` と `election.election_id`）。

```
seed/2026-general/
  election.toml                    選挙の定義（id・名前）と、選挙の種類（code・表示名・表示順 order・投票方式 method）
  districts.csv                    district_id, election_type, name, prefectures（; 区切りの 2 桁コード）, order
  candidates/<election_type>.csv   candidate_id, district_id, name, party, profile
  voters.csv                       voter_id, districts（; 区切りの district_id。有権者ごとの、属する選挙区のリスト）
```

CSV は表計算ソフトで編集できる（UTF-8・ヘッダ行あり・列の順序は問わない）。`seed/2026-general/` は手書きの小さなサンプル
（有権者 `alice` = 東京 1 区、`carol` = 東京 2 区、`bob` = 大阪 1 区、`dave` = 鳥取県）。

### ID 体系（原則13）

文字種は小文字の英数字・`_`・`-`。区切りは `.`（選挙区・候補者）と `/`（投票用紙）。読み込み時に、形式と最大長を検証する。

| ID | 形式 | 例 | 最大長 |
|---|---|---|---|
| `election_id` | 選挙の単位 | `2026-general` | 32 |
| `election_type` | 選挙の種類のコード | `shugiin_smd` `shugiin_pr` `sangiin_district` `sangiin_pr` `governor` `pref_assembly` `municipal_head` `municipal_assembly` `supreme_court_review` | 32 |
| `district_id` | 選挙区（先頭のセグメントが選挙の種類。都道府県は JIS X 0401 の 2 桁） | `shugiin_smd.13.01`（東京 1 区） | 64 |
| `contest_id` | `{election_id}/{district_id}`（投票用紙 1 枚） | `2026-general/shugiin_smd.13.01` | 97 |
| `candidate_id` | `{district_id}.c{連番}`（連番の桁数は固定しない）。予約値 `blank`（白票）は候補者に使えない | `shugiin_smd.13.01.c3` | 80 |

- 合区のように 1 つの選挙区が複数の都道府県にまたがる場合も、**ID は変えず**、選挙区の属性 `prefectures` で持つ
  （例: `sangiin_district.31_32` の `prefectures` は `31;32`）。区割りの変更などで将来変わり得る意味は、ID に埋め込まない。
- 投票方式は `enum`（今は `single_choice` だけ）。候補者の氏名・政党・略歴は属性。選挙区の中の候補者の表示順は、候補者コードの
  連番の数値順（`c2` < `c10`。CSV の行の順番には依存しない）。
- 投票用紙の**表示順**は、選挙の種類の `order`、次に選挙区の `order`。**投票する順番は固定**で、利用者は選べない
  （画面は先頭の未投票へ自動で進む）。
- 有権者は、`voters.csv` の名簿にある選挙区の投票用紙にだけ投票できる（対象外は 403）。名簿に無い ID はログインできるが、
  投票用紙は 1 枚もない。名簿は、`auth.mode=stub` では起動時に `voters.csv` から読み、`auth.mode=db` では credgen が登録した
  DB の `voter_roll` をリクエストごとに読む。

### 誤りの検出

不正なデータは、読み込み時（api の起動時・`seedgen --check`）に、ファイル・行・原因つきで検出する（ID の重複、存在しない
選挙区・選挙の種類の参照、不正な ID・都道府県コード、列の不足、同じ種類の選挙区が複数ある有権者、候補者のいない選挙区など）:

```
選挙データが不正です（1 件）:
  - seed/2026-general/candidates/governor.csv:11: 候補者 governor.99.c1 が、存在しない選挙区 governor.99 を参照しています
```

### ダミーデータの生成（seedgen）

47 都道府県規模（候補者 1 万人以上）も作れる。生成後に同じ検証を通す（[ADR 0014](adr/0014-seed-layout-and-seedgen.md)）。

```
cargo run -q -p seedgen -- --out /tmp/seed-big --prefectures 47 --districts-per-pref 6 \
    --candidates-per-district 8 --voters 2000        # /tmp/seed-big/2026-general/ に生成
APP__ELECTION__SEED_DIR=/tmp/seed-big scripts/dev_up.sh   # 生成したデータで起動する
cargo run -q -p seedgen -- --help                    # そのほかのオプション（--seed で再現可能・--voter-prefix など）
```

有権者は `voter-1` … `voter-N`。
