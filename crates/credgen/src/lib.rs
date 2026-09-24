//! 有権者ごとのログイン ID・パスワードの生成と、DB への事前登録。
//!
//! - ログイン ID とパスワードは、暗号論的乱数（OS 由来の種で初期化される `rand` の標準の生成器）で、
//!   見間違えやすい文字（`0/O`・`1/I/l`）を除いた 32 種類の文字（`application::credentials::ALPHABET`）から作る。
//! - DB に保存するのは、ログイン ID・パスワードのハッシュ（Argon2id）・内部用の `voter_id`（ランダムな 128 ビット。
//!   ログイン ID とは無関係）だけ。平文のパスワードは保存しない。
//! - 名簿の有権者ごとに、属する選挙区のリストを DB の名簿（`voter_roll`）にも登録する（`auth.mode=db` の api が引く）。
//! - 台帳（`voter_registry`）で「同じ有権者」を判定する。2 回目の実行では、既定でスキップ、`reissue` なら再発行
//!   （新しいログイン ID とパスワード。内部の `voter_id` は同じなので、投票済みの記録は保たれる）。

mod output;

use std::collections::HashMap;
use std::sync::Arc;

use application::credentials::ALPHABET;
use application::{
    CredentialAdmin, CredentialRecord, PasswordParams, RegistryEntry, StoreError, credentials,
};
use domain::ids::prefecture_name;
use domain::{DistrictId, Election, VoterId};
use shared_types::hex;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub use output::{CsvFileSink, NoSink, RecordSink};

/// 同時に処理する有権者の数（Argon2 は CPU とメモリを使うので、抑える）。
const CONCURRENCY: usize = 16;
/// ログイン ID が既存と衝突したときの、作り直しの最大回数。
const MAX_ID_ATTEMPTS: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum CredgenError {
    #[error("DB の操作に失敗しました: {0}")]
    Store(#[from] StoreError),
    #[error(transparent)]
    Credential(#[from] application::CredentialError),
    #[error(
        "ログイン ID が {MAX_ID_ATTEMPTS} 回続けて衝突しました（長さ credentials.login_id_length を増やしてください）"
    )]
    IdCollision,
    #[error("出力に失敗しました: {0}")]
    Output(#[from] std::io::Error),
    #[error("内部エラー: {0}")]
    Internal(String),
}

/// 暗号論的乱数で、`ALPHABET` の文字を `len` 個並べる。
///
/// `ALPHABET` は 32 種類なので、乱数 1 バイトの下位 5 ビットで、偏りなく選べる（剰余バイアスがない）。
pub fn generate_code(len: usize) -> String {
    (0..len)
        .map(|_| char::from(ALPHABET[usize::from(rand::random::<u8>() & 0x1f)]))
        .collect()
}

/// 内部用の `voter_id`: ランダムな 128 ビットの 16 進数（32 文字）。ログイン ID とも、名簿の有権者 ID とも無関係。
pub fn random_voter_id() -> Result<VoterId, CredgenError> {
    let bytes: [u8; 16] = rand::random();
    VoterId::new(&hex::encode(&bytes)).map_err(|e| CredgenError::Internal(e.to_string()))
}

/// 登録の設定。
#[derive(Debug, Clone)]
pub struct Options {
    /// 登録済みの有権者に、新しいログイン ID とパスワードを発行し直す（古い認証情報は削除する）。
    pub reissue: bool,
    pub login_id_length: usize,
    pub password_length: usize,
    pub params: PasswordParams,
}

/// 郵送用の 1 件（`credentials.output_file_enabled=true` のときに CSV へ出す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedRow {
    pub login_id: String,
    /// 平文のパスワード。ここにしか存在しない（DB にはハッシュだけ）。
    pub password: String,
    /// 居住地の都道府県名（`;` 区切り）。
    pub prefecture: String,
    /// 有権者に関係する選挙区の名前（表示順。`;` 区切り）。
    pub districts: String,
}

/// 実行の結果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// 新規に登録した有権者の数。
    pub created: usize,
    /// 再発行した有権者の数（`reissue`）。
    pub reissued: usize,
    /// 登録済みのためスキップした有権者の数。
    pub skipped: usize,
}

