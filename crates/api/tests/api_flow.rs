//! API の統合テスト。`Router` を直接呼び、HTTP レベルの振る舞いを検証する。

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use api::{Built, Config, app, app as app_for, build};
use application::Clock;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use domain::seal_policy::SealPolicy;
use domain::{
    Ballot, BallotId, Block, BlockHeader, CandidateId, ContestId, Ed25519Signer, Ed25519Verifier,
    ElectionPhase, ElectionRules, Period, Signer, verify_chain,
};
use sealer::{ManualClock, Sealer};
use serde_json::{Value, json};
use shared_types::hex;
use tower::ServiceExt;

const SECRET: &str = "test-secret-0123456789abcdef";

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_unix_secs(&self) -> u64 {
        self.0
    }
}

/// テスト用の小さな選挙データ（一時ディレクトリ。プロセスごとに 1 回だけ書く）。
///
/// 種類の表示順: 小選挙区（10）→ 比例代表（20）→ 知事（30）。
/// alice: 東京 1 区・比例東京・東京都知事 / bob: 大阪 1 区だけ / `voter-0`〜`voter-99`: 東京 1 区・比例東京。
fn seed_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("api-flow-seed-{}", std::process::id()));
        let dir = root.join("2026-general");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(dir.join("candidates")).expect("create seed dirs");
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).expect("write");
        write(
            "election.toml",
            "id = \"2026-general\"\nname = \"テスト選挙\"\n\n\
             [[types]]\ncode = \"shugiin_smd\"\nname = \"衆議院小選挙区選挙\"\norder = 10\nmethod = \"single_choice\"\n\n\
             [[types]]\ncode = \"shugiin_pr\"\nname = \"衆議院比例代表選挙\"\norder = 20\nmethod = \"single_choice\"\n\n\
             [[types]]\ncode = \"governor\"\nname = \"都道府県知事選挙\"\norder = 30\nmethod = \"single_choice\"\n",
        );
        write(
            "districts.csv",
            "district_id,election_type,name,prefectures,order\n\
             shugiin_smd.13.01,shugiin_smd,東京1区,13,1\n\
             shugiin_smd.27.01,shugiin_smd,大阪1区,27,2\n\
             shugiin_pr.tokyo,shugiin_pr,比例東京ブロック,13,1\n\
             governor.13,governor,東京都知事,13,1\n",
        );
        let candidates = |district: &str, n: u32| {
            let mut rows = String::from("candidate_id,district_id,name,party,profile\n");
            for i in 1..=n {
                rows.push_str(&format!("{district}.c{i},{district},候補{i},党{i},\n"));
            }
            rows
        };
        write("candidates/shugiin_smd.csv", &{
            let mut all = candidates("shugiin_smd.13.01", 4);
            all.push_str(candidates("shugiin_smd.27.01", 3).split_once('\n').map_or("", |(_, rows)| rows));
            all
        });
        write("candidates/shugiin_pr.csv", &candidates("shugiin_pr.tokyo", 3));
        write("candidates/governor.csv", &candidates("governor.13", 2));
        let mut voters = String::from("voter_id,districts\n");
        voters.push_str("alice,shugiin_smd.13.01;shugiin_pr.tokyo;governor.13\n");
        voters.push_str("bob,shugiin_smd.27.01\n");
        for i in 0..100 {
            voters.push_str(&format!("voter-{i},shugiin_smd.13.01;shugiin_pr.tokyo\n"));
        }
        write("voters.csv", &voters);
        root
    })
    .clone()
}

/// 投票用紙 1 枚（東京 1 区の小選挙区）の、パスと候補者 ID。
const SMD1: &str = "/api/v1/contests/2026-general/shugiin_smd.13.01";
const PR: &str = "/api/v1/contests/2026-general/shugiin_pr.tokyo";
const OSAKA: &str = "/api/v1/contests/2026-general/shugiin_smd.27.01";

fn candidate(district: &str, n: u32) -> String {
    format!("{district}.c{n}")
}

fn config(shards: u16) -> Config {
    Config {
        storage: api::config::Storage::Memory,
        port: 0,
        shard_count: std::num::NonZeroU16::new(shards).expect("non-zero"),
        seed_dir: seed_dir(),
        election_id: "2026-general".to_string(),
        labels: api::ApiLabels::default(),
        session_secret: SECRET.to_string(),
        session_ttl_secs: 3600,
        auth_mode: app_config::AuthMode::Stub,
        password_params: application::PasswordParams {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        },
        seal_policy: SealPolicy::new(100, 10, 10).expect("valid policy"),
        sealer_signing_seed: Some([7u8; 32]),
        reveal: api::RevealPolicy::Always,
        request_timeout: std::time::Duration::from_secs(10),
        // 期間は指定しない（原則17・18 の境界テストは、別に `election_state.transition` / `schedule` を
        // 直接呼んで検証する）。`built_with` が、テスト用に scheduled → open へ進める。
        period: Period::default(),
        election_grace: std::time::Duration::from_secs(1),
        state_cache_secs: 0,
        display_timezone: app_config::DisplayTimezone {
            name: "Asia/Tokyo",
            offset_secs: 9 * 3600,
        },
        admin_bind: "127.0.0.1:0".to_string(),
        admin_token: None,
        rules: ElectionRules { allow_blank: true },
    }
}

/// 独立したストアを持つ API インスタンスと、手動で動かす sealer。
async fn built(shards: u16, now: u64) -> (Router, Sealer) {
    built_with(shards, now, api::RevealPolicy::Always).await
}

/// `built` で、票の公開のタイミング（`chain.reveal_ballots`）を指定する。
async fn built_with(shards: u16, now: u64, reveal: api::RevealPolicy) -> (Router, Sealer) {
    let mut cfg = config(shards);
    cfg.reveal = reveal;
    built_from(cfg, now, None).await
}

