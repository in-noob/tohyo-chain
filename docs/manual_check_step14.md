# Step 14 手動確認チェックリスト（ブロックチェーンのビューア）

自動テスト（`scripts/check/chain.sh` の #7 と `scripts/check/web.sh` の #3）では、API（ページ送り・締切前後の票の公開・Cache-Control）と、画面のビルドまでを検証している。
ブラウザでの見た目・操作・リンクのたどり方は、このチェックリストで確認する。
各項目は、確認できたら `[x]` にする。

## 0. 準備

- [ ] 前提: [environment.md](environment.md) の手順で Rust と Trunk が入っている
- [ ] 小さな選挙データを生成する（有権者 `voter-1`〜`voter-30`）:
  ```
  cargo run -q -p seedgen -- --out /tmp/seed-demo --election-id 2026-general --prefectures 3 --districts-per-pref 2 \
    --candidates-per-district 4 --voters 30 --municipalities-per-pref 2 --pref-assembly-districts-per-pref 2 --force
  ```
- [ ] 締切を「数分後」にして起動し、投票の受付を始める（票の公開が、締切前後で変わることを確かめるため）:
  ```
  export APP__ELECTION__SEED_DIR=/tmp/seed-demo
  export APP__SEAL__MAX_BALLOTS=3            # 3 票ごとにブロックが増える
  export APP__SEAL__INTERVAL_SECS=10         # アンカーも 10 秒ごと（変化があれば）
  export APP__CHAIN__REVEAL_BALLOTS=after_close
  export APP__ELECTION__VOTING_CLOSES_AT=$(date -u -d '+5 minutes' +%Y-%m-%dT%H:%M:%SZ)
  scripts/dev_up.sh                          # api 18080・画面 http://localhost:8080
  scripts/election.sh open --now --yes
  ```
- [ ] 票を 30 票入れる（ブロックが 10 個できる）。各有権者が、名簿の 1 番目の選挙区に投票する:
  ```
  for n in $(seq 1 30); do
    t=$(curl -s -X POST localhost:18080/api/v1/login -H 'Content-Type: application/json' -d "{\"voter_id\":\"voter-$n\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    d=$(sed -n "$((n+1))p" /tmp/seed-demo/2026-general/voters.csv | cut -d, -f2 | cut -d';' -f1)
    curl -s -o /dev/null -w '%{http_code} ' -X POST "localhost:18080/api/v1/contests/2026-general/$d/vote" \
      -H "Authorization: Bearer $t" -H 'Content-Type: application/json' -d "{\"candidate_id\":\"$d.c1\"}"
  done; echo
  ```
  （201 が並ぶ。締切までに終わらせる。ブロックが 20 個を超えるページ送りを見たいときは、`--voters` を増やし、
  ループの範囲を広げる）
- [ ] 開発者ツール（F12）を開き、Console にエラーが出ていない

## 1. ログイン不要で開ける

- [ ] ログインしていない状態で、ヘッダの「ブロックチェーン」をクリックすると `/chain` が開く（ログイン画面へ飛ばされない）
- [ ] `http://localhost:8080/chain` を直接開いても表示される（リロードしても同じ）
- [ ] ログインしている状態でも、同じ画面が見られる

## 2. シャードの一覧（/chain）

- [ ] シャードごとに、先頭（最新）ブロックの高さ・票数・封印時刻（`2026-09-21 10:15 UTC` の形式で、分まで）・ハッシュ（先頭 8 文字…末尾 8 文字）が並ぶ
- [ ] ハッシュにマウスを載せると、全体が見える（title）
- [ ] 「署名者の公開鍵」が表示される
- [ ] 「アンカーの一覧」へのリンクがある
- [ ] 下に「このチェーンを検証するには」があり、`cargo run -q -p verifier -- verify --api http://localhost:8080` が表示される
      （`--api` は、いま開いている画面のオリジン。手元の端末でコピーして実行できる。api を直接指すなら `http://localhost:18080`）

## 3. ブロックの一覧（/chain/0）

