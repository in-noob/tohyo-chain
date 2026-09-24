//! 独立した sealer プロセスの設定。読み込みと検証は `app-config`（config/*.toml、secrets/、環境変数 `APP__…`）が行い、
//! ここではその結果を sealer が使う形に変換する。

use std::num::NonZeroU16;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use app_config::{AppConfig, Mode};
use domain::seal_policy::SealPolicy;
use domain::{ElectionRules, Period};
use shared_types::hex;

pub struct SealerConfig {
    /// `db.nodes`（`host:port`）
    pub nodes: Vec<String>,
    /// `db.keyspace`
    pub keyspace: String,
    /// `shard.count`。api と同じ値にする（食い違うと DB が接続を拒否する）。
    pub shard_count: NonZeroU16,
    /// `seal.max_ballots` / `seal.interval_secs` / `seal.min_ballots_after_interval`（アンカーの判定の間隔も同じ値）
    pub policy: SealPolicy,
    /// `sealer.signing_seed`（秘密情報。必須、64 桁の hex）。全 sealer で同じ値にする。
    pub signing_seed: [u8; 32],
    /// `sealer.id`（空ならランダム）。プロセスごとに一意にする。
    pub sealer_id: String,
    /// `sealer.lease_ttl_secs`（3 以上）
    pub lease_ttl: Duration,
    /// `election.voting_opens_at` / `voting_closes_at`（原則17。アンカーのリースを持つ sealer が、
    /// この期間で選挙状態を自動遷移させる）。
    pub period: Period,
    /// 締切の手続きの待ち時間（`election.state_cache_secs + api.request_timeout_secs`）。
    pub election_grace: Duration,
    /// open に遷移させるときに固定する選挙のルール（`vote.allow_blank`。原則19）。
    pub rules: ElectionRules,
}

// 署名鍵の種がログに出ないよう、Debug では伏せる。
impl std::fmt::Debug for SealerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealerConfig")
            .field("nodes", &self.nodes)
            .field("keyspace", &self.keyspace)
            .field("shard_count", &self.shard_count)
            .field("policy", &self.policy)
            .field("signing_seed", &"<redacted>")
            .field("sealer_id", &self.sealer_id)
            .field("lease_ttl", &self.lease_ttl)
            .field("rules", &self.rules)
            .finish()
    }
}

impl SealerConfig {
    /// 設定（config/*.toml、secrets/、環境変数）を読み込み、この版で実装済みの機能だけを使っていることまで確認する。
    pub fn load() -> anyhow::Result<Self> {
        let loaded = app_config::load()?;
        loaded.ensure_supported()?;
        Self::from_app(&loaded.config)
    }

