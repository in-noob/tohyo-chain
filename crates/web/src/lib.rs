//! 投票画面（Leptos CSR）。
//!
//! - [`flow`]: 画面遷移のロジック（純粋関数。通常の `cargo test` で検証する）
//! - [`chain`]: ブロックチェーンのビューア（`/chain` 以下）の表示ロジック（純粋関数）
//! - [`theme`]: テーマ（ライト / ダーク / OS に合わせる）の決定と、デザイントークンのコントラストのテスト
//! - [`error`]: API 失敗の種類と HTTP ステータスの写像（純粋）
//! - `api`: `/api/v1` を呼ぶクライアント（ブラウザの fetch）
//! - [`labels`]: 画面の文言（設定 `labels.*` で、ビルド時に変えられる）
//! - `app` / `pages`: ルータと各画面（`flow` の結果に従って描画・遷移するだけ）
//!
//! 依存してよいワークスペースクレートは `shared-types` のみ（原則5）。

mod api;
mod app;
pub mod chain;
pub mod election_status;
pub mod error;
pub mod flow;
pub mod labels;
mod pages;
pub mod theme;

pub use app::App;
