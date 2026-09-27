# tohyo-chain — 投票システム プロトタイプ

Web 投票システムのプロトタイプ。水平スケールできる API と、ハッシュチェーン（簡易ブロックチェーン）による
改ざん検知を検証する。

- 有権者は、事前に配られた ID とパスワードでログインし、自分の選挙区の投票用紙に、決まった順番で投票する（白票・投票期間中の再投票は設定で選べる）
- 票は、投票者と切り離した形（秘密投票）でシャードごとのハッシュチェーンに封印され、署名つきのアンカーで全体がまとめられる
- 誰でも、公開のビューアと `verifier` でチェーンを検証でき、集計（`tally`）は検証と突合に成功したときだけ行われる

守るべき原則は [CLAUDE.md](CLAUDE.md)、設計判断の記録は [docs/adr/](docs/adr/)。

## 3 分で試す

前提: Rust（[rust-toolchain.toml](rust-toolchain.toml) の版を rustup が自動で入れる）と Trunk。詳しくは [docs/environment.md](docs/environment.md)。

```
scripts/dev_up.sh                    # api（メモリに保存・sealer 内蔵）と画面を起動する。初回は wasm のビルドで数分かかる
scripts/election.sh open --now --yes # 選挙状態を open にする（起動直後は scheduled で、投票を受け付けない）
```

1. http://localhost:8080 を開き、ID `alice`（パスワードは不要。開発用のスタブ認証）でログインして、表示される 9 枚の投票用紙に
   順番に投票する（東京 1 区の有権者なので、関係する投票用紙だけが決まった順に出る）
2. ログアウトして、ID `bob` で 1 枚以上投票する（未封印の票が 10 件以上になり、10 秒経つとブロックに封印される）
3. ヘッダの「ブロックチェーン」で、封印されたブロックと票を見る
4. 別のターミナルで検証する: `cargo run -q -p verifier -- verify --api http://localhost:18080`
5. 改ざんを検出させる: `curl -s -X POST http://localhost:18080/debug/tamper` の後に、もう一度 4 を実行する（NG になる）

止めるときは `scripts/dev_down.sh`。DB（Cassandra）とパスワード認証で動かすときは `scripts/dev_up.sh cassandra`
（[docs/operations.md](docs/operations.md)）。

## ドキュメント

| 文書 | 内容 |
|---|---|
| [docs/environment.md](docs/environment.md) | 想定する環境（開発: ChromeOS の Linux / CI・本番: x86_64 Linux）と、必要なツールとその版 |
| [docs/configuration.md](docs/configuration.md) | 設定ファイルの種類と優先順、全項目の説明・既定値・変更できるタイミング |
| [docs/data-setup.md](docs/data-setup.md) | どのデータがどこにできるか、選挙データの形式と、準備から集計までの手順 |
| [docs/operations.md](docs/operations.md) | 起動・停止、選挙状態の操作、DB のリセット、サンプルデータ、集計・検証、よくある失敗への対処 |
| [docs/security.md](docs/security.md) | 大事にすること、秘密情報の管理、データを書き換えるときの決まり、既知の限界 |
| [docs/architecture-scaling.md](docs/architecture-scaling.md) | 構成（CDN・ロードバランサー・api・sealer・DB）と拡張の仕方、ボトルネック、ベンチマークの読み方 |
| [docs/features.md](docs/features.md) | 機能の詳細: 画面とテーマ、白票、再投票、ブロックチェーンのビューアと API、チェーンの形式 |
| [docs/testing.md](docs/testing.md) | 確認スイート（`scripts/check/*.sh`）がそれぞれ何を確認しているか |
| [docs/adr/](docs/adr/) | 設計判断の記録（背景・決定・理由・代替案） |
| [docs/manual_check_step5.md](docs/manual_check_step5.md) ほか | 画面をブラウザで操作する手動の確認項目（投票画面・ビューア・テーマ） |

## ライセンス

このリポジトリのコードは [Apache License 2.0](LICENSE) で公開している。誰でも利用・改変・再配布できる（商用も可）。
再配布するとき（改変したもの・組み込んだものを含む）は、[LICENSE](LICENSE) と [NOTICE](NOTICE) を同梱し、NOTICE の出典表示を残すこと。

### 依存するソフトウェアのライセンス

- Rust の依存クレート（`Cargo.lock` に記載のもの）は、すべて MIT・Apache-2.0 などの寛容なライセンス（2026 年 9 月に確認）。
  このリポジトリは依存を名前と版で参照するだけで、そのコードは含まない。ビルドした成果物（特に、ブラウザに配布される `crates/web` の wasm）を
  配布するときは、依存クレートの著作権表示とライセンス文も添えること（`cargo about` などで一覧を作れる）。
- `docker-compose.yml` が参照する DB のコンテナイメージは、このリポジトリに含まれず、使うときに各提供元から取得される。
  - Cassandra（`cassandra`。ローカル開発の既定）: Apache License 2.0
  - ScyllaDB（`scylladb/scylla`。`--profile scylla` を明示したときだけ使う）: 2025.1 以降はオープンソースではなく、ScyllaDB 独自の
    ソース公開型（source-available）ライセンス。使う前に [ScyllaDB のライセンスの FAQ](https://www.scylladb.com/source-available-faq/) で利用条件を確認すること。
    条件が合わない場合は Cassandra を使う（同じ `docs/schema.cql` で動く）。
