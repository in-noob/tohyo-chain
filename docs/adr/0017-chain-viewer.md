# 0017: ブロックチェーンのビューア（API・画面）と、締切後の票の公開

## 背景
封印済みのブロックは公開データだが、確認手段は `head`・`blocks/{height}`・`anchors/latest` を個別に叩くしかなかった。
だれでも、シャードの一覧からブロック・アンカーをたどれて、検証の方法も分かる画面が要る。また、原則 14（投票期間中に票の中身や集計を見せない）
のため、`chain.reveal_ballots=after_close` を実装する必要がある。

## 決定
- **API**（認証なし）: `GET /chains`（シャードの一覧と先頭の要約）、`GET /chains/{shard}/blocks?before_height=&limit=`（票を含まない要約を
  新しい順に。`limit` は 1〜100 にそろえる（既定 20）、`before_height` は「それより低い」高さ、空文字は指定なし、不正な値は 400）、
  `GET /chains/{shard}/blocks/{height}`（詳細）、`GET /anchors?limit=`（新しい順）。ページの本文には「先頭の高さ」を入れない
  （入れると、同じ URL の応答が変わり、`immutable` にできない）。次のページは `next_before_height`（最後は省く）。
  `ChainRead` に、範囲読み取り（`blocks_before`・`latest_anchors`）を足した。既定の実装は、`head`/`block` の組み合わせ
  （テスト用のフェイクは変更不要）。Scylla は、クラスタリング順（高さの降順・seq の降順）を使った範囲読み取り（`LIMIT`）。
- **`reveal_ballots=after_close`**: 締切前の詳細は、票（`ballot_id`・`contest_id`・`candidate_id`）を返さず、`ballots: []`・
  `ballots_revealed: false`、ヘッダー・票数・ハッシュ・署名は返す。締切は `election.voting_closes_at` を api の時計で判定し
  （締切ちょうどから公開。再起動は不要）、締切が無い `after_close` は、設定の検証で拒否する。api の設定では、
  `RevealPolicy::{Always, AfterClose{closes_at}}` で持つ（締切なしの after_close を作れない型）。
  詳細の DTO（`BlockDto`）は、フィールドを足しただけ（`ballots_revealed`・`signer_public_key`・票の表示名）で、verifier は今までどおり読める。
- **Cache-Control**: 内容が確定した応答だけ `public, max-age=31536000, immutable`（票を返す詳細、`before_height` が先頭 + 1 以下の一覧）。
  **票を伏せた詳細に付けてはいけない**: 締切後に同じ URL の中身が変わるので、CDN が票なしの版を 1 年保持してしまう。
  伏せた詳細とエラー（404 を含む。まだ封印されていないブロックが、あとで成功する）は `no-store`。先頭が変わる一覧
  （`/chains`、`before_height` なし・先頭より先のページ、アンカーの一覧）は `no-cache`（毎回確認）。判定は純粋関数
  （`api::chain_view`）で、通常の `cargo test` で検証する。
- **画面**（Leptos。`/chain`・`/chain/{shard}`・`/chain/{shard}/blocks/{height}`・`/chain/anchors`。ログイン不要でルート保護の対象外）:
  一覧 → 詳細 → 前のブロックへのリンクでたどる。詳細に、高さ・ハッシュ・前のハッシュ・Merkle 根・票数・封印時刻（分単位、UTC）・署名・
  署名者の公開鍵。票の一覧は、公開しているときだけ（ハッシュ順、選挙区・候補者の表示名は API が選挙データから付ける。web は選挙データを持たない）。
  「このチェーンを検証するには」に、画面のオリジンを使った verifier のコマンドを表示する。表示の判断は `web::chain`（純粋関数）。
  UTC の暦計算は、api・verifier・web で同じ表示にするため、`shared_types::time` にまとめた（外部クレートを増やさない）。
- **verifier**: 票が伏せられている（`ballots_revealed=false`）と、`BallotsHidden` を返し、終了コード 4（「まだ実行できない」）で、
  「票が非公開のため、締切後に実行してください」と表示する。伏せられた応答を、そのまま検証すると、票が 0 件に見えて、改ざんと誤判定するため。
  ヘッダーだけの部分検証（`prev_hash` の連鎖と署名）は、この版では作らない。

## 理由
- 票を伏せる判断を api の 1 か所（`block_detail`）に置けば、他の経路（一覧・アンカー・`/chains`）は、そもそも票を持たないので漏れない。
- 確定した応答だけを `immutable` にすれば、CDN に載せても、締切前後の切り替えが壊れない。

## 代替案
- 締切前も `immutable` にして、締切後の URL を変える（クエリなど）: URL が増え、既存のリンクが締切前の版を指してしまうので不採用。
- verifier が、締切前はヘッダーだけを検証する: 票ごとの突合・集計ができず、検証の意味が薄いので、この版では見送り。
- 締切の判定を画面（ブラウザの時計）で行う: 票を返すか否かは、api が決めるべきなので不採用。