- [ ] 「シャード 0」をクリックすると、ブロックが**新しい順**（高さの大きい順）に並ぶ
- [ ] ブロックが 20 個を超えるときは、下に「さらに古いブロックを表示」があり、押すと古いブロックが続きに追加される
      （重複も欠落もない。最後まで行くとボタンが消える）
- [ ] ブロックが 20 個以下のときは、ボタンが出ない
- [ ] 存在しないシャード（`/chain/9`）を開くと、エラーの案内が出る（画面が壊れない）。`/chain/abc` は「見つかりません」

## 4. ブロックの詳細（締切前）

- [ ] 高さをクリックすると `/chain/0/blocks/<高さ>` が開く
- [ ] 高さ・ブロックハッシュ（全体）・前のブロックのハッシュ・Merkle 根・票数・封印時刻（分単位）・署名・署名者の公開鍵が表示される
- [ ] **票の一覧は出ず**、「票の中身（N 票）は、投票の締切後に公開されます」と表示される
- [ ] 開発者ツールの Network で、`/api/v1/chains/0/blocks/<高さ>` の応答に `candidate_id`・`ballot_id` が含まれない。
      `Cache-Control: no-store`
- [ ] 「← 前のブロック（高さ N-1）」をクリックすると、前のブロックの詳細が開く。これを繰り返して、高さ 0（ジェネシス）まで戻れる
- [ ] 高さ 0 では「（ジェネシス: 前のブロックはありません）」と表示され、前へのリンクがない。前のハッシュは 0 のみ
- [ ] 上の「ブロックチェーン › シャード 0 › 高さ N」のパンくずで、一覧に戻れる
- [ ] 存在しない高さ（`/chain/0/blocks/9999`）は、「見つかりません」の案内になる

## 5. アンカーの一覧（/chain/anchors）

- [ ] 「アンカー #N」が新しい順に並び、作成時刻・ハッシュ・前のアンカーのハッシュが表示される
- [ ] 各アンカーに「シャード 0 の高さ N」のリンクがあり、クリックするとそのブロックの詳細が開く
      （詳細のブロックハッシュが、アンカーの一覧で表示されたものと同じ）
- [ ] ブロックがまだ無いときは、「まだアンカーがありません」と表示される

## 6. 締切後

- [ ] 締切の時刻（`APP__ELECTION__VOTING_CLOSES_AT`）を過ぎてから、ブロックの詳細を開き直す（api の再起動は不要）
- [ ] 「票の一覧（3 票。ballot_id のハッシュ順）」の表が表示される。列は、番号・選挙区の名前・候補者の名前（政党）・ballot_id（短縮）
- [ ] 選挙区と候補者の**表示名**が出る（ID ではない）
- [ ] 開発者ツールの Network で、応答に `candidate_id`・`ballot_id`・`candidate_name` が含まれる。
      `Cache-Control: public, max-age=31536000, immutable`
- [ ] ページを再読み込みしても、票の一覧が表示される（ブラウザのキャッシュから）
- [ ] ジェネシス（高さ 0）は、「このブロックに票はありません」と表示される
- [ ] 締切後に、コマンド `cargo run -q -p verifier -- verify --api http://localhost:18080` が「検証 OK」になる
      （締切前は、「票が非公開のため、締切後に実行してください」と表示されて、終了コード 4 になる）

## 7. 表示の崩れ・アクセシビリティ

- [ ] 画面幅を狭くして（スマートフォンの幅）も、ハッシュや表がはみ出さない（詳細の項目は縦に並ぶ）
- [ ] ダークモード（OS の設定）でも読める
- [ ] キーボード（Tab）だけで、リンク・「さらに古いブロックを表示」のボタンを操作できる。フォーカスが見える
- [ ] 画面のどこにも、投票者を特定できる情報（有権者の ID など）が出ていない

## 8. 締切を設定しない場合（`chain.reveal_ballots=always`、既定）

- [ ] `APP__CHAIN__REVEAL_BALLOTS` と `APP__ELECTION__VOTING_CLOSES_AT` を指定せずに api を起動し直すと、
      締切に関係なく、ブロックの詳細に票の一覧が表示される
- [ ] `chain.reveal_ballots=after_close` で `election.voting_closes_at` が空だと、api は起動時に、その旨のエラーで終了する
