# 0022: 投票期間中の再投票（slot・seq・supersedes と、締切での revote_key の破棄）

原則1（秘密投票。再投票のための仮名 slot だけは票と一緒に記録してよい）・原則18（受け付けは状態と期間の両方）・
原則19（選挙のルールは open の時点で固定）の、再投票の部分の実装。ADR 0004（シャードは ballot_id から決める）を、
再投票を認める選挙に限って置き換える。ブロックの形式の版を 3 に上げる（ADR 0013 の票の正規化形式に、後ろを足す）。

## 背景
- 投票期間中に、有権者が投票をやり直せるようにしたい（強要・買収への対策として、後から自分の意思で上書きできる）。
  再投票を認めるかどうかと、上限の回数は選挙のルールで、投票の途中で変わってはいけない（原則19）。
- 秘密投票（原則1）のため、票は投票者を特定する情報を持たない。一方で、再投票では「どの票が、どの票を置き換えたか」を
  検証できないと、集計で「最後の票だけを数える」ことを第三者が確かめられない。
- 既存の設計では、participation（誰がどの投票用紙に投票したか）と票（ballot_pool・blocks）を結ぶキーは無い。
  シャードは ballot_id から決めるので、同じ有権者の票は別々のチェーンに散る。

## 決定
- **設定と固定**: `vote.allow_revote`（既定 `false`）・`vote.max_revotes`（既定 5。1〜100。初回の投票を含めない）。
  `domain::ElectionRules { allow_blank, allow_revote, max_revotes }` を、open への遷移と同じ条件付き書き込み
  （DB は `election_state.allow_revote` / `max_revotes` の列。LWT）で固定する（ADR 0021 と同じ仕組み）。
- **slot**: `slot = HMAC-SHA256(revote_key, election_id ‖ voter_id ‖ contest_id)`（`application::RevoteKey::slot`）。
  各 ID の前に 2 バイトの固定幅・ビッグエンディアンの長さを置く（原則4 と同じく、境界の曖昧さをなくす）。
  - `revote_key` は `secrets/revote_key`（64 桁の hex。1 ファイル）にだけ置く。**環境変数では渡せない**（設定項目ではないので、
    `APP__VOTE__REVOTE_KEY` は未知の項目としてエラー）。ログ・DB・`Debug` には出さない（`RevoteKey` の `Debug` は伏せ字、
    破棄のときにメモリを 0 で上書き）。
  - `allow_revote=true` の選挙で、締切の手続きに入る前に鍵が無ければ、api は起動しない。
  - 再投票を認めない選挙では、slot を計算せず、記録もしない（票に `revote` を付けない。participation の `seq` も NULL）。
- **票**: `domain::Ballot` に `revote: Option<RevoteLink { slot, seq, supersedes }>` を足した。`seq` はその slot の何番目の票か
  （1 始まり）、`supersedes` は 1 つ前の版の票のハッシュ（初回は無し）。
  - 票のハッシュ（`domain::ballot_hash`）は Merkle 木の葉と同じ `SHA256(0x00 ‖ 正規化バイト列)`。正規化バイト列には
    `supersedes` 自体も入るので、同じ slot の票はハッシュの鎖でつながる。
  - 正規化エンコード（版 3）: 版 2 のバイト列の後ろに、`0x01 ‖ slot(32) ‖ seq(4)`（初回）または
    `0x02 ‖ slot(32) ‖ seq(4) ‖ supersedes(32)`（再投票）を足す。つながりの無い票は版 2 と同じバイト列なので、
    再投票を認めない選挙の Merkle 根は変わらない。`BLOCK_VERSION = 3`。
  - DB の `blocks.ballots` は `tuple<blob, text, text, blob>`（4 つ目が `encode_revote` のバイト列。無ければ NULL）。
- **シャード**: 再投票を認める選挙では `shard_for_slot(slot)`（`hash(slot) mod shard_count`）。同じ slot の全版が 1 本の
  チェーンに入るので、順序と前の票へのつながりを、1 本のチェーンの中で検証できる。認めない選挙は、これまでどおり ballot_id から。
