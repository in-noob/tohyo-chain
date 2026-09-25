# 想定する環境

## 開発: ChromeOS の Linux 環境（aarch64）

手元の開発は、ChromeOS の Linux 開発環境（Crostini。aarch64）で行う。

- **DB は Cassandra を使う（ScyllaDB は動かない）。** この環境のカーネルは仮想アドレス空間が 39 ビットで、ScyllaDB は
  起動時に 32TiB の仮想アドレス空間を予約しようとして `mmap` が `ENOMEM` になり、`SIGABRT` で終了する（版や設定を変えても同じ。
  [ADR 0008](adr/0008-local-db-cassandra.md)）。Cassandra（JVM）は動くので、ローカルの既定は `db.backend = "cassandra"`。
  どちらも同じ `docs/schema.cql` と同じドライバ（`scylla` クレート）を使う。
- Cassandra での結果は、ScyllaDB の動作確認や性能の指標にはならない（LWT の実装・競合時の挙動が違う）。性能は、下の CI・本番の想定の環境で測る。
- Docker（Compose プラグイン）で DB を動かす。メモリは、Cassandra のヒープ（計測時は 4G）と、wasm のビルドの分を見込む。

## CI と本番の想定: x86_64 Linux・ScyllaDB

- **x86_64 Linux**（仮想アドレス空間 48 ビット以上）で、DB は **ScyllaDB**（`db.backend = "scylla"`）。構成は
  [architecture-scaling.md](architecture-scaling.md)。
- **NTP による時刻同期が必須。** 選挙の期間は秒単位（開始時刻ちょうどから受け付け、終了時刻ちょうどから拒否。原則18）で、
  api・sealer はそれぞれ自分の時計で判定する（封印の経過時間・締切の手続きの開始・締切後の票の公開も同じ）。
  時計がずれると、ノードごとに受付の判定が食い違い、封印時刻（分単位）の前後関係も崩れる。chrony などで同期し、ずれを監視する。
- ScyllaDB はノードを 3 台以上にし、レプリケーション係数 3 を想定する（[architecture-scaling.md](architecture-scaling.md)）。

## ツールと版

下の区間は `scripts/env_report.sh` の出力で、手で書かない。版の出どころは、[rust-toolchain.toml](../rust-toolchain.toml)
（Rust）、[crates/web/Trunk.toml](../crates/web/Trunk.toml) の `trunk-version`（Trunk）、`Cargo.lock`（依存クレート）、
`docker-compose.yml`（DB のイメージ）。版を上げるときは、それらのファイルを変えてから `scripts/env_report.sh --update` を実行する。
`scripts/check/docs.sh#6` が、再生成した内容とこの区間が一致することを確認する（手順どおりに入れていない環境では失敗する）。

<!-- env_report:begin（scripts/env_report.sh --update が書き換える。手で編集しない） -->

### Rust ツールチェーン（`rust-toolchain.toml`）

| 項目 | 値 |
|---|---|
| channel | `1.98.1` |
| components | rustfmt, clippy |
| targets | wasm32-unknown-unknown |
| edition（`Cargo.toml`） | 2024 |

### ツールの版（各ツールの `--version`）

| ツール | 出力 |
|---|---|
| rustc | `rustc 1.98.1 (48a229cea 2026-09-01)` |
| cargo | `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| rustfmt | `rustfmt 1.9.0-stable (48a229ceae 2026-09-01)` |
| clippy | `clippy 0.1.98 (48a229ceae 2026-09-01)` |
| trunk（`crates/web/Trunk.toml` の trunk-version） | `0.21.14` |
| trunk | `trunk 0.21.14` |

### ワークスペースの依存（`Cargo.toml` の `[workspace.dependencies]` の名前と、`Cargo.lock` にある版。間接依存の版も並ぶ）

| クレート | 版 |
|---|---|
| anyhow | 1.0.104 |
| argon2 | 0.6.0 |
| async-trait | 0.1.92 |
| axum | 0.8.9 |
| base64 | 0.22.1, 0.23.1 |
| console_error_panic_hook | 0.1.7 |
| csv | 1.4.0 |
| ed25519-dalek | 3.0.0 |
| gloo-net | 0.6.0, 0.7.0 |
| hmac | 0.13.0 |
| leptos | 0.8.20 |
| leptos_router | 0.8.15 |
| proptest | 1.11.0 |
| rand | 0.9.5, 0.10.2 |
| scylla | 1.9.0 |
| serde | 1.0.229 |
| serde_json | 1.0.151 |
| sha2 | 0.10.9, 0.11.0 |
| thiserror | 1.0.69, 2.0.20 |
| tokio | 1.53.1 |
| toml | 1.1.6+spec-1.1.0 |
| tower | 0.5.3 |
| tower-http | 0.7.1 |
| tracing | 0.1.44 |
| tracing-subscriber | 0.3.23 |
| ureq | 3.4.2 |
| web-sys | 0.3.105 |

### DB のコンテナイメージ（`docker-compose.yml`）

| サービス | イメージ |
|---|---|
| cassandra | `cassandra:5.0` |
| scylla | `scylladb/scylla:2026.2.7` |
| schema-cassandra | `cassandra:5.0` |
| schema-scylla | `scylladb/scylla:2026.2.7` |

<!-- env_report:end -->

### 入れ方

```
# Rust: rustup を入れておけば、リポジトリの中で最初に cargo を実行したときに、rust-toolchain.toml の版・
# コンポーネント（rustfmt・clippy）・ターゲット（wasm32-unknown-unknown）が自動で入る
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Trunk: 上の表の trunk-version と同じ版を入れる（--locked で依存の版も固定する）
cargo install trunk --version "$(sed -nE 's/^trunk-version = "(.*)"/\1/p' crates/web/Trunk.toml)" --locked

# 確認
scripts/env_report.sh --check
```

ほかに、ホストに必要なもの（版は固定しない。この機械の値は `scripts/env_report.sh --host` で表示できる）:

- Docker と Compose プラグイン（DB を使うモード・`scripts/check/{core,chain,auth}.sh`・`scripts/bench.sh`）
- bash・curl・coreutils・procps（`pgrep`）・util-linux（`setsid`）
- python3（`scripts/election.sh status` の整形表示、`scripts/check/election.sh`）
