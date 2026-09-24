//! インメモリのポート実装。DB（ScyllaDB）導入までの代用で、プロセスのメモリにだけ状態を持つ。
//! そのため複数の API インスタンス間では状態が共有されない（水平スケールの検証は DB 導入後）。

pub mod election;
pub mod store;

pub use election::{StaticElectionRepository, StaticVoterRoll};
pub use store::InMemoryStore;
#[cfg(feature = "dev-tools")]
pub use store::{TamperError, TamperedAt};