    /// 読み込み済みの設定から、sealer の設定を作る。
    pub fn from_app(app: &AppConfig) -> anyhow::Result<Self> {
        if app.app.mode != Mode::Db {
            bail!(
                "独立した sealer は app.mode=db が必要です（memory は api プロセス内のタスクとして動きます。\
                 環境変数 APP__APP__MODE=db か、設定ファイルの [app] mode で指定してください）"
            );
        }
        let signing_seed = app
            .sealer
            .signing_seed
            .as_ref()
            .map(|seed| *seed.expose())
            .ok_or_else(|| {
                anyhow!(
                    "sealer.signing_seed（64 桁の hex）が未設定です（環境変数 APP__SEALER__SIGNING_SEED か \
                     secrets/sealer_signing_seed で、全 sealer で共通の値を渡してください）"
                )
            })?;
        let sealer_id = app
            .sealer
            .id
            .clone()
            .unwrap_or_else(|| format!("sealer-{}", hex::encode(&rand::random::<[u8; 4]>())));
        Ok(Self {
            nodes: app.db.nodes.clone(),
            keyspace: app.db.keyspace.clone(),
            shard_count: app.shard.count,
            policy: SealPolicy::new(
                usize::try_from(app.seal.max_ballots).context("seal.max_ballots が大きすぎます")?,
                app.seal.interval_secs,
                usize::try_from(app.seal.min_ballots_after_interval)
                    .context("seal.min_ballots_after_interval が大きすぎます")?,
            )
            .context(
                "seal.max_ballots / seal.interval_secs / seal.min_ballots_after_interval が不正です",
            )?,
            signing_seed,
            sealer_id,
            lease_ttl: Duration::from_secs(app.sealer.lease_ttl_secs),
            period: Period {
                opens_at: app.election.voting_opens_at.as_ref().map(|t| t.unix_secs),
                closes_at: app.election.voting_closes_at.as_ref().map(|t| t.unix_secs),
            },
            election_grace: Duration::from_secs(
                app.election.state_cache_secs + app.api.request_timeout_secs,
            ),
            rules: ElectionRules {
                allow_blank: app.vote.allow_blank,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";
    const BASE: [(&str, &str); 2] = [("app.mode", "db"), ("sealer.signing_seed", SEED)];

    fn config(pairs: &[(&str, &str)]) -> anyhow::Result<SealerConfig> {
        let loaded = app_config::load_for_test(pairs)?;
        SealerConfig::from_app(&loaded.config)
    }

    #[test]
    fn defaults() {
        let c = config(&BASE).expect("valid");
        assert_eq!(c.nodes, vec!["127.0.0.1:9042".to_string()]);
        assert_eq!(c.keyspace, "vote");
        assert_eq!(c.shard_count.get(), 1);
        assert_eq!(c.policy, SealPolicy::default());
        assert_eq!(c.lease_ttl, Duration::from_secs(30));
        assert_eq!(c.signing_seed, [7u8; 32]);
        assert!(c.sealer_id.starts_with("sealer-"), "{}", c.sealer_id);
        assert_eq!(c.rules, ElectionRules { allow_blank: true });
    }

    #[test]
    fn values_are_read_from_the_config() {
        let mut pairs = BASE.to_vec();
        pairs.extend([
            ("shard.count", "4"),
            ("seal.max_ballots", "50"),
            ("seal.interval_secs", "10"),
            ("sealer.id", "sealer-a"),
            ("sealer.lease_ttl_secs", "6"),
            ("db.nodes", "db1:9042,db2:9042"),
            ("db.keyspace", "vote_test"),
            ("vote.allow_blank", "false"),
        ]);
        let c = config(&pairs).expect("valid");
        assert_eq!(c.shard_count.get(), 4);
        assert_eq!(c.policy, SealPolicy::new(50, 10, 10).expect("valid"));
        assert_eq!(c.sealer_id, "sealer-a");
        assert_eq!(c.lease_ttl, Duration::from_secs(6));
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.keyspace, "vote_test");
        assert_eq!(c.rules, ElectionRules { allow_blank: false });
    }

    #[test]
    fn requires_db_mode_and_a_signing_seed() {
        let memory = config(&[("sealer.signing_seed", SEED)]).expect_err("default mode is memory");
        assert!(memory.to_string().contains("api プロセス内"), "{memory}");
        let no_seed = config(&[("app.mode", "db")]).expect_err("seed is required");
        assert!(
            no_seed.to_string().contains("APP__SEALER__SIGNING_SEED"),
            "{no_seed}"
        );
        // 形式の不正は、app-config が出所つきで弾く（値は含めない）。
        let bad = config(&[("app.mode", "db"), ("sealer.signing_seed", "abcd")])
            .expect_err("invalid seed");
        assert!(!bad.to_string().contains("abcd"), "{bad}");
    }

    #[test]
    fn invalid_values_are_errors() {
        for extra in [
            ("shard.count", "0"),
            ("seal.max_ballots", "0"),
            ("seal.interval_secs", "x"),
            ("sealer.lease_ttl_secs", "2"),
            ("db.keyspace", "a-b"),
        ] {
            let mut pairs = BASE.to_vec();
            pairs.push(extra);
            assert!(config(&pairs).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn debug_hides_the_signing_seed() {
        let c = config(&BASE).expect("valid");
        assert!(!format!("{c:?}").contains("0707"));
    }
}
