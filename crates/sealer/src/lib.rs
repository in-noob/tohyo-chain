//! sealer: 未封印の票をポリシー（`domain::seal_policy`）に従ってブロックに封印する。
//!
//! ストアには `application::SealStore` ポート越しにアクセスするため、DB 実装に依存しない。
//! `app.mode=memory` では api プロセス内の tokio タスクとして [`spawn`] で動かす。

mod clock;
pub mod config;
mod coordinator;
mod runner;
mod schedule;
mod sealer;

pub use clock::{ManualClock, MonotonicClock, SystemMonotonic};
pub use coordinator::{ANCHOR_LEASE, Coordinator, LeaseConfig, StepOutcome};
pub use runner::{DEFAULT_TICK, SealerHandle, spawn, spawn_coordinator};
pub use schedule::AnchorSchedule;
pub use sealer::{FinalAnchor, LeaseGuard, SealEvent, Sealer, SealerError, TickOutcome, Trigger};
