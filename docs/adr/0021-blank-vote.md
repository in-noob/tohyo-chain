# 0021: 白票（`candidate_id` の予約値 `blank`）と、選挙のルール `vote.allow_blank` の open 時点での固定

ADR 0016（集計）の「白票（無効票）= 選挙区の候補者ではない票。白票を投じる手段は作らない」を置き換える。
原則19（選挙のルールは open に移った時点で固定する）の、最初の実装でもある（固定するのは、今は白票の可否だけ）。

## 背景
- 有権者が「どの候補者にも投票しない」ことを、明示的に選べるようにしたい（画面・API）。
- 既存の実装を確認したところ、domain に白票を表す値は無く、集計（`verifier tally`）は「投票用紙の候補者ではない票」を
  白票（無効票）として数えていた。API はそのような票を受け付けない（422）ので、実際には常に 0 だった。明示的な白票を
  加えると、「有権者が選んだ白票」と「あり得ない票（選挙データの取り違え・不具合）」が同じ数に混ざってしまう。
- 白票を認めない選挙もある。認めるかどうかは選挙のルールなので、投票の途中で変わってはいけない（原則19）。

## 決定
- **ID**: 票の投票先を `domain::CandidateId` の `enum`（`Blank` / `Candidate(CandidateCode)`）にした。候補者コード
  （`{district_id}.c{連番}`）は `CandidateCode` という別の型にし、選挙データの `Candidate::id` はこの型にした。
  文字列表現は、候補者なら候補者コード、白票なら予約値 `blank`（`domain::BLANK_CANDIDATE_ID`。web は domain に依存しないので
  `shared_types::BLANK_CANDIDATE_ID` にも置き、一致をテストで確かめる）。
  - `CandidateCode::parse("blank")` は `IdError::Reserved` で失敗する。seed の `candidates/*.csv` に `blank` があれば、
    読み込み時に、ファイル・行・理由（白票の予約値）つきのエラーになる。
  - `Contest::accepts(choice, allow_blank)`: 白票は `allow_blank` のときだけ、候補者はその選挙区の候補者のときだけ受け付ける。
- **チェーン**: 白票の票は、`candidate_id` に `blank` をそのまま入れる。正規化形式（原則4・ADR 0013 の長さ接頭辞つき文字列）は
  変わらないので、ブロックの形式の版（`BLOCK_VERSION`）は上げない。
- **API**: `POST …/vote` の `candidate_id` に `"blank"`（小文字の完全一致）で白票。`GET …/candidates` は、候補者の一覧
  （白票は含めない）と `allow_blank` を返す。白票を認めない選挙での白票は 422 `blank_not_allowed`（文言は `labels.blank_name` から）。
  ビューアのブロックの詳細（`BallotDto`）は、白票に `blank: true` を付け、候補者の表示名は付けない（締切前の、票を伏せた詳細には
  何も出ない）。
- **画面**: 候補者一覧の最後に、白票の選択肢（`labels.blank_option`）を、候補者と区切って（余白と破線の枠）置く。`allow_blank` が
  偽なら出さない。確認画面は、候補者名の型（「「○○」に投票します」）ではなく `labels.blank_confirm` をそのまま出す。これらは
  `web::flow` の純粋関数（`choices`・`confirm_message`）にして、`cargo test` で確かめる。ビューアは、白票の票を `labels.blank_name`
  で、斜体・破線の区切りで表示し、ブロックの「投票先別の票数」でも、候補者の後の別の行にする。
- **集計**: 白票（`CandidateId::Blank`）は候補者とは別に数える（表では候補者の後の、順位の無い行。CSV は白票の列。
  `candidates.csv` には入れない。有効票 + 白票 = 合計）。投票用紙の候補者でも白票でもない票がチェーンにあれば、
  選挙データに無い投票用紙の票と同じく、集計を中止する（終了コード 3。`TallyError::ForeignCandidate`）。
  白票の呼び名は `labels.blank_name`（既定「白票」。以前の固定の「白票（無効票）」は廃止）。
- **設定と固定**: `vote.allow_blank`（既定 `true`）。`domain::ElectionRules { allow_blank }` を、選挙状態が open に遷移する
  **その書き込み**（DB では `election_state` の LWT `UPDATE … SET phase, opened_at, allow_blank … IF phase = 'scheduled'`、
  memory ではプロセス内の同じ更新）で保存する。`ElectionStateStore::transition` にルールを渡し、open への遷移のときだけ保存する
  （他の遷移では無視する）。固定するのは、open に遷移させたプロセス（db モードはアンカーのリースを持つ sealer、memory モードは
  api 内蔵のスケジューラ、`open --now` は api）の設定の値。
  以後は `ElectionRules::effective(固定した値, 設定の値)` で、固定した値を使う（api の投票の判定・候補者の一覧）。起動時に、
  固定した値と設定が違っていれば警告する（期間の食い違いと同じ扱い）。固定前（scheduled）は、設定の値を返す（投票は受け付けない）。

## 理由
- `enum` にすると、集計・表示・改ざんデモなど、投票先を扱うすべての箇所で、白票の扱いを `match` で決めることがコンパイラに
  強制される。文字列の比較（`== "blank"`）だと、扱い漏れがあっても気づけない。選挙データの候補者を `CandidateCode` にしたので、
  「候補者の一覧に白票が紛れ込む」ことも型の上で起きない。
- 予約値を ID の形式（`{district_id}.c{連番}`）と重ならない文字列にしたので、既存の候補者コードと衝突しない。そのうえで、
  seed でも明示的に拒否し、エラーの理由を分かるようにした。
- 「あり得ない票」を白票に混ぜず集計を止めるのは、選挙データに無い投票用紙の票の扱い（ADR 0016）と揃えるため。
- ルールを遷移と同じ条件付き書き込みで保存すると、「open になったのに、ルールが記録されていない」瞬間ができない。
  複数の sealer が同時に遷移を試みても、LWT で 1 つだけが成功し、その 1 つの値が固定される。

## 代替案
- `CandidateId` を文字列のまま、`is_blank()` だけを足す: 変更は小さいが、扱い漏れをコンパイラが見つけられないので不採用。
- 白票を、各選挙区の「候補者」として seed に加える: 候補者の一覧・順位・得票の多い順に白票が混ざり、「候補者とは別の行」に
  できない。seed を作る人が選挙区ごとに書く必要もあるので不採用。
- ルールを init（スキーマ作成後の最初の接続）で固定する: open までの間に設定を直せなくなる。原則19 の「open に移った時点」と
  も合わないので不採用。
- ルールを別の行・別の表に保存する: 遷移との原子性を失うので不採用。
- `allow_blank` を web のビルド時の値（`app-config web-env`）で渡す: 固定した値ではなく、ビルドした時点の設定になるので不採用
  （API が実行時に返す）。

## 残っていること
- 原則19 のうち、再投票の可否・上限と封印ルールの固定は未実装。
- 既存のキースペースは、`election_state` に `allow_blank` 列が無い。`ALTER TABLE <keyspace>.election_state ADD allow_blank boolean;`
  を一度実行するか、`scripts/db_reset.sh --all` で作り直す（`CREATE TABLE IF NOT EXISTS` は既存の表に列を足さない）。