/// 設定 `configured` の `vote.allow_blank` で起動し、open に進めるときに `frozen`（省略時は設定の値）を固定する
/// （原則19: open にしたプロセスの設定が固定され、以降は、この api の設定より優先する）。
async fn built_rules(configured: bool, frozen: bool) -> (Router, Sealer) {
    let mut cfg = config(1);
    cfg.rules = ElectionRules {
        allow_blank: configured,
    };
    built_from(
        cfg,
        1_000,
        Some(ElectionRules {
            allow_blank: frozen,
        }),
    )
    .await
}

async fn built_from(cfg: Config, now: u64, frozen: Option<ElectionRules>) -> (Router, Sealer) {
    let Built { state, sealer } = build(
        &cfg,
        Arc::new(FixedClock(now)),
        Arc::new(ManualClock::new()),
    )
    .await
    .expect("build");
    // 期間を指定していないテストは、原則17の状態機械を意識せずに投票できるよう、
    // scheduled → open へ進めておく（境界を検証するテストは、この関数を使わず個別に組み立てる）。
    state
        .election_state
        .transition(
            ElectionPhase::Scheduled,
            ElectionPhase::Open,
            frozen.unwrap_or(cfg.rules),
            "test",
            0,
        )
        .await
        .expect("open for test");
    // app.mode=memory ではプロセス内 sealer がある（テストでは手動で `flush` / `anchor` を呼ぶ）。
    (
        app(state),
        sealer.expect("memory storage has an in-process sealer"),
    )
}