- **participation と slot_state**:
  - participation に `seq`（受理した票の数）を持たせる。初回の投票は `INSERT … IF NOT EXISTS`（seq=1）、再投票は
    `UPDATE … SET seq = n+1, attempt = ? … IF seq = n`（LWT の比較更新）。同時に送られても 1 件だけが成功する。
  - `slot_state(slot → seq, last_ballot_hash)`。キーは voter_id ではなく slot。participation とは結ぶキーを持たない
    （participation の行から slot は計算できない。鍵が要る）。書き込み時刻は「分の先頭 + seq」（`USING TIMESTAMP`）:
    participation の LWT の書き込み時刻（マイクロ秒）と突き合わせて投票者と slot を結び付けられないようにし、同じ分の中でも
    後の版が必ず勝つようにした。
  - 書き込みの順序は participation → ballot_pool → slot_state。途中で失敗したら、逆順に取り消す（ベストエフォート）。
    participation と slot_state の seq が食い違っていたら、再投票は競合（409）として拒否する（壊れた鎖を作らない）。
- **API**: `POST …/vote` の `revote` に、画面が見た受理済みの票の数（`ballot-status` の `ballots_cast`）を入れて再投票する。
  **この値のときだけ再投票する**（二重送信・2 つの画面から同時に送っても、2 回やり直したことにならない）。
  - 省く（初回の投票）と、投票済みなら 409 `already_voted`（再投票を認める選挙でも、明示しないと再投票にしない）。
  - 409 `revote_not_allowed`（認めない選挙）・`revote_limit_reached`（上限。文言は `labels.revote_limit_reached`）・
    `revote_conflict`（競合）・`not_voted`（まだ投票していない）。受け付けの可否は初回の投票と同じ（原則18）。
  - `ballot-status` は、投票用紙ごとの `ballots_cast` と、認める選挙だけ `revote: { max_revotes, open }` を返す。
    前回の投票内容は、画面にも API にも返さない。
  - `election-status` は、固定した選挙のルール（`rules`）を返す（verifier が検証に使う。ルールは公開情報）。
- **締切**: 締切の手続き（closing）の中で、待ち時間（`state_cache_secs + request_timeout_secs`）が過ぎて投票の受け付けが
  確実に止まった後に、鍵をファイルごと破棄し（`sealer::destroy_revote_key`）、`election_audit` に
  `revote_key_destroyed`（列 `event`。`from = to = closing`）を 1 回だけ記録する。破棄に失敗したら closed に進めない。
  db モードはアンカーのリースを持つ sealer、memory モードは api 内蔵のスケジューラ（api と同じ鍵の置き場所を共有するので、
  メモリ上の写しも同時に消える）。db モードの api は、closing 以降を読んだらメモリ上の写しを捨てる。
- **封印の順序**: `ballot_pool` のクラスタリングキーを `(received_minute, seq, ballot_id)` にした（memory は到着順）。
  件数による封印（先頭から `max_ballots` 件）でも、次の版が前の版より先に封印されない。
- **verifier**:
  - `verify`: 全シャードの検証の後に、`domain::analyze_revotes` で、slot ごとに seq が 1 から連続・supersedes が 1 つ前の
    票のハッシュ・seq が `max_revotes + 1` 以下・認めない選挙に `seq > 1`（と slot）の票が無い・認める選挙に slot の無い
    票が無い・同じ slot の票が `hash(slot)` のシャードにあり投票用紙も同じ、を確かめる。失敗は検証失敗（終了コード 3）。
  - 突合: participation（投票した有権者の数）= チェーン内の slot の数（重複を除く。= 封印済みの票 − `seq > 1` の票）
    + 未封印の最初の票。さらに、受理した票の数（participation の seq の合計。`cast`）= 封印済み + 未封印の票。
    `cast` と `pending_initial` は、票を公開してよいとき（`chain.reveal_ballots`）だけ API が返す。
  - `tally`: slot ごとに最後の票だけを数える。締切後（closed）の集計だけ、再投票の件数と変更の内訳（前の票の投票先 →
    次の票の投票先の件数表）を、表・`revotes.csv`・`tally.json` の `revotes` に出す（中間集計では出さない）。
- **画面**: 完了画面に、すべて投票済みで、再投票を認める選挙の期間内なら「投票をやり直す」（`labels.revote_button`）。
  やり直しの一覧は、投票済みの投票用紙を固定の順番（表示順）で並べ、上限に達したものは選べず、理由
  （`labels.revote_limit_reached`）を ⚠ つきで出す。確認画面は「前回の投票内容を変更します」（`labels.revote_confirm`）。
  判定は `web::flow` の純粋関数（`can_revote`・`revote_items`・`guard` など）。
