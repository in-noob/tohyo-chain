//! `verifier demo`: ダミー票でハッシュチェーンを作り、検証と改ざん検出を実演する。

use anyhow::{Context, bail};
use domain::{
    Ballot, BallotId, Block, CandidateId, ContestId, DistrictId, Ed25519Signer, Hash32, genesis,
    inclusion_proof, seal_block, verify_chain, verify_inclusion,
};

/// デモで生成する票の総数と、ブロックごとの件数（合計が一致すること）。
const DEMO_BLOCK_SIZES: [usize; 3] = [100, 100, 50];
/// デモの封印時刻の起点（UNIX 分）。
const DEMO_BASE_MINUTE: u64 = 29_000_000;

/// 先頭 8 バイトの hex。
fn short_hex(hash: &Hash32) -> String {
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn print_table(chain: &[Block]) {
    println!(
        "{:>6} | {:>7} | {:<16} | {:<16}",
        "height", "ballots", "prev_hash", "block_hash"
    );
    println!("{:-<6}-+-{:-<7}-+-{:-<16}-+-{:-<16}", "", "", "", "");
    for block in chain {
        println!(
            "{:>6} | {:>7} | {:<16} | {:<16}",
            block.header.height,
            block.ballots.len(),
            short_hex(&block.header.prev_hash),
            short_hex(&block.block_hash),
        );
    }
}

/// ダミー票を `total` 件作る。候補者は 1〜5 を巡回させる。
fn dummy_ballots(total: usize) -> Vec<Ballot> {
    (0..total)
        .map(|i| Ballot {
            ballot_id: BallotId::from_random_bytes(rand::random()),
            contest_id: ContestId::parse("2026-general/shugiin_smd.13.01").expect("valid"),
            candidate_id: CandidateId::parse(&format!("shugiin_smd.13.01.c{}", i % 5 + 1))
                .expect("valid"),
            revote: None,
        })
        .collect()
}

pub fn run() -> anyhow::Result<()> {
    let signer = Ed25519Signer::from_seed(&rand::random());
    let verifier = signer.verifier();

    // (1) ダミー票 250 件から、ジェネシス + 100/100/50 件の 3 ブロックを作る。
    println!("== (1) チェーンの生成");
    let mut ballots = dummy_ballots(DEMO_BLOCK_SIZES.iter().sum()).into_iter();
    let mut chain = vec![genesis(&signer, DEMO_BASE_MINUTE)];
    for (i, size) in DEMO_BLOCK_SIZES.iter().enumerate() {
        let group: Vec<Ballot> = ballots.by_ref().take(*size).collect();
        let prev = chain.last().context("チェーンが空です")?;
        let block = seal_block(prev, group, DEMO_BASE_MINUTE + 1 + i as u64, &signer)
            .context("ブロックの封印に失敗しました")?;
        chain.push(block);
    }
    print_table(&chain);

    // (2) 改ざん前の検証。
    println!("\n== (2) 検証");
    verify_chain(&chain, &verifier).context("改ざん前のチェーンの検証に失敗しました")?;
    println!("検証 OK: {} ブロック（ジェネシス + 3）", chain.len());

    // (3) 2 番目のブロック（height=2）の 1 票の candidate_id を書き換える。
    println!("\n== (3) 改ざんと再検証");
    const TARGET_HEIGHT: usize = 2;
    const TARGET_INDEX: usize = 7;
    let original = chain[TARGET_HEIGHT]
        .ballots
        .get(TARGET_INDEX)
        .context("対象の票が見つかりません")?
        .clone();
    // 形式は正しい別の投票先に書き換える（候補者は連番を 1 つ進め、白票は 1 番目の候補者にする）。
    let district = DistrictId::new(original.contest_id.district_part())
        .context("投票用紙の選挙区が不正です")?;
    let seq = match &original.candidate_id {
        CandidateId::Blank => 1,
        CandidateId::Candidate(code) => code.sequence() + 1,
    };
    let forged = Ballot {
        candidate_id: CandidateId::new(&district, seq).context("候補者 ID を作れません")?,
        ..original.clone()
    };
    // 検証済みのチェーンは残したまま、複製に対して改ざんする。
    let mut tampered = chain.clone();
    tampered[TARGET_HEIGHT].ballots[TARGET_INDEX] = forged.clone();
    println!(
        "height={TARGET_HEIGHT} のブロックの票 #{TARGET_INDEX} の candidate_id を書き換えました"
    );
    match verify_chain(&tampered, &verifier) {
        Err(e) => println!(
            "改ざん検出: height={} で {} ({e})",
            e.height()
                .map_or_else(|| "-".to_string(), |h| h.to_string()),
            e.kind(),
        ),
        Ok(()) => bail!("改ざんを検出できませんでした"),
    }

    // (4) 元のブロックで作った包含証明は、書き換えた票では成立しない。
    println!("\n== (4) Merkle 包含証明");
    let block = &chain[TARGET_HEIGHT];
    let proof = inclusion_proof(&block.ballots, TARGET_INDEX).context("証明を作れません")?;
    if !verify_inclusion(&original, &proof, &block.header.merkle_root) {
        bail!("正規の票の包含証明が失敗しました");
    }
    println!("正規の票: 包含証明 OK");
    if verify_inclusion(&forged, &proof, &block.header.merkle_root) {
        bail!("改ざんした票の包含証明が成立してしまいました");
    }
    println!("改ざんした票: 包含証明 失敗（期待どおり）");
    Ok(())
}
