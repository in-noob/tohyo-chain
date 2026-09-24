//! ScyllaDB（CQL）版のストア。`application` のポート（`VoteStore` / `ChainRead` / `SealStore`）を実装する。
//!
//! スキーマは `docs/schema.cql`（キースペース名を置換するテンプレート。このクレートは作成しない。
//! docker compose の schema ジョブや確認スクリプトが投入する。描画は [`schema`]）。
//!
//! すべての CQL はキースペース名付きの完全修飾テーブル名で書く（キースペース名は設定値から組み立てる）。
//! セッションで `USE <keyspace>` を使うと、Cassandra が「prepared statement と USE の併用はアンチパターン」
//! という警告を返すため。
//!
//! 秘密投票（原則1・3）のための設計:
//! - participation（誰がどの投票用紙に投票したか）と ballot_pool（票）は別テーブルで、結ぶキーがない。
//! - 票の書き込み時刻（WRITETIME）は分に丸めて指定する。participation は LWT で書くため
//!   `USING TIMESTAMP` を付けられず、マイクロ秒の書き込み時刻が残る。票側を分に丸めることで、
//!   書き込み時刻による突き合わせを防ぐ。

mod convert;
pub mod keyspace;
pub mod schema;
mod store;

pub use store::{ConnectError, ScyllaConfig, ScyllaStore};