- **ビューア**: 票を公開している（締切後）ときだけ、再投票の票に「#<前の票> を置き換え（A→B）」のリンク（前の票がある
  ブロックの行）を出す。api が同じシャードのチェーンを高さの降順に読んで、前の版を探す（`chain_view::find_replaced`）。
  締切前（票を伏せた詳細）には、candidate も slot / seq / supersedes も含めない。

## 理由
- **slot を HMAC にして、鍵を締切で捨てる**: 投票期間中は、同じ有権者の票を 1 つの鎖にまとめる必要がある（api は鍵で slot を
  計算する）。締切後は、鍵が無ければ slot から投票者に戻せないので、公開したチェーンと participation を結び付けられない。
  投票用紙ごとに slot が違うので、1 人の有権者の別の投票用紙の票どうしも結び付かない。
- **ファイルだけ（環境変数は不可）**: 環境変数は起動したシェルやプロセスの外に残り、締切の手続きから消せない。
- **supersedes をハッシュにする**: 票の中身（前の票の投票先）を複製せずに、置き換えの順序を改ざんできない形で残せる。
  チェーンの Merkle 根に入るので、封印の後に鎖を付け替えると検証に失敗する。
- **seq を participation の LWT で守る**: 「同時に 2 つの再投票」の判定を、既存の二重投票の防止と同じ仕組み（LWT）に
  そろえた。画面が見た票の数を条件にしたので、要求が順番に処理されても（memory モード）、同時に処理されても（DB の LWT）、
  結果は同じ（1 件だけ成功）。
- **slot でシャードを決める**: 全版が 1 本のチェーンに入るので、verifier は、シャードをまたいで順序を合わせる必要がない。
  slot は投票者 ID に戻せない値なので、シャードが投票者と結び付くことはない（ADR 0004 の懸念は、鍵の破棄で解消する）。
- **版 2 のバイト列を変えない**: 再投票を認めない選挙の票・既存のチェーンの検証結果が変わらない。
- **変更の内訳を締切後だけ出す**: 期間中の心変わりの傾向は、中間の集計結果と同じく、投票の途中で公開しない（原則14）。

## 代替案
- **participation に slot や最後の票のハッシュを持たせる**: participation（voter_id）と票（ハッシュ）が 1 行で結び付くので不採用
  （原則1）。slot_state を別の表にし、キーを slot にした。
- **再投票のたびに、前の票をプール・チェーンから消す**: 封印済みのブロックは変えられない（ハッシュチェーン）。未封印の票だけを
  消すと、封印の時期によって「チェーンに残る版」が変わり、検証の結果が不定になる。すべての版を残し、集計で最後だけを数える。
- **seq を持たず、到着時刻で最後の票を決める**: 時刻は分に丸めて保存する（原則3）ので、同じ分の版を区別できない。
- **API が seq の条件を持たず、サーバの読み取り値だけで比較更新する**: 同時に処理された要求は 1 件だけになるが、順番に
  処理された要求（二重送信）は 2 件とも通ってしまう。画面が見た値を条件にした。
- **revote_key を DB に置く**: 締切後に DB から消しても、バックアップ・レプリカに残り得る。原則1 のとおり secrets/ に置く。
- **slot を voter_id のハッシュ（鍵なし）にする**: 名簿の voter_id を知っていれば、誰でも slot を計算して投票内容を特定できる。

## 残っていること
- 封印ルールの固定（原則19）は、まだ実装していない。
- 鍵の配布（複数ホストの api に同じ `secrets/revote_key` を置く）と、各ホストでの破棄は、運用で行う（このプロトタイプでは、
  締切の手続きを行うプロセスと同じ `secrets/` の鍵を消し、他の api はメモリ上の写しだけを捨てる）。
- 途中で落ちたとき（participation を更新した後、プール・slot_state の書き込みと取り消しの両方に失敗したとき）は、participation と
  slot_state の seq が食い違ったまま残り、その投票用紙の再投票は競合として拒否され続ける（票は失われない。突合で検出できる）。
- 既存のキースペースは、再投票の列・表・クラスタリングキーを持たない（`ballot_pool` の主キーは `ALTER TABLE` で変えられない）。
  api / sealer は接続時に検出して、起動を拒否する。`scripts/db_reset.sh --all` で作り直す。
