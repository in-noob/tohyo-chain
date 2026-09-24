//! API サーバ。ハンドラ・設定・状態の組み立てを提供し、`main.rs` が起動を担う。

pub mod admin;
mod auth;
pub mod chain_view;
pub mod config;
#[cfg(feature = "dev-tools")]
mod debug;
mod election_gate;
pub mod error;
pub mod routes;
pub mod state;

pub use chain_view::RevealPolicy;
pub use config::Config;
pub use election_gate::ElectionGate;
pub use routes::app;
pub use state::{ApiLabels, AppState, Built, SystemClock, build};
