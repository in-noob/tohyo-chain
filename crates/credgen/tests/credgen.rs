//! credgen のテスト。DB の代わりに、メモリ上の偽のストアを使う（`CredentialAdmin` / `CredentialStore`）。

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use application::{
    AuthError, Authenticator, CredentialAdmin, CredentialRecord, CredentialStore, Credentials,
    DbAuthenticator, InlineChecker, PasswordParams, RegistryEntry, StoreError,
};
use async_trait::async_trait;
use credgen::{
    CredgenError, CsvFileSink, IssuedRow, NoSink, Options, RecordSink, generate_code,
    mailing_columns, random_voter_id, register,
};
use domain::{
    Candidate, CandidateCode, District, DistrictId, Election, ElectionId, ElectionType,
    ElectionTypeCode, VoterId, VotingMethod,
};

/// 速く回るパラメータ。
const FAST: PasswordParams = PasswordParams {
    memory_kib: 8,
    iterations: 1,
    parallelism: 1,
};

fn options(reissue: bool) -> Options {
    Options {
        reissue,
        login_id_length: 10,
        password_length: 12,
        params: FAST,
    }
}

#[derive(Default)]
struct FakeDb {
    credentials: Mutex<HashMap<String, CredentialRecord>>,
    registry: Mutex<HashMap<String, RegistryEntry>>,
    roll: Mutex<HashMap<String, Vec<DistrictId>>>,
    /// 最初の N 回の `insert_credential` を、ログイン ID の衝突として扱う。
    collisions: AtomicUsize,
    always_collide: bool,
}

#[async_trait]
impl CredentialAdmin for FakeDb {
    async fn registry_get(&self, external_id: &str) -> Result<Option<RegistryEntry>, StoreError> {
        Ok(self
            .registry
            .lock()
            .expect("lock")
            .get(external_id)
            .cloned())
    }

    async fn insert_credential(&self, record: &CredentialRecord) -> Result<bool, StoreError> {
        if self.always_collide
            || self
                .collisions
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Ok(false);
        }
        let mut credentials = self.credentials.lock().expect("lock");
        if credentials.contains_key(&record.login_id) {
            return Ok(false);
        }
        credentials.insert(record.login_id.clone(), record.clone());
        Ok(true)
    }

    async fn delete_credential(&self, login_id: &str) -> Result<(), StoreError> {
        self.credentials.lock().expect("lock").remove(login_id);
        Ok(())
    }

    async fn upsert_registry(&self, entry: &RegistryEntry) -> Result<(), StoreError> {
        self.registry
            .lock()
            .expect("lock")
            .insert(entry.external_id.clone(), entry.clone());
        Ok(())
    }

    async fn put_roll(&self, voter: &VoterId, districts: &[DistrictId]) -> Result<(), StoreError> {
        self.roll
            .lock()
            .expect("lock")
            .insert(voter.as_str().to_string(), districts.to_vec());
        Ok(())
    }
}

#[async_trait]
impl CredentialStore for FakeDb {
    async fn find(&self, login_id: &str) -> Result<Option<CredentialRecord>, StoreError> {
        Ok(self
            .credentials
            .lock()
            .expect("lock")
            .get(login_id)
            .cloned())
    }
}

/// 出力を、メモリに集めるだけの出力先。
#[derive(Default)]
struct CollectSink(Vec<IssuedRow>);

impl RecordSink for CollectSink {
    fn write(&mut self, row: &IssuedRow) -> std::io::Result<()> {
        self.0.push(row.clone());
        Ok(())
    }

    fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn district(id: &str, name: &str, prefectures: &[&str], order: u32) -> District {
    let id = DistrictId::new(id).expect("valid");
    District {
        election_type: ElectionTypeCode::new(id.type_segment()).expect("valid"),
        name: name.to_string(),
        prefectures: prefectures.iter().map(|p| (*p).to_string()).collect(),
        order,
        id,
    }
}

/// 東京 1 区・比例東京ブロック・東京都知事・全国比例（東京と大阪）・鳥取島根の合区。
fn election() -> Election {
    let etype = |code: &str, order: u32| ElectionType {
        code: ElectionTypeCode::new(code).expect("valid"),
        name: code.to_string(),
        order,
        method: VotingMethod::SingleChoice,
    };
    let districts = vec![
        district("shugiin_smd.13.01", "東京1区", &["13"], 1),
        district("shugiin_pr.tokyo", "比例代表 東京ブロック", &["13"], 1),
        district("governor.13", "東京都知事", &["13"], 1),
        district(
            "sangiin_pr.national",
            "参議院比例代表（全国）",
            &["13", "27"],
            1,
        ),
        district(
            "sangiin_district.31_32",
            "鳥取県・島根県選挙区（合区）",
            &["31", "32"],
            1,
        ),
    ];
    let candidates = districts
        .iter()
        .map(|d| Candidate {
            id: CandidateCode::new(&d.id, 1).expect("valid"),
            name: "候補".to_string(),
            party: String::new(),
            profile: String::new(),
        })
        .collect();
    Election::new(
        ElectionId::new("2026-general").expect("valid"),
        "選挙".to_string(),
        vec![
            etype("shugiin_smd", 10),
            etype("shugiin_pr", 20),
            etype("sangiin_district", 30),
            etype("sangiin_pr", 40),
            etype("governor", 50),
        ],
        districts,
        candidates,
    )
    .expect("valid")
}

fn d(id: &str) -> DistrictId {
    DistrictId::new(id).expect("valid")
}

fn tokyo_districts() -> Vec<DistrictId> {
    // 名簿の並びは表示順とは限らない（郵送用の列は、表示順に直す）。
    vec![
        d("governor.13"),
        d("sangiin_pr.national"),
        d("shugiin_smd.13.01"),
        d("shugiin_pr.tokyo"),
    ]
}

fn voters(n: usize) -> Vec<(String, Vec<DistrictId>)> {
    (1..=n)
        .map(|i| (format!("voter-{i}"), tokyo_districts()))
        .collect()
}

// --- 乱数 ---

#[test]
fn generated_codes_use_only_the_unambiguous_alphabet() {
    for len in [8, 10, 16, 32] {
        let code = generate_code(len);
        assert_eq!(code.len(), len);
        assert!(
            code.bytes()
                .all(|b| application::credentials::ALPHABET.contains(&b)),
            "{code}"
        );
        for banned in ['0', 'O', '1', 'I', 'l', 'o'] {
            assert!(!code.contains(banned), "{banned} は使わない: {code}");
        }
    }
}

#[test]
fn generated_codes_are_unbiased_and_distinct() {
    // 32 種類の文字が、ほぼ均等に出る（剰余バイアスがない）。期待値 1000 回のところ、±20% に収まる。
    let mut counts: HashMap<char, usize> = HashMap::new();
    for _ in 0..1000 {
        for c in generate_code(32).chars() {
            *counts.entry(c).or_default() += 1;
        }
    }
    assert_eq!(counts.len(), 32, "32 種類すべてが出る");
    for (c, n) in &counts {
        assert!((800..=1200).contains(n), "{c}: {n}");
    }
    let unique: HashSet<String> = (0..2000).map(|_| generate_code(10)).collect();
    assert_eq!(
        unique.len(),
        2000,
        "10 文字（50 ビット）は、2000 個で衝突しない"
    );
}

#[test]
fn internal_voter_ids_are_random_hex_and_valid() {
    let ids: HashSet<String> = (0..500)
        .map(|_| random_voter_id().expect("id").as_str().to_string())
        .collect();
    assert_eq!(ids.len(), 500);
    for id in &ids {
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        assert!(VoterId::new(id).is_ok());
    }
}

// --- 登録 ---

#[tokio::test]
async fn registers_hashed_credentials_roll_and_registry_for_every_voter() {
    let db = Arc::new(FakeDb::default());
    let mut sink = CollectSink::default();
    let summary = register(
        db.clone(),
        &election(),
        voters(20),
        &options(false),
        &mut sink,
    )
    .await
    .expect("register");
    assert_eq!(
        (summary.created, summary.reissued, summary.skipped),
        (20, 0, 0)
    );
    assert_eq!(sink.0.len(), 20);

    let credentials = db.credentials.lock().expect("lock");
    assert_eq!(credentials.len(), 20);
    let internal: HashSet<&str> = credentials.values().map(|c| c.voter_id.as_str()).collect();
    assert_eq!(internal.len(), 20, "内部の voter_id は、有権者ごとに別");
    for row in &sink.0 {
        let record = credentials.get(&row.login_id).expect("registered");
        // DB にあるのはハッシュ（Argon2id）だけで、平文は含まれない。voter_id は、ログイン ID とも、名簿の ID とも無関係。
        assert!(
            record.password_hash.starts_with("$argon2id$"),
            "{}",
            record.password_hash
        );
        assert!(!record.password_hash.contains(&row.password));
        assert_ne!(record.voter_id.as_str(), row.login_id);
        assert!(!record.voter_id.as_str().starts_with("voter-"));
        assert_eq!(row.login_id.len(), 10);
        assert_eq!(row.password.len(), 12);
        assert_ne!(row.login_id, row.password);
    }
    // 台帳と名簿。
    let registry = db.registry.lock().expect("lock");
    let roll = db.roll.lock().expect("lock");
    for i in 1..=20 {
        let entry = registry.get(&format!("voter-{i}")).expect("registry");
        assert!(credentials.contains_key(&entry.login_id));
        assert_eq!(roll.get(entry.voter_id.as_str()), Some(&tokyo_districts()));
    }
}

#[tokio::test]
async fn registered_credentials_log_in_and_yield_the_internal_voter_id() {
    let db = Arc::new(FakeDb::default());
    let mut sink = CollectSink::default();
    register(
        db.clone(),
        &election(),
        voters(3),
        &options(false),
        &mut sink,
    )
    .await
    .expect("register");
    let auth = DbAuthenticator::new(db.clone(), Arc::new(InlineChecker), &FAST).expect("auth");
    for (i, row) in sink.0.iter().enumerate() {
        let voter = auth
            .authenticate(Credentials {
                voter_id: row.login_id.clone(),
                password: Some(row.password.clone()),
                my_number: None,
            })
            .await
            .expect("login");
        let record = db
            .credentials
            .lock()
            .expect("lock")
            .get(&row.login_id)
            .cloned()
            .expect("record");
        assert_eq!(voter, record.voter_id);
        // 別の人のパスワード・存在しない ID は、通らない。
        let other = &sink.0[(i + 1) % 3];
        for (id, pw) in [
            (row.login_id.as_str(), other.password.as_str()),
            ("ZZZZZZZZZZ", row.password.as_str()),
        ] {
            let result = auth
                .authenticate(Credentials {
                    voter_id: id.to_string(),
                    password: Some(pw.to_string()),
                    my_number: None,
                })
                .await;
            assert_eq!(result, Err(AuthError::InvalidCredentials));
        }
    }
}

#[tokio::test]
async fn a_second_run_skips_registered_voters_and_keeps_their_credentials() {
    let db = Arc::new(FakeDb::default());
    register(
        db.clone(),
        &election(),
        voters(5),
        &options(false),
        &mut CollectSink::default(),
    )
    .await
    .expect("first");
    let before = db.credentials.lock().expect("lock").clone();
    // 有権者 6 人目が増えた名簿で、もう一度。
    let mut sink = CollectSink::default();
    let summary = register(
        db.clone(),
        &election(),
        voters(6),
        &options(false),
        &mut sink,
    )
    .await
    .expect("second");
    assert_eq!(
        (summary.created, summary.reissued, summary.skipped),
        (1, 0, 5)
    );
    assert_eq!(
        sink.0.len(),
        1,
        "出力されるのは、新しく発行した分だけ（既存の平文は取り出せない）"
    );
    let after = db.credentials.lock().expect("lock");
    assert_eq!(after.len(), 6);
    for (login_id, record) in &before {
        assert_eq!(
            after.get(login_id),
            Some(record),
            "既存の認証情報は変わらない"
        );
    }
}

#[tokio::test]
async fn reissue_replaces_the_credentials_and_keeps_the_internal_voter_id() {
    let db = Arc::new(FakeDb::default());
    let mut first = CollectSink::default();
    register(
        db.clone(),
        &election(),
        voters(4),
        &options(false),
        &mut first,
    )
    .await
    .expect("first");
    let voter_ids: HashMap<String, VoterId> = db
        .registry
        .lock()
        .expect("lock")
        .iter()
        .map(|(k, v)| (k.clone(), v.voter_id.clone()))
        .collect();

    let mut second = CollectSink::default();
    let summary = register(
        db.clone(),
        &election(),
        voters(4),
        &options(true),
        &mut second,
    )
    .await
    .expect("reissue");
    assert_eq!(
        (summary.created, summary.reissued, summary.skipped),
        (0, 4, 0)
    );
    assert_eq!(second.0.len(), 4);
    // 内部の voter_id は同じ（投票済みの記録が保たれる）。古い認証情報は削除され、新しいものだけが有効。
    for (external, voter_id) in &voter_ids {
        assert_eq!(
            &db.registry.lock().expect("lock")[external].voter_id,
            voter_id
        );
    }
    let credentials = db.credentials.lock().expect("lock");
    assert_eq!(credentials.len(), 4);
    let old: HashSet<&str> = first.0.iter().map(|r| r.login_id.as_str()).collect();
    for row in &second.0 {
        assert!(!old.contains(row.login_id.as_str()), "新しいログイン ID");
        assert!(credentials.contains_key(&row.login_id));
    }
    for row in &first.0 {
        assert!(
            !credentials.contains_key(&row.login_id),
            "古い認証情報は削除された"
        );
    }
}

#[tokio::test]
async fn login_id_collisions_are_retried_with_a_new_id() {
    let db = Arc::new(FakeDb::default());
    db.collisions.store(3, Ordering::SeqCst);
    let mut sink = CollectSink::default();
    let summary = register(
        db.clone(),
        &election(),
        voters(1),
        &options(false),
        &mut sink,
    )
    .await
    .expect("register");
    assert_eq!(summary.created, 1);
    assert_eq!(db.credentials.lock().expect("lock").len(), 1);

    let stuck = Arc::new(FakeDb {
        always_collide: true,
        ..FakeDb::default()
    });
    let err = register(
        stuck,
        &election(),
        voters(1),
        &options(false),
        &mut CollectSink::default(),
    )
    .await
    .expect_err("collisions never resolve");
    assert!(matches!(err, CredgenError::IdCollision), "{err}");
}

// --- 郵送用の列 ---

#[test]
fn mailing_columns_use_the_residence_prefecture_and_all_district_names_in_display_order() {
    let (prefecture, districts) = mailing_columns(&election(), &tokyo_districts());
    // 居住地: 対象の都道府県が最も少ない選挙区（東京 1 区 = 東京都）。全国区の 2 県ではない。
    assert_eq!(prefecture, "東京都");
    assert_eq!(
        districts, "東京1区;比例代表 東京ブロック;参議院比例代表（全国）;東京都知事",
        "選挙の種類の表示順（小選挙区 → 比例 → 参議院比例 → 知事）"
    );
    // 合区（1 つの選挙区が 2 県）は、県名を `;` で並べる。
    let (prefecture, _) = mailing_columns(&election(), &[d("sangiin_district.31_32")]);
    assert_eq!(prefecture, "鳥取県;島根県");
}

// --- CSV の出力 ---

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("credgen-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("out").join("credentials.csv")
}

#[test]
fn csv_output_is_created_with_mode_0600_and_never_overwritten() {
    let path = temp_path("csv");
    let mut sink = CsvFileSink::create(&path).expect("create");
    sink.write(&IssuedRow {
        login_id: "ABCDEFGHJK".to_string(),
        password: "PASSW0RD2345".to_string(),
        prefecture: "東京都".to_string(),
        districts: "東京1区;東京都知事, 補足 \"引用\"".to_string(),
    })
    .expect("write");
    sink.finish().expect("finish");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "権限は 0600（{mode:o}）");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        text.starts_with("login_id,password,都道府県,選挙区\n"),
        "{text}"
    );
    // 引用符・カンマを含む列も、CSV として正しく読み戻せる。
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let rows: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().expect("parse");
    assert_eq!(rows.len(), 1);
    assert_eq!(&rows[0][3], "東京1区;東京都知事, 補足 \"引用\"");
    // すでにあれば、上書きしない（送付前の平文を消さない）。
    let err = CsvFileSink::create(&path).expect_err("exists");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(err.to_string().contains("上書きしません"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        text,
        "元のファイルは無傷"
    );
    let _ = std::fs::remove_dir_all(path.parent().and_then(|p| p.parent()).expect("dir"));
}

#[tokio::test]
async fn no_sink_drops_the_plaintext_but_still_registers() {
    let db = Arc::new(FakeDb::default());
    let summary = register(
        db.clone(),
        &election(),
        voters(3),
        &options(false),
        &mut NoSink,
    )
    .await
    .expect("register");
    assert_eq!(summary.created, 3);
    assert_eq!(db.credentials.lock().expect("lock").len(), 3);
}
