# 構成と拡張（ロードバランサー・スケーリング）

## 全体の構成

```
 ブラウザ
   │  静的ファイル（crates/web/dist: index.html・wasm・css）
   ├──────────────▶ CDN
   │  /api/...（公開用のポートだけ）
   └──────────────▶ L7 ロードバランサー ──▶ api × N（ステートレス。api.port）
                      ・ヘルスチェック GET /healthz          │
                      ・スティッキーセッション不要           │ LOCAL_QUORUM / LWT
                      ・ログインにレート制限                 ▼
                                                  Cassandra / ScyllaDB（ノード × K）
                                                             ▲
            sealer × M（シャードのリースを持つものだけが封印）─┘
            管理用リスナー（admin.bind）← scripts/election.sh（内部ネットワークから）
```

| クレート | 役割 |
|---|---|
| `crates/domain` | ハッシュチェーン・Merkle 木・署名・封印ルール・受付の判定・選挙マスタと ID 体系（純粋なロジック。IO に依存しない） |
| `crates/application` | ユースケースとポート（トレイト）、セッショントークン、再投票の鍵 |
| `crates/api` | axum の HTTP API（公開用のポートと、管理用リスナー） |
| `crates/sealer` | 票をブロックに封印する。独立した実行ファイル（`app.mode=db`）と、api に内蔵するライブラリ（`app.mode=memory`） |
| `crates/infra-memory` / `crates/infra-scylla` | ストアの実装（メモリ / Cassandra・ScyllaDB） |
| `crates/app-config` | 設定の読み込みと検証（[configuration.md](configuration.md)） |
| `crates/seed` / `crates/seedgen` / `crates/credgen` | 選挙データの読み込みと検証 / ダミーデータの生成 / ID とパスワードの事前登録（[data-setup.md](data-setup.md)） |
| `crates/verifier` | チェーンの検証・集計の CLI（`demo` / `verify` / `tally`） |
| `crates/bench` | 性能計測ツール（`scripts/bench.sh`） |
| `crates/shared-types` / `crates/web` | API と画面で共有する型 / 投票画面とビューア（Leptos CSR + Trunk） |

## CDN（静的ファイル）

画面は `cd crates/web && trunk build --release` の `dist/` を配る静的ファイルだけで、サーバ側の描画は無い。CDN に置き、
`/api/` だけをロードバランサーへ転送すると、画面と API が同一オリジンになり CORS が要らない（開発では `trunk serve` の proxy が同じ役割）。

