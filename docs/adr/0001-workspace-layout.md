# 0001: Cargo ワークスペースのクレート分割と依存方向

## 背景
投票システムのプロトタイプを、水平スケール可能な API とハッシュチェーンによる改ざん検知の検証用に作る。
秘密投票などの原則（CLAUDE.md）をコンパイル時の依存関係で守れる構成にしたい。

## 決定
`crates/` 配下に次の 8 クレートを置く（Step 0 では空。api のみ `GET /healthz` を実装）。

| クレート | 種別 | 役割 |
|---|---|---|
| shared-types | lib | API と画面で共有する型 |
| domain | lib | 純粋なドメインロジック（封印ポリシー等）。IO・async・DB に依存しない |
| application | lib | ユースケース層 |
| infra-memory | lib | インメモリ実装（DB 導入前の永続化層）|
| api | bin | axum による HTTP API。ステートレス |
| sealer | bin | ブロック封印ワーカー |
| verifier | bin | ハッシュチェーン検証 |
| web | lib | Leptos CSR 画面。shared-types 以外のワークスペースクレートに依存しない |

- ルート `Cargo.toml` は仮想ワークスペース（`resolver = "3"`）とし、依存バージョンは `[workspace.dependencies]` に集約する。
- `scripts/check_stepN.sh` を各ステップの完了条件とする。

## 理由
- 依存方向をクレート境界で固定すると、原則5（web と domain の分離）の違反が循環依存・未宣言依存としてビルド時に検出できる。
- 依存バージョンを一箇所で管理すると、クレート間のバージョン不整合を防げる。
- sealer と verifier は単独プロセスとして動かすため bin にした。

## 代替案
- 単一クレートでモジュール分割: 依存制約を強制できないため不採用。
- Cargo feature による層分離: 境界が曖昧になり、原則5の検査が難しいため不採用。