/// 独立したストアを持つ API インスタンス（時計は固定）。
async fn instance(now: u64) -> Router {
    built(4, now).await.0
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn login(app: &Router, voter_id: &str) -> String {
    let (status, body) = send(
        app,
        "POST",
        "/api/v1/login",
        None,
        Some(json!({ "voter_id": voter_id, "my_number": "123456789012" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["token"].as_str().expect("token").to_string()
}

fn vote_body(candidate_id: &str) -> Option<Value> {
    Some(json!({ "candidate_id": candidate_id }))
}

/// エラー応答が、機械可読なコードと、利用者向けの文言を持つこと。文言に旧来の呼び名は出ない。
fn assert_error(body: &Value, code: &str) {
    assert_eq!(body["error"], json!(code), "{body}");
    let message = body["message"].as_str().expect("message");
    assert!(!message.is_empty(), "{body}");
    // 旧来の呼び名は、メッセージに出ない（この語そのものを、ソースに書かないために、分けて組み立てる）。
    let old_word = ["コン", "テスト"].concat();
    assert!(!message.contains(&old_word), "{message}");
}

#[tokio::test]
async fn healthz_returns_ok_json() {
    let (status, body) = send(&instance(1_000).await, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "status": "ok" }));
}

#[tokio::test]
async fn full_voting_flow() {
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;

    // 状態: alice に関係する 3 枚だけが、表示順（小選挙区 → 比例代表 → 知事）で、すべて未投票。
    let (status, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let ballots = body["ballots"].as_array().expect("ballots");
    let ids: Vec<&str> = ballots
        .iter()
        .map(|b| b["contest_id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        [
            "2026-general/shugiin_smd.13.01",
            "2026-general/shugiin_pr.tokyo",
            "2026-general/governor.13"
        ]
    );
    assert!(ballots.iter().all(|b| b["voted"] == json!(false)));
    assert_eq!(ballots[0]["name"], json!("東京1区"));
    assert_eq!(ballots[0]["election_type"], json!("shugiin_smd"));
    assert_eq!(ballots[0]["type_name"], json!("衆議院小選挙区選挙"));
    assert_eq!(ballots[0]["method"], json!("single_choice"));

    // 候補者（ID は文字列。氏名・政党つき）。
    let (status, body) = send(
        &app,
        "GET",
        &format!("{SMD1}/candidates"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["candidates"].as_array().expect("candidates").len(), 4);
    assert_eq!(
        body["candidates"][0],
        json!({ "candidate_id": "shugiin_smd.13.01.c1", "name": "候補1", "party": "党1" })
    );

    // 投票（201。レシートは返さない）
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, json!({ "status": "accepted" }));

    // 同じ投票用紙への再投票は 409
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 2)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "already_voted");

    // 状態: 先頭の 1 枚だけ投票済み（並びは変わらない）。
    let (_, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&token), None).await;
    let voted: Vec<bool> = body["ballots"]
        .as_array()
        .expect("ballots")
        .iter()
        .map(|b| b["voted"].as_bool().expect("voted"))
        .collect();
    assert_eq!(voted, [true, false, false]);

    // 別の投票用紙には投票できる
    let (status, _) = send(
        &app,
        "POST",
        &format!("{PR}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_pr.tokyo", 3)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn ballot_status_lists_only_the_voters_own_ballots() {
    let app = instance(1_000).await;
    // bob は大阪 1 区の 1 枚だけ。
    let bob = login(&app, "bob").await;
    let (_, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&bob), None).await;
    let ids: Vec<&str> = body["ballots"]
        .as_array()
        .expect("ballots")
        .iter()
        .map(|b| b["contest_id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["2026-general/shugiin_smd.27.01"]);
    // 名簿に無い有権者は、ログインできるが、投票用紙は 1 枚もない。
    let stranger = login(&app, "stranger").await;
    let (status, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&stranger), None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ballots": [] })));
}

#[tokio::test]
async fn voters_cannot_reach_ballots_outside_their_districts() {
    let app = instance(1_000).await;
    let bob = login(&app, "bob").await;
    let stranger = login(&app, "stranger").await;
    for token in [&bob, &stranger] {
        // bob は東京 1 区の有権者ではなく、名簿に無い有権者はどの投票用紙も対象外: 403。
        let (status, body) = send(
            &app,
            "GET",
            &format!("{SMD1}/candidates"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "not_eligible");
        let (status, body) = send(
            &app,
            "POST",
            &format!("{SMD1}/vote"),
            Some(token),
            vote_body(&candidate("shugiin_smd.13.01", 1)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "not_eligible");
    }
    // 拒否された投票は「投票済み」にならず、票も入らない。
    let (_, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&bob), None).await;
    assert_eq!(body["ballots"][0]["voted"], json!(false));
    let (status, _) = send(
        &app,
        "POST",
        &format!("{OSAKA}/vote"),
        Some(&bob),
        vote_body(&candidate("shugiin_smd.27.01", 3)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn other_voters_are_independent() {
    let app = instance(1_000).await;
    let alice = login(&app, "alice").await;
    let voter = login(&app, "voter-1").await;
    let (a, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&alice),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    let (b, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&voter),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!((a, b), (StatusCode::CREATED, StatusCode::CREATED));
    let (_, body) = send(&app, "GET", "/api/v1/ballot-status", Some(&voter), None).await;
    assert_eq!(body["ballots"][1]["voted"], json!(false));
}

#[tokio::test]
async fn requests_without_valid_token_are_unauthorized() {
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;
    let tampered = token.replacen("alice", "bob", 1);
    let cases = [
        None,
        Some("garbage".to_string()),
        Some(tampered),
        Some(String::new()),
    ];
    let candidates_uri = format!("{SMD1}/candidates");
    let vote_uri = format!("{SMD1}/vote");
    for case in cases {
        for (method, uri, body) in [
            ("GET", "/api/v1/ballot-status", None),
            ("GET", candidates_uri.as_str(), None),
            (
                "POST",
                vote_uri.as_str(),
                vote_body(&candidate("shugiin_smd.13.01", 1)),
            ),
        ] {
            let (status, resp) = send(&app, method, uri, case.as_deref(), body).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {case:?}");
            assert_error(&resp, "unauthorized");
        }
    }
}

#[tokio::test]
async fn non_bearer_scheme_is_unauthorized() {
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;
    let request = Request::builder()
        .uri("/api/v1/ballot-status")
        .header("authorization", format!("Basic {token}"))
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_works_across_instances_until_it_expires() {
    // ステートレス: 同じ署名鍵なら、ログインした所と別のインスタンスでも検証できる。
    let issuer = instance(1_000).await;
    let token = login(&issuer, "alice").await;

    let other_instance = instance(1_500).await;
    let (status, _) = send(
        &other_instance,
        "GET",
        "/api/v1/ballot-status",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 有効期限（発行から 3600 秒）を過ぎると拒否される。
    let later = instance(1_000 + 3_600).await;
    let (status, _) = send(&later, "GET", "/api/v1/ballot-status", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_rejects_malformed_voter_id() {
    let app = instance(1_000).await;
    for bad in ["", "a.b", "a b", &"x".repeat(65)] {
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/login",
            None,
            Some(json!({ "voter_id": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bad:?}");
        assert_error(&body, "unauthorized");
    }
}

#[tokio::test]
async fn unknown_contest_and_foreign_candidate_are_rejected() {
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;

    // 存在しない投票用紙（選挙区・選挙の ID が不正な形式のものを含む）は 404。
    for uri in [
        "/api/v1/contests/2026-general/shugiin_smd.99.99/candidates",
        "/api/v1/contests/other-election/shugiin_smd.13.01/candidates",
        "/api/v1/contests/2026-general/BAD/candidates",
    ] {
        let (status, body) = send(&app, "GET", uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_error(&body, "not_found");
    }
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/contests/2026-general/shugiin_smd.99.99/vote",
        Some(&token),
        vote_body(&candidate("shugiin_smd.99.99", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 別の投票用紙の候補者・存在しない候補者・形式が不正な候補者は、422。
    for bad in [
        candidate("shugiin_pr.tokyo", 1),
        candidate("shugiin_smd.13.01", 99),
        "not a candidate id".to_string(),
    ] {
        let (status, body) = send(
            &app,
            "POST",
            &format!("{SMD1}/vote"),
            Some(&token),
            vote_body(&bad),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_error(&body, "invalid_candidate");
    }
    // 不正な投票は「投票済み」にならない。
    let (status, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn error_messages_use_the_configured_ballot_item_label() {
    // 呼び名は設定（labels.ballot_item）から読む。既定は「投票用紙」。
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;
    let (_, body) = send(
        &app,
        "GET",
        "/api/v1/contests/2026-general/shugiin_smd.99.99/candidates",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["message"], json!("投票用紙が見つかりません。"));

    let mut cfg = config(4);
    cfg.labels = api::ApiLabels {
        ballot_item: "票".to_string(),
        ..api::ApiLabels::default()
    };
    let Built { state, .. } = build(
        &cfg,
        Arc::new(FixedClock(1_000)),
        Arc::new(ManualClock::new()),
    )
    .await
    .expect("build");
    let custom = app_for(state);
    let token = login(&custom, "alice").await;
    let (_, body) = send(
        &custom,
        "GET",
        "/api/v1/contests/2026-general/shugiin_smd.99.99/candidates",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["message"], json!("票が見つかりません。"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_votes_for_same_voter_and_contest_succeed_exactly_once() {
    let app = instance(1_000).await;
    let token = login(&app, "alice").await;

    let mut tasks = Vec::new();
    for i in 0..100u32 {
        let (app, token) = (app.clone(), token.clone());
        tasks.push(tokio::spawn(async move {
            let candidate = candidate("shugiin_smd.13.01", 1 + i % 4);
            send(
                &app,
                "POST",
                &format!("{SMD1}/vote"),
                Some(&token),
                vote_body(&candidate),
            )
            .await
            .0
        }));
    }
    let mut created = 0;
    let mut conflict = 0;
    for task in tasks {
        match task.await.expect("task") {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => conflict += 1,
            other => panic!("unexpected status: {other}"),
        }
    }
    assert_eq!((created, conflict), (1, 99));
}

#[cfg(not(feature = "dev-tools"))]
#[tokio::test]
async fn debug_pool_is_not_exposed_without_dev_tools() {
    let app = instance(1_000).await;
    let (status, _) = send(&app, "GET", "/debug/pool", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "POST", "/debug/tamper", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[cfg(feature = "dev-tools")]
#[tokio::test]
async fn debug_pool_returns_counts_only() {
    let app = instance(1_000).await;
    let (status, body) = send(&app, "GET", "/debug/pool", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], json!(0));
    assert_eq!(body["shards"].as_array().expect("shards").len(), 4);

    let token = login(&app, "alice").await;
    send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    // 拒否された投票は数えない。
    send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 2)),
    )
    .await;

    let (_, body) = send(&app, "GET", "/debug/pool", None, None).await;
    assert_eq!(body["total"], json!(1));
    let per_shard: i64 = body["shards"]
        .as_array()
        .expect("shards")
        .iter()
        .map(|s| s["pending"].as_i64().expect("pending"))
        .sum();
    assert_eq!(per_shard, 1);

    // 件数以外（票の中身・ID・候補者）は含まれない。
    let text = body.to_string();
    for forbidden in ["alice", "candidate", "ballot", "voter"] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
    let keys: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"shards") && keys.contains(&"total"));
}

// --- 封印済みチェーンの公開 API ---

/// `n` 人が東京 1 区の小選挙区に投票する。
async fn vote_many(app: &Router, n: u32) {
    for i in 0..n {
        let token = login(app, &format!("voter-{i}")).await;
        let (status, _) = send(
            app,
            "POST",
            &format!("{SMD1}/vote"),
            Some(&token),
            vote_body(&candidate("shugiin_smd.13.01", 1 + i % 4)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
}

/// API から取得したブロックを、検証用のドメイン型に戻す。
fn to_block(v: &Value) -> Block {
    let hex32 = |x: &Value| hex::decode_array::<32>(x.as_str().expect("hex")).expect("32 bytes");
    let h = &v["header"];
    Block {
        header: BlockHeader {
            version: h["version"].as_u64().expect("version") as u16,
            height: h["height"].as_u64().expect("height"),
            prev_hash: hex32(&h["prev_hash"]),
            merkle_root: hex32(&h["merkle_root"]),
            ballot_count: h["ballot_count"].as_u64().expect("count") as u32,
            sealed_at_minute: h["sealed_at_minute"].as_u64().expect("minute"),
        },
        ballots: v["ballots"]
            .as_array()
            .expect("ballots")
            .iter()
            .map(|b| Ballot {
                ballot_id: BallotId(
                    hex::decode_array::<16>(b["ballot_id"].as_str().expect("id")).expect("16"),
                ),
                contest_id: ContestId::parse(b["contest_id"].as_str().expect("contest"))
                    .expect("valid contest id"),
                candidate_id: CandidateId::parse(b["candidate_id"].as_str().expect("cand"))
                    .expect("valid candidate id"),
            })
            .collect(),
        block_hash: hex32(&v["block_hash"]),
        signature: hex::decode_array::<64>(v["signature"].as_str().expect("sig")).expect("64"),
    }
}

/// シャード 0 のチェーンを API から全部取得する。
async fn fetch_chain(app: &Router) -> (Vec<Block>, [u8; 32]) {
    let (status, head) = send(app, "GET", "/api/v1/chains/0/head", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let public_key =
        hex::decode_array::<32>(head["signer_public_key"].as_str().expect("key")).expect("32");
    let mut blocks = Vec::new();
    for height in 0..=head["height"].as_u64().expect("height") {
        let uri = format!("/api/v1/chains/0/blocks/{height}");
        let (status, block) = send(app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        blocks.push(to_block(&block));
    }
    (blocks, public_key)
}

#[tokio::test]
async fn chain_endpoints_expose_sealed_blocks_without_authentication() {
    let (app, mut sealer) = built(1, 1_000).await;

    // 起動直後はジェネシスだけ。
    let (status, head) = send(&app, "GET", "/api/v1/chains/0/head", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(head["shard"], json!(0));
    assert_eq!(head["height"], json!(0));
    assert_eq!(head["ballot_count"], json!(0));
    assert_eq!(head["block_hash"].as_str().expect("hash").len(), 64);
    assert_eq!(head["signer_public_key"].as_str().expect("key").len(), 64);

    vote_many(&app, 5).await;
    let outcome = sealer.close_flush().await;
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.events.len(), 1);

    let (_, head) = send(&app, "GET", "/api/v1/chains/0/head", None, None).await;
    assert_eq!(
        (head["height"].clone(), head["ballot_count"].clone()),
        (json!(1), json!(5))
    );

    let (status, block) = send(&app, "GET", "/api/v1/chains/0/blocks/1", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let ballots = block["ballots"].as_array().expect("ballots");
    assert_eq!(ballots.len(), 5);
    assert!(
        ballots
            .iter()
            .all(|b| b["contest_id"] == json!("2026-general/shugiin_smd.13.01"))
    );
    // ブロック内は ballot_id のハッシュ昇順。
    let keys: Vec<_> = to_block(&block)
        .ballots
        .iter()
        .map(|b| domain::encoding::ballot_order_key(&b.ballot_id))
        .collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]));
    // 投票者を特定できる情報は含まれない。
    assert!(!block.to_string().contains("voter"));

    // ジェネシスは票を持たない。
    let (_, genesis) = send(&app, "GET", "/api/v1/chains/0/blocks/0", None, None).await;
    assert_eq!(genesis["ballots"], json!([]));

    // 存在しないシャード・高さは 404。
    for uri in [
        "/api/v1/chains/9/head",
        "/api/v1/chains/9/blocks/0",
        "/api/v1/chains/0/blocks/2",
    ] {
        let (status, body) = send(&app, "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_error(&body, "not_found");
    }
}

#[tokio::test]
async fn chain_fetched_over_http_verifies_with_the_published_key() {
    let (app, mut sealer) = built(1, 1_000).await;
    vote_many(&app, 12).await;
    sealer.close_flush().await;

    let (blocks, public_key) = fetch_chain(&app).await;
    assert_eq!(blocks.len(), 2);
    // 公開鍵は設定した署名鍵の種から導かれるものと一致する。
    assert_eq!(
        public_key,
        Ed25519Signer::from_seed(&[7u8; 32]).public_key()
    );
    let verifier = Ed25519Verifier::from_public_key(&public_key).expect("key");
    assert_eq!(verify_chain(&blocks, &verifier), Ok(()));
}

#[cfg(feature = "dev-tools")]
#[tokio::test]
async fn tamper_breaks_chain_verification_and_reveals_only_the_location() {
    let (app, mut sealer) = built(1, 1_000).await;
    vote_many(&app, 6).await;
    sealer.close_flush().await;

    let (blocks, public_key) = fetch_chain(&app).await;
    let verifier = Ed25519Verifier::from_public_key(&public_key).expect("key");
    assert_eq!(verify_chain(&blocks, &verifier), Ok(()));

    let (status, body) = send(&app, "POST", "/debug/tamper", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "tampered": { "shard": 0, "height": 1, "index": 0 } })
    );

    let (tampered, _) = fetch_chain(&app).await;
    assert_eq!(
        verify_chain(&tampered, &verifier),
        Err(domain::ChainError::MerkleRootMismatch { height: 1 })
    );

    // 明示指定も可能で、対象がなければ 404。
    let (status, _) = send(
        &app,
        "POST",
        "/debug/tamper",
        None,
        Some(json!({ "shard": 0, "height": 1, "index": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        "POST",
        "/debug/tamper",
        None,
        Some(json!({ "height": 99 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- アンカーと監査用の集計 ---

#[tokio::test]
async fn latest_anchor_is_absent_until_created_and_then_verifies_against_the_chain() {
    let (app, mut sealer) = built(2, 1_000).await;
    let (status, body) = send(&app, "GET", "/api/v1/anchors/latest", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, "not_found");

    vote_many(&app, 6).await;
    sealer.close_flush().await;
    let anchor = sealer.anchor().await.expect("anchor").expect("created");

    // 認証なしで取得でき、署名の公開鍵（head が示すもの）で検証でき、head が実チェーンと一致する。
    let (status, dto) = send(&app, "GET", "/api/v1/anchors/latest", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dto["seq"], json!(1));
    assert_eq!(dto["anchor_hash"], json!(hex::encode(&anchor.anchor_hash)));
    let heads = dto["heads"].as_array().expect("heads");
    assert_eq!(heads.len(), 2);
    assert_eq!(heads[0]["shard"], json!(0));

    let rebuilt = domain::Anchor {
        seq: dto["seq"].as_u64().expect("seq"),
        anchor_minute: dto["anchor_minute"].as_u64().expect("minute"),
        prev_anchor_hash: hex::decode_array::<32>(dto["prev_anchor_hash"].as_str().expect("prev"))
            .expect("32"),
        heads: heads
            .iter()
            .map(|h| domain::ShardHead {
                shard: h["shard"].as_u64().expect("shard") as u16,
                height: h["height"].as_u64().expect("height"),
                block_hash: hex::decode_array::<32>(h["block_hash"].as_str().expect("hash"))
                    .expect("32"),
            })
            .collect(),
        anchor_hash: hex::decode_array::<32>(dto["anchor_hash"].as_str().expect("hash"))
            .expect("32"),
        signature: hex::decode_array::<64>(dto["signature"].as_str().expect("sig")).expect("64"),
    };
    let (_, head0) = send(&app, "GET", "/api/v1/chains/0/head", None, None).await;
    let key =
        hex::decode_array::<32>(head0["signer_public_key"].as_str().expect("key")).expect("32");
    let verifier = Ed25519Verifier::from_public_key(&key).expect("key");
    assert_eq!(domain::verify_anchor(&rebuilt, &verifier), Ok(()));
    for h in &rebuilt.heads {
        let (_, head) = send(
            &app,
            "GET",
            &format!("/api/v1/chains/{}/head", h.shard),
            None,
            None,
        )
        .await;
        assert_eq!(head["height"], json!(h.height));
        assert_eq!(head["block_hash"], json!(hex::encode(&h.block_hash)));
    }
}

#[tokio::test]
async fn audit_counts_report_participation_and_pending_per_ballot_only() {
    let (app, mut sealer) = built(1, 1_000).await;
    let (status, body) = send(&app, "GET", "/api/v1/audit/counts", None, None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "contests": [] })));

    // 3 人が東京 1 区の小選挙区に、うち 2 人が比例東京にも投票する。
    for (i, uris) in [(0, vec![SMD1]), (1, vec![SMD1, PR]), (2, vec![SMD1, PR])] {
        let token = login(&app, &format!("voter-{i}")).await;
        for uri in uris {
            let district = uri.rsplit('/').next().expect("district");
            let (status, _) = send(
                &app,
                "POST",
                &format!("{uri}/vote"),
                Some(&token),
                vote_body(&candidate(district, 1)),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
        }
    }
    // 投票用紙の ID の昇順（`shugiin_pr.tokyo` < `shugiin_smd.13.01`）。
    let expected = |pending_smd: u64, pending_pr: u64| {
        json!({ "contests": [
            { "contest_id": "2026-general/shugiin_pr.tokyo", "participation": 2, "pending": pending_pr },
            { "contest_id": "2026-general/shugiin_smd.13.01", "participation": 3, "pending": pending_smd },
        ]})
    };
    let (status, body) = send(&app, "GET", "/api/v1/audit/counts", None, None).await;
    assert_eq!((status, body), (StatusCode::OK, expected(3, 2)));

    // 封印すると pending だけが 0 になり、participation は変わらない。
    sealer.close_flush().await;
    let (_, body) = send(&app, "GET", "/api/v1/audit/counts", None, None).await;
    assert_eq!(body, expected(0, 0));
    // 集計値だけで、投票者・票の中身は含まれない。
    let text = body.to_string();
    for forbidden in ["voter", "ballot", "candidate"] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
}

// --- ブロックチェーンのビューア向け API ---

/// レスポンスヘッダーも返す `send`（GET 専用）。
async fn get_with_headers(app: &Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let request = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn cache_control(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// 1 票ずつ投票して、そのたびに封印する（1 票 = 1 ブロック）。`voters` は、投票する有権者の番号の範囲。
async fn seal_one_by_one(app: &Router, sealer: &mut Sealer, voters: std::ops::Range<u32>) {
    for i in voters {
        let token = login(app, &format!("voter-{i}")).await;
        let (status, _) = send(
            app,
            "POST",
            &format!("{SMD1}/vote"),
            Some(&token),
            vote_body(&candidate("shugiin_smd.13.01", 1 + i % 4)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(sealer.close_flush().await.errors.is_empty());
    }
}

fn heights(page: &Value) -> Vec<u64> {
    page["blocks"]
        .as_array()
        .expect("blocks")
        .iter()
        .map(|b| b["header"]["height"].as_u64().expect("height"))
        .collect()
}

#[tokio::test]
async fn chains_index_lists_every_shard_with_its_head_and_the_signer_key() {
    let (app, mut sealer) = built(2, 1_000).await;
    seal_one_by_one(&app, &mut sealer, 0..3).await;
    let (status, headers, body) = get_with_headers(&app, "/api/v1/chains").await;
    assert_eq!(status, StatusCode::OK);
    // 先頭は伸びるので、毎回確認させる（immutable ではない）。
    assert_eq!(cache_control(&headers), "no-cache");
    let shards = body["shards"].as_array().expect("shards");
    assert_eq!(shards.len(), 2);
    assert_eq!(shards[0]["shard"], json!(0));
    assert_eq!(shards[1]["shard"], json!(1));
    let total: u64 = shards
        .iter()
        .map(|s| s["head"]["header"]["height"].as_u64().expect("height"))
        .sum();
    // ジェネシス（高さ 0）のほかに、3 票が封印されて、2 つのシャードの高さの合計が 3。
    assert_eq!(total, 3);
    assert_eq!(
        body["signer_public_key"].as_str().expect("key"),
        hex::encode(&Ed25519Signer::from_seed(&[7u8; 32]).public_key())
    );
    assert!(shards[0]["head"]["block_hash"].as_str().is_some());
    // 票は含まない（要約だけ）。
    assert!(!body.to_string().contains("candidate_id"));
}

#[tokio::test]
async fn blocks_pages_walk_the_chain_newest_first_without_gaps_or_duplicates() {
    let (app, mut sealer) = built(1, 1_000).await;
    seal_one_by_one(&app, &mut sealer, 0..10).await; // 高さ 0（ジェネシス）〜 10
    let mut seen = Vec::new();
    let mut before: Option<u64> = None;
    let mut pages = 0;
    loop {
        let uri = match before {
            Some(b) => format!("/api/v1/chains/0/blocks?before_height={b}&limit=4"),
            None => "/api/v1/chains/0/blocks?limit=4".to_string(),
        };
        let (status, headers, page) = get_with_headers(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(page["shard"], json!(0));
        // 先頭からのページは変わり得るので確認させる。before_height が先頭以下のページは確定（immutable）。
        let expected = if before.is_none() {
            "no-cache"
        } else {
            IMMUTABLE
        };
        assert_eq!(cache_control(&headers), expected, "{uri}");
        // 要約だけ（票の中身は含まない）。
        assert!(!page.to_string().contains("candidate_id"), "{page}");
        seen.extend(heights(&page));
        pages += 1;
        match page["next_before_height"].as_u64() {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, (0..=10).rev().collect::<Vec<u64>>());
    assert_eq!(pages, 3); // 4 + 4 + 3
}

#[tokio::test]
async fn blocks_page_boundaries_and_query_handling() {
    let (app, mut sealer) = built(1, 1_000).await;
    seal_one_by_one(&app, &mut sealer, 0..5).await; // 高さ 0〜5
    let page = |uri: &'static str| {
        let app = app.clone();
        async move { get_with_headers(&app, uri).await }
    };
    // 空文字は、指定なし。
    let (status, _, p) = page("/api/v1/chains/0/blocks?before_height=&limit=").await;
    assert_eq!(
        (status, heights(&p)),
        (StatusCode::OK, vec![5, 4, 3, 2, 1, 0])
    );
    assert_eq!(p["next_before_height"], Value::Null);
    // limit は 1〜100 にそろえる。
    let (_, _, p) = page("/api/v1/chains/0/blocks?limit=0").await;
    assert_eq!(heights(&p), vec![5]);
    assert_eq!(p["next_before_height"], json!(5));
    let (_, _, p) = page("/api/v1/chains/0/blocks?limit=100000").await;
    assert_eq!(heights(&p).len(), 6);
    // before_height は「それより低い」高さ。0 なら空。先頭より先でも、先頭から返す（ただし確定しない）。
    let (_, h, p) = page("/api/v1/chains/0/blocks?before_height=0").await;
    assert_eq!(heights(&p), Vec::<u64>::new());
    assert_eq!(cache_control(&h), IMMUTABLE);
    let (_, h, p) = page("/api/v1/chains/0/blocks?before_height=6&limit=2").await;
    assert_eq!(heights(&p), vec![5, 4]);
    assert_eq!(cache_control(&h), IMMUTABLE);
    let (_, h, p) = page("/api/v1/chains/0/blocks?before_height=7&limit=2").await;
    assert_eq!(heights(&p), vec![5, 4]);
    assert_eq!(cache_control(&h), "no-cache");
    // 不正な値は 400、存在しないシャードは 404（どちらも保存させない）。
    for uri in [
        "/api/v1/chains/0/blocks?limit=abc",
        "/api/v1/chains/0/blocks?before_height=-1",
    ] {
        let (status, _, _) = page(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    let (status, h, body) = page("/api/v1/chains/9/blocks").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&h), "no-store");
    assert_error(&body, "not_found");
}

#[tokio::test]
async fn a_blank_vote_is_accepted_and_sealed_as_a_blank_ballot() {
    let (app, mut sealer) = built(1, 1_000).await;
    let token = login(&app, "voter-0").await;
    // 候補者一覧には白票を入れず、白票を選べることを allow_blank で示す。
    let (status, body) = send(
        &app,
        "GET",
        &format!("{SMD1}/candidates"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allow_blank"], json!(true));
    let ids: Vec<&str> = body["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| c["candidate_id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&"blank"), "{ids:?}");
    // 予約値 "blank" で白票を投じる。
    let (status, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body("blank"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // 白票でも投票済みになる（同じ投票用紙に 2 回目は 409）。
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "already_voted");
    // 大文字など、予約値と違う綴りは白票ではない（存在しない候補者）。
    let token2 = login(&app, "voter-1").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token2),
        vote_body("BLANK"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error(&body, "invalid_candidate");
    // 封印すると、票の candidate_id は "blank"。ビューアでは白票の印が付き、候補者の表示名は付かない。
    assert!(sealer.close_flush().await.errors.is_empty());
    let (status, _, block) = get_with_headers(&app, "/api/v1/chains/0/blocks/1").await;
    assert_eq!(status, StatusCode::OK);
    let ballot = &block["ballots"][0];
    assert_eq!(ballot["candidate_id"], json!("blank"));
    assert_eq!(ballot["blank"], json!(true));
    assert_eq!(ballot["district_name"], json!("東京1区"));
    assert!(ballot.get("candidate_name").is_none(), "{ballot}");
}

#[tokio::test]
async fn blank_is_rejected_when_the_election_does_not_allow_it() {
    let (app, _sealer) = built_rules(false, false).await;
    let token = login(&app, "voter-0").await;
    let (status, body) = send(
        &app,
        "GET",
        &format!("{SMD1}/candidates"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allow_blank"], json!(false));
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body("blank"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error(&body, "blank_not_allowed");
    assert_eq!(body["message"], json!("この選挙では、白票は選べません。"));
    // 拒否された投票は数えない（まだ投票できる）。
    let (status, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 2)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn the_rule_fixed_at_open_wins_over_the_current_config() {
    // open の時点で「白票あり」に固定した後、設定を「白票なし」にして起動し直した api でも、白票を受け付ける。
    let (app, _sealer) = built_rules(false, true).await;
    let token = login(&app, "voter-0").await;
    let (_, body) = send(
        &app,
        "GET",
        &format!("{SMD1}/candidates"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["allow_blank"], json!(true));
    let (status, _) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body("blank"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // 逆に「白票なし」で固定した後は、設定を「白票あり」にしても拒否する。
    let (app, _sealer) = built_rules(true, false).await;
    let token = login(&app, "voter-0").await;
    let (_, body) = send(
        &app,
        "GET",
        &format!("{SMD1}/candidates"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["allow_blank"], json!(false));
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body("blank"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error(&body, "blank_not_allowed");
}

#[tokio::test]
async fn block_detail_is_immutable_and_carries_display_names_when_ballots_are_public() {
    let (app, mut sealer) = built(1, 1_000).await;
    seal_one_by_one(&app, &mut sealer, 0..1).await;
    let (status, headers, block) = get_with_headers(&app, "/api/v1/chains/0/blocks/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache_control(&headers), IMMUTABLE);
    assert_eq!(block["ballots_revealed"], json!(true));
    let ballot = &block["ballots"][0];
    assert_eq!(ballot["candidate_id"], json!("shugiin_smd.13.01.c1"));
    assert_eq!(ballot["district_name"], json!("東京1区"));
    assert!(
        ballot["candidate_name"]
            .as_str()
            .is_some_and(|n| !n.is_empty())
    );
    assert_eq!(
        block["signer_public_key"].as_str().expect("key"),
        hex::encode(&Ed25519Signer::from_seed(&[7u8; 32]).public_key())
    );
    // 存在しない高さは 404（保存させない）。
    let (status, headers, _) = get_with_headers(&app, "/api/v1/chains/0/blocks/99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&headers), "no-store");
}

#[tokio::test]
async fn after_close_hides_the_ballots_until_the_close_and_only_then_becomes_immutable() {
    let policy = api::RevealPolicy::AfterClose {
        closes_at_unix: 5_000,
    };
    // 締切前（時計 4_999）。
    let (app, mut sealer) = built_with(1, 4_999, policy).await;
    seal_one_by_one(&app, &mut sealer, 0..2).await;
    let (status, headers, block) = get_with_headers(&app, "/api/v1/chains/0/blocks/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(block["ballots_revealed"], json!(false));
    assert_eq!(block["ballots"], json!([]));
    // ヘッダーの情報は返す。票の中身（ballot_id・contest_id・candidate_id）は、どこにも現れない。
    assert_eq!(block["header"]["ballot_count"], json!(1));
    assert!(block["block_hash"].as_str().is_some() && block["signature"].as_str().is_some());
    let text = block.to_string();
    for secret in ["candidate_id", "ballot_id", "contest_id", "shugiin_smd"] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
    // 締切後に中身が変わる応答なので、CDN に保存させない（immutable にしない）。
    assert_eq!(cache_control(&headers), "no-store");

    // 一覧・アンカーは、票を含まないので、締切に関係なく、これまでどおり。
    let (_, _, page) = get_with_headers(&app, "/api/v1/chains/0/blocks?limit=10").await;
    assert_eq!(heights(&page), vec![2, 1, 0]);

    // 締切ちょうど（時計 5_000）から公開する。同じストアで、時計だけが進んだ API を作れないので、別の API で確認する。
    let (open, mut open_sealer) = built_with(1, 5_000, policy).await;
    seal_one_by_one(&open, &mut open_sealer, 0..1).await;
    let (_, headers, block) = get_with_headers(&open, "/api/v1/chains/0/blocks/1").await;
    assert_eq!(block["ballots_revealed"], json!(true));
    assert_eq!(
        block["ballots"][0]["candidate_id"],
        json!("shugiin_smd.13.01.c1")
    );
    assert_eq!(cache_control(&headers), IMMUTABLE);
}

#[tokio::test]
async fn anchors_are_listed_newest_first_and_limited() {
    let (app, mut sealer) = built(2, 1_000).await;
    // まだアンカーが無い。
    let (status, headers, body) = get_with_headers(&app, "/api/v1/anchors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["anchors"], json!([]));
    assert_eq!(cache_control(&headers), "no-cache");
    for i in 0..3 {
        seal_one_by_one(&app, &mut sealer, i..i + 1).await;
        sealer.anchor().await.expect("anchor");
    }
    let (_, _, body) = get_with_headers(&app, "/api/v1/anchors").await;
    let seqs: Vec<u64> = body["anchors"]
        .as_array()
        .expect("anchors")
        .iter()
        .map(|a| a["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs.len(), 3);
    assert!(seqs.windows(2).all(|w| w[0] > w[1]), "{seqs:?}");
    // 各アンカーは、シャードごとの先頭（シャードと高さ）を持ち、ブロックの詳細へたどれる。
    let heads = body["anchors"][0]["heads"].as_array().expect("heads");
    assert_eq!(heads.len(), 2);
    let (shard, height) = (
        heads[0]["shard"].as_u64().expect("shard"),
        heads[0]["height"].as_u64().expect("height"),
    );
    let (status, _, block) =
        get_with_headers(&app, &format!("/api/v1/chains/{shard}/blocks/{height}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(block["block_hash"], heads[0]["block_hash"]);
    // limit。
    let (_, _, body) = get_with_headers(&app, "/api/v1/anchors?limit=2").await;
    assert_eq!(body["anchors"].as_array().expect("anchors").len(), 2);
    assert_eq!(body["anchors"][0]["seq"], json!(seqs[0]));
}

// ===========================================================================
// 選挙状態と投票の受付期間（原則17・18）
// ===========================================================================

/// `phase` まで、正しい順（scheduled → open → closing → closed）で遷移させたアプリを作る。
async fn app_at_phase(now: u64, period: Period, phase: ElectionPhase) -> Router {
    let mut cfg = config(1);
    cfg.period = period;
    let Built { state, .. } = build(
        &cfg,
        Arc::new(FixedClock(now)),
        Arc::new(ManualClock::new()),
    )
    .await
    .expect("build");
    let steps: &[ElectionPhase] = match phase {
        ElectionPhase::Scheduled => &[],
        ElectionPhase::Open => &[ElectionPhase::Open],
        ElectionPhase::Closing => &[ElectionPhase::Open, ElectionPhase::Closing],
        ElectionPhase::Closed => &[
            ElectionPhase::Open,
            ElectionPhase::Closing,
            ElectionPhase::Closed,
        ],
    };
    let mut current = ElectionPhase::Scheduled;
    for &next in steps {
        state
            .election_state
            .transition(
                current,
                next,
                ElectionRules { allow_blank: true },
                "test",
                0,
            )
            .await
            .expect("advance");
        current = next;
    }
    app(state)
}

#[tokio::test]
async fn election_status_reports_phase_and_period() {
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };
    let app = app_at_phase(1_500, period, ElectionPhase::Open).await;
    let (status, body) = send(&app, "GET", "/api/v1/election-status", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["phase"], json!("open"));
    assert_eq!(body["opens_at"], json!(1_000));
    assert_eq!(body["closes_at"], json!(2_000));
    assert_eq!(body["now"], json!(1_500));
    assert_eq!(body["display_timezone"], json!("Asia/Tokyo"));
    assert_eq!(body["display_timezone_offset_secs"], json!(9 * 3600));
}

#[tokio::test]
async fn scheduled_rejects_voting_with_the_configured_message() {
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };
    let app = app_at_phase(500, period, ElectionPhase::Scheduled).await;
    let token = login(&app, "alice").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "voting_not_started");
    assert_eq!(body["message"], json!("投票の受付はまだ開始していません"));
}

#[tokio::test]
async fn open_but_before_the_opening_time_rejects_voting() {
    // open --now で、期間の開始前に手動で開けた場合（原則18: 状態だけでなく期間も見る）。
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };
    let app = app_at_phase(500, period, ElectionPhase::Open).await;
    let token = login(&app, "alice").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "voting_not_started");
}

#[tokio::test]
async fn the_opening_instant_is_accepted_and_the_closing_instant_is_rejected() {
    // 期間の境界: 開始時刻ちょうどは受け付け、終了時刻ちょうどは拒否する。
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };

    let opening = app_at_phase(1_000, period, ElectionPhase::Open).await;
    let token = login(&opening, "alice").await;
    let (status, _) = send(
        &opening,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "開始時刻ちょうどは受け付ける");

    let closing_instant = app_at_phase(2_000, period, ElectionPhase::Open).await;
    let token = login(&closing_instant, "alice").await;
    let (status, body) = send(
        &closing_instant,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "終了時刻ちょうどは拒否する");
    assert_error(&body, "voting_closed");
}

#[tokio::test]
async fn closing_rejects_voting_with_its_own_message() {
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };
    let app = app_at_phase(2_500, period, ElectionPhase::Closing).await;
    let token = login(&app, "alice").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "voting_closing");
}

#[tokio::test]
async fn closed_rejects_voting_with_its_own_message() {
    let period = Period {
        opens_at: Some(1_000),
        closes_at: Some(2_000),
    };
    let app = app_at_phase(9_000, period, ElectionPhase::Closed).await;
    let token = login(&app, "alice").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("{SMD1}/vote"),
        Some(&token),
        vote_body(&candidate("shugiin_smd.13.01", 1)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "voting_closed");
}