/// 郵送用の列: 都道府県名（居住地）と、選挙区名の一覧。
///
/// 居住地は、有権者の選挙区のうち、対象の都道府県が最も少ないもの（同数なら表示順で先）の都道府県。
/// 全国の選挙区（47 都道府県）や、複数県の比例ブロックより、その県の小選挙区・知事などが選ばれる。
pub fn mailing_columns(election: &Election, districts: &[DistrictId]) -> (String, String) {
    let mut contests: Vec<(usize, &domain::Contest)> = districts
        .iter()
        .filter_map(|d| election.contest_for_district(d))
        .filter_map(|c| Some((election.contest_position(&c.id)?, c)))
        .collect();
    contests.sort_by_key(|(position, _)| *position);
    let residence = contests
        .iter()
        .min_by_key(|(position, c)| (c.district.prefectures.len(), *position))
        .map(|(_, c)| &c.district.prefectures);
    let prefecture = residence
        .map(|codes| {
            codes
                .iter()
                .map(|code| prefecture_name(code).unwrap_or(code.as_str()))
                .collect::<Vec<_>>()
                .join(";")
        })
        .unwrap_or_default();
    let names = contests
        .iter()
        .map(|(_, c)| c.district.name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (prefecture, names)
}

/// 1 人分の結果。
enum Outcome {
    Skipped,
    Issued { row: IssuedRow, reissued: bool },
}

/// 名簿の有権者を登録する。`voters` は `(名簿の有権者 ID, 属する選挙区)`。出力（`sink`）は、DB に登録できた
/// 有権者から順に書く（途中で失敗しても、書いた分は、DB に登録済みのものだけ）。
pub async fn register(
    admin: Arc<dyn CredentialAdmin>,
    election: &Election,
    voters: Vec<(String, Vec<DistrictId>)>,
    options: &Options,
    sink: &mut dyn RecordSink,
) -> Result<Summary, CredgenError> {
    let semaphore = Arc::new(Semaphore::new(CONCURRENCY));
    let mut tasks: JoinSet<Result<(usize, Outcome), CredgenError>> = JoinSet::new();
    // 郵送用の列は、表示順（選挙の種類・選挙区）から作るので、ここで先に計算する（タスクには文字列だけ渡す）。
    let mut columns: HashMap<usize, (String, String)> = HashMap::new();
    for (index, (external_id, districts)) in voters.iter().enumerate() {
        columns.insert(index, mailing_columns(election, districts));
        let (admin, semaphore, options) = (admin.clone(), semaphore.clone(), options.clone());
        let (external_id, districts) = (external_id.clone(), districts.clone());
        let (prefecture, names) = columns[&index].clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|e| CredgenError::Internal(e.to_string()))?;
            let outcome = register_one(
                admin.as_ref(),
                &external_id,
                &districts,
                &options,
                prefecture,
                names,
            )
            .await?;
            Ok((index, outcome))
        });
    }

    let mut summary = Summary::default();
    let mut rows: Vec<(usize, IssuedRow)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (index, outcome) = joined.map_err(|e| CredgenError::Internal(e.to_string()))??;
        match outcome {
            Outcome::Skipped => summary.skipped += 1,
            Outcome::Issued { row, reissued } => {
                if reissued {
                    summary.reissued += 1;
                } else {
                    summary.created += 1;
                }
                rows.push((index, row));
            }
        }
    }
    // 出力は、名簿の順（決定的）。DB への登録がすべて終わってから、まとめて書く。
    rows.sort_by_key(|(index, _)| *index);
    for (_, row) in &rows {
        sink.write(row)?;
    }
    sink.finish()?;
    Ok(summary)
}

async fn register_one(
    admin: &dyn CredentialAdmin,
    external_id: &str,
    districts: &[DistrictId],
    options: &Options,
    prefecture: String,
    names: String,
) -> Result<Outcome, CredgenError> {
    let existing = admin.registry_get(external_id).await?;
    let (voter_id, previous_login) = match existing {
        // 登録済みで、再発行でない: 認証情報はそのまま。名簿（選挙区）だけ、今のデータに合わせる。
        Some(entry) if !options.reissue => {
            admin.put_roll(&entry.voter_id, districts).await?;
            return Ok(Outcome::Skipped);
        }
        Some(entry) => (entry.voter_id, Some(entry.login_id)),
        None => (random_voter_id()?, None),
    };

    // 名簿を先に書く（認証情報だけあって名簿が無い、という状態を作らない）。
    admin.put_roll(&voter_id, districts).await?;

    let password = generate_code(options.password_length);
    let params = options.params;
    // Argon2 は重い計算なので、専用のスレッドで行う（`password` は、クロージャに所有権ごと渡さず、複製を渡す）。
    let password_for_hash = password.clone();
    let password_hash = tokio::task::spawn_blocking(move || {
        credentials::hash_password(&password_for_hash, &params)
    })
    .await
    .map_err(|e| CredgenError::Internal(e.to_string()))??;

    // ログイン ID の一意性は、DB の LWT で守る（衝突したら別の ID で作り直す）。
    let mut login_id = String::new();
    let mut registered = false;
    for _ in 0..MAX_ID_ATTEMPTS {
        login_id = generate_code(options.login_id_length);
        let record = CredentialRecord {
            login_id: login_id.clone(),
            password_hash: password_hash.clone(),
            voter_id: voter_id.clone(),
        };
        if admin.insert_credential(&record).await? {
            registered = true;
            break;
        }
    }
    if !registered {
        return Err(CredgenError::IdCollision);
    }
    admin
        .upsert_registry(&RegistryEntry {
            external_id: external_id.to_string(),
            voter_id,
            login_id: login_id.clone(),
        })
        .await?;
    // 再発行: 古い認証情報を消す（古いログイン ID・パスワードは使えなくなる）。
    let reissued = previous_login.is_some();
    if let Some(old) = previous_login {
        admin.delete_credential(&old).await?;
    }
    Ok(Outcome::Issued {
        row: IssuedRow {
            login_id,
            password,
            prefecture,
            districts: names,
        },
        reissued,
    })
}
