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
    /// open に遷移させるときに固定する選挙のルール（`vote.*`。原則19）。
    pub rules: ElectionRules,
    /// 再投票の鍵のファイル（`<secrets>/revote_key`）。締切の手続きの中で破棄する（ADR 0022）。
    pub revote_key_path: Option<std::path::PathBuf>,
    /// 選挙データのディレクトリ（`<election.seed_dir>/<election.election_id>`）。選挙定義のハッシュを計算して、
    /// ジェネシスに入れ、DB と照合する（ADR 0025）。
    pub election_dir: std::path::PathBuf,
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
            .field("revote_key_path", &self.revote_key_path)
            .field("election_dir", &self.election_dir)
            .finish()
    }
}

impl SealerConfig {
    /// 設定（config/*.toml、secrets/、環境変数）を読み込み、この版で実装済みの機能だけを使っていることまで確認する。
    pub fn load() -> anyhow::Result<Self> {
        let loaded = app_config::load()?;
        loaded.ensure_supported()?;
        let mut config = Self::from_app(&loaded.config)?;
        config.revote_key_path = loaded.revote_key_path();
        Ok(config)
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
                allow_revote: app.vote.allow_revote,
                max_revotes: app.vote.max_revotes,
            },
            revote_key_path: None,
            election_dir: app.election.election_dir(),
        })
    }

    /// 選挙データ（seed）を読み込み、選挙定義のハッシュを計算する（ADR 0025）。
    pub fn election_hash(&self) -> anyhow::Result<domain::Hash32> {
        let election = seed::load_election(&self.election_dir).with_context(|| {
            format!(
                "選挙データ {} を読み込めません（election.seed_dir / election.election_id を確認してください）",
                self.election_dir.display()
            )
        })?;
        Ok(domain::election_definition_hash(&election))
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
        assert_eq!(
            c.election_dir,
            std::path::PathBuf::from("seed/2026-general")
        );
        assert_eq!(
            c.rules,
            ElectionRules {
                allow_blank: true,
                ..ElectionRules::default()
            }
        );
    }

    #[test]
    fn the_election_hash_is_computed_from_the_seed_and_an_unreadable_seed_is_an_error() {
        // テストはクレートのディレクトリで動くので、リポジトリの seed/ を指す。
        let mut pairs = BASE.to_vec();
        pairs.push(("election.seed_dir", "../../seed"));
        let c = config(&pairs).expect("valid");
        let expected = domain::election_definition_hash(
            &seed::load_election(&c.election_dir).expect("sample seed"),
        );
        assert_eq!(c.election_hash().expect("hash"), expected);

        pairs.push(("election.election_id", "no-such-election"));
        let err = config(&pairs)
            .expect("valid")
            .election_hash()
            .expect_err("missing");
        assert!(format!("{err:#}").contains("選挙データ"), "{err:#}");
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
        assert_eq!(
            c.rules,
            ElectionRules {
                allow_blank: false,
                ..ElectionRules::default()
            }
        );
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
