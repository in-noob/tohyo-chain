//! アプリケーション層: ユースケースとポート（トレイト）。
//!
//! IO の実装（DB・メモリ等）は infra-* クレートが、HTTP は api クレートが担う。

pub mod auth;
pub mod credentials;
pub mod ports;
pub mod session;
pub mod voting;

pub use auth::StubAuthenticator;
pub use credentials::{
    CredentialAdmin, CredentialError, CredentialRecord, CredentialStore, DbAuthenticator,
    InlineChecker, PasswordChecker, PasswordParams, RegistryEntry,
};
pub use ports::{
    AuthError, Authenticator, BallotIdSource, CastError, ChainRead, Clock, ContestCounts,
    Credentials, ElectionAuditEntry, ElectionRepository, ElectionStateSnapshot, ElectionStateStore,
    LeaseStore, SealStore, StoreError, SystemClock, VoteStore, VoterRoll,
};
pub use session::{SessionError, SessionSigner, SessionToken};
pub use voting::{BallotStatus, RandomBallotIds, ServiceError, VotingService};
