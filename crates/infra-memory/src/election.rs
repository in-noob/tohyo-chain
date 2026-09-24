//! 選挙マスタと有権者名簿（読み取り専用）。起動時に `seed` クレートが読み込んだものを保持する。
//! 状態を持たない読み取りキャッシュなので、原則 6（api はステートレス）の例外として許される。

use std::collections::HashMap;
use std::sync::Arc;

use application::{ElectionRepository, StoreError, VoterRoll};
use async_trait::async_trait;
use domain::{DistrictId, Election, VoterId};

/// 起動時に読み込んだ選挙マスタを保持する（読み取りキャッシュ。原則6の例外）。
#[derive(Debug, Clone)]
pub struct StaticElectionRepository {
    election: Arc<Election>,
}

impl StaticElectionRepository {
    pub fn new(election: Election) -> Self {
        Self {
            election: Arc::new(election),
        }
    }
}

#[async_trait]
impl ElectionRepository for StaticElectionRepository {
    async fn election(&self) -> Result<Arc<Election>, StoreError> {
        Ok(self.election.clone())
    }
}

/// 起動時に読み込んだ有権者名簿（有権者 → 属する選挙区のリスト）を保持する（読み取りキャッシュ）。
#[derive(Debug, Clone, Default)]
pub struct StaticVoterRoll {
    districts: Arc<HashMap<VoterId, Vec<DistrictId>>>,
}

impl StaticVoterRoll {
    pub fn new(districts: HashMap<VoterId, Vec<DistrictId>>) -> Self {
        Self {
            districts: Arc::new(districts),
        }
    }

    pub fn len(&self) -> usize {
        self.districts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.districts.is_empty()
    }
}

#[async_trait]
impl VoterRoll for StaticVoterRoll {
    async fn districts_of(&self, voter: &VoterId) -> Result<Option<Vec<DistrictId>>, StoreError> {
        Ok(self.districts.get(voter).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roll_returns_the_voters_districts_or_none() {
        let alice = VoterId::new("alice").expect("valid");
        let district = DistrictId::new("governor.13").expect("valid");
        let roll = StaticVoterRoll::new(HashMap::from([(alice.clone(), vec![district.clone()])]));
        assert_eq!(roll.len(), 1);
        assert_eq!(roll.districts_of(&alice).await, Ok(Some(vec![district])));
        let bob = VoterId::new("bob").expect("valid");
        assert_eq!(roll.districts_of(&bob).await, Ok(None));
    }
}
