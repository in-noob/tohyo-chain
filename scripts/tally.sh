#!/usr/bin/env bash
# 集計（verifier tally）。チェーン全体の検証と、投票済み記録との突合が成功したときだけ、集計する。
#
#   scripts/tally.sh [--api URL] [--public-key HEX] [--allow-interim] [--out DIR] [--all]
#
#   集計の前に必ず行うこと:
#     1. チェーン全体の検証と、投票済み記録との突合（失敗したら集計しない。終了コード 3）
#     2. 未封印の票が残っていたら、件数を表示して中止する（終了コード 4）。残りの票は、投票終了の手続き
#        （選挙状態 closing）の中でだけ封印される（sealer の停止ではフラッシュしない。原則9）ので、closed を待つ
#     3. 選挙状態（scripts/election.sh status で確認できる）が closed になる前は、--allow-interim を
#        付けない限り集計しない（終了コード 4）。--allow-interim は app.env=dev のときだけ使える
#        （終了コード 2。中間集計の漏洩を防ぐため）
#   出力: ターミナルに表形式で表示し、out/tally/{日時（UTC）}/ に CSV（districts / candidates / prefectures / types /
#   reconciliation）と tally.json を出力する（--out で親のディレクトリを変えられる）。
#
# api は、稼働中のものを使う（--api の既定は http://localhost:<api.port>）。設定は config/*.toml・環境変数 APP__…。
set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo run -q -p verifier -- tally "$@"
