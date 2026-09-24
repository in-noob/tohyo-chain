# 0003: ブロック封印ポリシーの判定を純粋関数にする

## 背景
原則9のブロック封印ルール（件数到達・時間満了・0 件満了・フラッシュ）は、sealer が
シャードごとに実行する。時計や DB に依存する形で書くと、境界条件のテストが難しく、
タイミングによる不具合（票の取りこぼし、空ブロックの生成による到着時刻の漏洩）を検出しにくい。

## 決定
- `domain::seal_policy::decide(pending, window_start, now, &SealPolicy) -> SealDecision` を純粋関数とする。
  時刻は `u64` 秒の引数で受け取り、時計・IO・状態は持たない。
- 判定順: (1) `pending >= max_ballots` なら `SealCount(max_ballots)`（最優先）、
  (2) `now - window_start >= max_interval_secs` なら 1 件以上で `SealAll`、0 件で `ResetWindowOnly`、
  (3) それ以外は `Wait`。境界は「以上」（ちょうど 600 秒で満了）。
- 締切・正常停止時のフラッシュは `decide_flush(pending)` に分ける（1 件以上で `SealAll`、0 件で `ResetWindowOnly`）。
- 窓と未封印件数の更新は呼び出し側（sealer）の責務とし、判定後の状態更新は純粋関数
  `SealDecision::next_window_start` / `pending_after` として提供する。
- `now < window_start`（時計の巻き戻り）は `saturating_sub` で経過 0 とし、panic しない。
- `SealPolicy::new` は `max_ballots == 0`（`SealCount(0)` の無限ループ）と `max_interval_secs == 0` を拒否する。
  環境変数（`SEAL_MAX_BALLOTS` / `SEAL_MAX_INTERVAL_SECS`）の読み取りは IO なので domain に置かず sealer で行う。

## 理由
- 純粋関数なら、99 件・599 秒のような境界を時計なしで網羅的にテストできる。
- 件数到達を優先し、超過分を繰り越すことで、1 ブロックの票数の上限を常に守れる。
- 0 件で満了したらブロックを作らないので、「いつ票が入らなかったか」が空ブロックとして残らない（原則9）。
- フラッシュを別関数にすると、`decide` の引数を増やさずに、時間・件数と独立した全件封印を表現できる。

## 代替案
- **`decide` に `flush: bool` を追加**: 通常運転と終了処理が 1 つの分岐に混ざり、
  通常運転の呼び出しで誤って `true` を渡す余地が生まれるため不採用。
- **窓の状態を持つ構造体（`&mut self` のステートマシン）**: テスト時に状態の組み立てが必要になり、
  「純粋関数として実装し、時刻は引数で受け取る」という原則に反するため不採用。
- **`std::time::Instant` / `SystemTime` を引数に取る**: 単体テストで任意の時刻を作りにくいため、秒の整数にした。