ビューアの API（`/api/v1/chains/...`）の、内容が確定した応答には `Cache-Control: public, max-age=31536000, immutable` が付くので、
CDN でキャッシュしてよい。締切前の票を伏せた応答・エラーは `no-store`、先頭を含む一覧は `no-cache`（[features.md](features.md#ブロックチェーンのビューア)）。
投票・ログインの API はキャッシュさせない。

## L7 ロードバランサー

- **公開用のポート（`api.port`）だけを登録する。** 管理用リスナー（`admin.bind`）は登録しない（下の「管理用のポート」）。
- **ヘルスチェックは `GET /healthz`**（`{"status":"ok"}`）。プロセスが応答できるかだけを見る（DB の状態は見ない。
  DB 障害を LB で切り離しても、全台が同じ DB を見ているので意味がないため）。
- **スティッキーセッションは不要。** api はステートレス（原則6）で、セッションは HMAC 署名トークン。どの api が受けても同じ結果になる。
  そのため、`session.secret` は全 api で同じ値にする。
- **ログインにレート制限を掛ける。** api 自身には、ログイン失敗の回数制限・ロックアウトが無い（[security.md](security.md#既知の限界)）。
  `POST /api/v1/login` は Argon2id の照合で CPU とメモリを使うので、送信元ごとの制限（例: 1 分あたり数回）を LB か WAF で行う。
  存在しない ID でも同じ計算をする（応答時間で ID の有無を区別させないため）ので、総当たりはそのまま CPU 負荷になる。

## api の水平スケール

- サーバのメモリに状態を持たない（例外は、候補者マスタの読み取りキャッシュと、選挙状態の短期キャッシュ `election.state_cache_secs`）。
  台数は負荷に合わせて増減してよい。
- 二重投票の防止は DB の LWT（`participation` への `INSERT ... IF NOT EXISTS`、再投票は `UPDATE ... IF seq = n`）で行うので、
  api が何台あっても 1 人 1 票が保たれる。

## Cassandra / ScyllaDB

- **ノードの追加**: DB の通常の手順で追加する（アプリ側は `db.nodes` に初期接続先を並べるだけで、残りのノードはドライバが見つける）。
- **レプリケーション**: `docs/schema.cql` は開発用の `SimpleStrategy`・レプリケーション係数 1。本番は、キースペースを作る前に
  `NetworkTopologyStrategy` の係数 3 に書き換えて投入する（1 台が落ちても QUORUM を保てる）。
- **一貫性レベル**: 書き込みと通常の読み取りは `LOCAL_QUORUM`。LWT（Paxos）で書いた行（`participation`・`blocks`）の読み取りは
  `SERIAL`。係数 3 なら、各操作に 2 台の応答が要る。

## sealer（シャードとリース）

- 票は `shard.count` 個のシャードに分かれ、シャードごとに独立したチェーンになる（原則9 の封印ルールもシャードごと）。
- sealer は、DB のリース（TTL つきの LWT）を取ったシャードだけを封印する。複数起動すると、シャードを分け合う。1 台が落ちると、
  `sealer.lease_ttl_secs` の後に残りが引き継ぐ。リースを失ったシャードは直ちに処理を止めるので、同じ高さのブロックが 2 つできる（分岐）ことはない。
- アンカー（全シャードの先頭を署名でまとめたもの）と選挙状態の自動遷移・締切の手続きは、**アンカーのリース**を持つ 1 台が行う。
- 封印の並列度の上限はシャード数（sealer をシャード数より多くしても、余りは待機するだけ）。`shard.count` は DB の初期化後に
  変えられない（init で固定）ので、想定する票数から先に決める。署名鍵（`sealer.signing_seed`）は全 sealer で同じ値にする。

## 管理用のポート

`admin.bind`（既定 `127.0.0.1:18081`）は選挙状態の変更（`schedule` / `open --now` / `close --now`）を受け付ける。
**外部に出さない**: ループバックか内部ネットワークだけで待ち受け、LB にも CDN にも登録しない。操作は、踏み台から
`scripts/election.sh` で行う。トークン（`admin.token`）が無ければリスナー自体が起動しない。

## 締切の手続きの待ち時間

終了時刻（または `close --now`）で状態が `closing` になっても、すぐには封印を締めない。待ち時間は
`election.state_cache_secs + api.request_timeout_secs`（既定 5 + 10 = 15 秒）:

- 各 api は選挙状態を最大 `state_cache_secs` 秒キャッシュしているので、その間は `open` と判断して票を受け付けうる
- 受け付けた処理中のリクエストは、最長 `request_timeout_secs` 秒で終わる

この 2 つを待てば、受理（201）した票はすべて票のプールに入っている。`closing` の間、各 sealer は持っているシャードを周期ごとに
フラッシュし続け（`trigger=close`。待ち時間の間に届いた票も封印する）、アンカー担当の sealer は、待ち時間が過ぎたら
再投票の鍵を破棄 → 未封印 0 件を確認 → 変化があれば最終アンカー → `closed` の順に進める。どちらかの設定を大きくしたら、待ち時間も延びる。

## ボトルネックになり得る箇所

| 箇所 | 理由 | 対策 |
|---|---|---|
| LWT（`participation`・再投票） | Paxos は通常の書き込みより往復が多く、同じパーティションへの同時の LWT は競合して再試行になる | 同じ有権者・同じ投票用紙への同時の送信は本来まれ。DB のノードと CPU を増やす。ScyllaDB は LWT の実装が Cassandra と違うので、ScyllaDB で測る |
| Argon2id（ログイン） | 1 回の照合で `auth.argon2.memory_kib`（既定 約 19 MiB）のメモリと CPU を使う（`spawn_blocking` で他のリクエストは止めない） | ログインのレート制限。ログインの山に合わせて api を増やす。パラメータを下げるのは安全性とのトレードオフ |
| sealer | 1 シャードの封印は 1 台の sealer が直列に行う（票の並べ替え・Merkle 木・署名・DB への書き込み） | シャード数を増やし、sealer を増やす。封印が受付に追いつかないと未封印が積み上がり、締切の手続きのフラッシュが長くなる |

## ベンチマーク結果の読み方

`scripts/bench.sh`（[operations.md](operations.md#性能計測scriptsbenchsh)）は、構成（シャード数:sealer 数:api 台数）ごとに、ウォームアップ →
定常負荷（固定レート）→ ドレイン → 飽和（クローズドループ）を測り、`<出力先>/<構成>/summary.json` と、集計表 `tables.md` を出す。

- **受付の性能（api + DB）と、封印の性能（sealer）を分けて読む。** 例: [bench/results/v1-saturation-first](../bench/results/v1-saturation-first)
  （初期の版・1 シャード・sealer 1 台・api 1 台）では、定常負荷の 約 2,050 件/秒 はすべて 201 で受理されたが、封印は 1 ブロック（100 件）に
  中央値 約 2.5 秒かかり、未封印が 77 万件まで積み上がってドレインが終わらなかった（`drain.drained = false`）。受付は足りていても、
  1 シャードでは封印が追いつかない、と読む。
- **`environment.txt` で、どこで測ったかを必ず確認する。** 開発環境（ChromeOS の Linux・aarch64・Cassandra 1 ノードを api・sealer・
  負荷生成と同じ機械の Docker で動かす）の数値は**参考値**。DB が CPU を奪い合い（上の例では DB だけで平均 3 コア強）、
  Cassandra と ScyllaDB は LWT の実装も違う。性能の判断は、x86_64 の ScyllaDB 複数ノードで、負荷生成を別の機械に置いて測った値で行う。
- 同じ条件（設定・データ・版）で比べる。`summary.json` の `config` に、封印の設定と構成が残る。
