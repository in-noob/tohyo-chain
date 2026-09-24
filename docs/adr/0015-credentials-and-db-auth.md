# 0015: ログイン ID・パスワードの事前登録（credgen）と DB 認証・DB のリセット

## 背景
認証は `StubAuthenticator`（入力 ID を voter_id として採用）だけだった（原則 7）。郵送した ID とパスワードでのログインに
対応するため、事前登録の仕組み、パスワードのハッシュ保存、ID の存在を推測させないログイン、開発中に投票データを消す手段が要る。
`auth.mode=db`（要 `app.mode=db`）だけを対象とし、memory モードは stub のまま。

## 決定
- **テーブル**（`docs/schema.cql`）:
  `credentials(login_id PK, password_hash, voter_id)` — ログインに必要な 3 つだけ。
  `voter_roll(voter_id PK, districts list<text>)` — `auth.mode=db` の api が名簿として使う（リクエストごとに読む。
  voters.csv のキャッシュは stub のときだけ）。
  `voter_registry(external_id PK, voter_id, login_id)` — credgen だけが使う台帳（名簿の ID → 内部 voter_id・現在のログイン ID）。
  スキップ / 再発行の判定に使う。api は読まない。
- **ID の分離**: 内部の `voter_id` は 16 バイトの乱数（32 桁の hex）で、ログイン ID とも名簿の ID（`voter-1`）とも無関係。
  再発行でも voter_id は変えない（participation が voter_id を持つので、投票済みの記録が保たれる）。
  秘密投票（原則 1）に影響しない: participation と ballot_pool はこれまでどおり結合できない。
- **パスワード**: Argon2id（`argon2` 0.6.0。パラメータは `auth.argon2.*`。PHC 文字列にパラメータを埋め込むので、後からパラメータを
  変えても既存のハッシュを照合できる）。ソルトは 16 バイトの乱数。平文は DB・ログ・`Debug` に出さない（`Credentials` と
  `LoginRequest` の `Debug` は伏せる）。
- **乱数と文字**: ログイン ID とパスワードは、`rand` の OS 乱数から、`ABCDEFGHJKLMNPQRSTUVWXYZ23456789`（32 文字。0/O・1/I/l を除く）
  で作る。32 = 2^5 なので、下位 5 ビットを取れば剰余バイアスがない。ログイン ID の一意性は LWT（`INSERT ... IF NOT EXISTS`）で保証し、
  衝突したら作り直す（最大 20 回）。ログイン入力は、大文字化し、空白とハイフンを除いて照合する（書き写しの揺れを許す）。
- **登録の順序**（途中で落ちても再実行で回復できるように）: 名簿 → ハッシュ → credentials（LWT）→ 台帳 → 再発行なら古い credentials を削除。
- **ログインの失敗を区別させない**: 常に DB を 1 回引き（形式が不正な ID は、存在しない ID として扱い、同じ回数だけ引く）、
  Argon2id の照合を**ちょうど 1 回**行う（存在しない ID では、起動時に設定と同じパラメータで作ったダミーのハッシュで）。
  失敗はすべて同じ `InvalidCredentials` → 401 `unauthorized`、同じ文言（ID の有無を含まない）。
  照合は CPU を使うので、api では `spawn_blocking` で行い、tokio のワーカーを塞がない。
  確認スクリプトは、応答時間が 0.5〜2 倍に収まることを見る（厳密な定数時間の保証ではない）。
- **マイナンバー**: `LoginRequest.my_number` は API と画面の項目として存在するだけ。`Credentials.my_number` に載るが、認証には使わず、
  ログにも出さず、保存しない。
- **CSV**: `credentials.output_file_enabled=true` のときだけ、`output_path` に `login_id,password,都道府県,選挙区` を出す。
  `create_new` + 権限 0600 で作る（すでにあれば上書きしない。作成時から 0600）。CSV は DB に書く前に作る（作れなければ、平文を
  渡す手段がないまま登録してしまうことを避ける）。`false` のときは警告して、`--confirm-no-output` がなければ何もしない（終了コード 2）。
  都道府県は、居住地（選挙区のうち、都道府県数が最も少ないもの）、選挙区は、表示順の全選挙区名を `;` で連結する。
- **db_reset.sh**: `--votes`（既定）は participation・ballot_pool・blocks・anchors・sealer_lease を `TRUNCATE`。`credentials`・`voter_roll`・
  `voter_registry`・`cluster_config`・`signer_keys` は残す。`--all` はキースペースを `DROP` してスキーマから作り直す。
  `app.env=production` は何も削除せずにエラー。件数を表示して確認（`--yes` で省略）。api / sealer が動いていたら拒否
  （sealer は、メモリ上のチェーンの先頭と DB が食い違うと分岐するため）。memory モードは案内だけ。
- **依存の追加**: `argon2` 0.6.0（追加時点の最新の安定版。RustCrypto のパスワードハッシュ。自前で書かない）。

## 理由
- 認証情報を、名簿・台帳と分けると、api が読むテーブルは最小になる（api は台帳を読まない）。
- 内部 voter_id を乱数にすれば、ログイン ID・名簿の ID が漏れても、participation の行を特定できない。
- 「1 回の DB 参照 + 1 回の Argon2 照合」を常に行う構造にすれば、分岐による応答時間の差が原理的に出にくい。

## 代替案
- ログイン ID を voter_id にする: 郵送物の ID から participation を引けるので不採用。
- 存在しない ID は照合を省いて即失敗: 応答時間で ID の有無が分かるので不採用。
- パスワードをハッシュの代わりに暗号化して保存: 平文を復元できる状態を作るので不採用。

## 対象外
ログイン失敗の回数制限・ロックアウト、パスワードの期限・変更、メール等での配布、ログイン画面の文言の labels 化。
