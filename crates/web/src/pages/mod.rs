//! 各画面。`flow` が返す値に従って描画・遷移するだけで、判断ロジックは持たない。

mod chain;
mod done;
mod login;
mod progress;
mod vote;

pub use chain::{ChainAnchorsPage, ChainBlockPage, ChainIndexPage, ChainShardPage};
pub use done::DonePage;
pub use login::LoginPage;
pub use progress::ProgressPage;
pub use vote::VotePage;
