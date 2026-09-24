//! ブロックチェーンのビューア（`/chain` 以下。ログイン不要）。
//!
//! シャードの一覧 → ブロックの一覧 → ブロックの詳細 → 前のブロック、とたどれる。アンカーの一覧は、各アンカーが
//! 指している先頭ブロックへのリンクを持つ。表示の判断は `crate::chain`（純粋関数）に任せ、ここは描画だけ。

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;
use shared_types::{BlockSummaryDto, HeaderDto};

use crate::api;
use crate::chain::{self, ANCHORS_PATH, CHAIN_HOME};
use crate::error::{self, ApiFailure};
use crate::labels;

/// この画面を配信しているオリジン（検証コマンドの `--api` に使う）。取得できなければ空。
fn origin() -> String {
    window().location().origin().unwrap_or_default()
}

/// 経路の案内（パンくず）。
#[component]
fn Crumbs(children: Children) -> impl IntoView {
    view! {
        <nav class="crumbs" aria-label="現在の位置">
            <A href=CHAIN_HOME>"ブロックチェーン"</A>
            {children()}
        </nav>
    }
}

/// 「このチェーンを検証するには」。
#[component]
fn VerifyBox(#[prop(into)] public_key: Signal<Option<String>>) -> impl IntoView {
    let command = move || chain::verify_command(&origin(), public_key.get().as_deref());
    view! {
        <section class="verify-box">
            <h2>"このチェーンを検証するには"</h2>
            <p class="hint">
                "ブロックはハッシュでつながり、署名されています。次のコマンドで、全シャードのチェーンの検証と、"
                "投票済み記録との突合ができます（この画面の表示を信じずに、自分の手元で確かめられます）。"
            </p>
            <pre><code>{command}</code></pre>
            <p class="hint">
                "公開鍵は、API が返した値を使います。運営者が別の経路で公表した鍵を固定するには、"
                <code>"--public-key"</code>" を付けてください。"
            </p>
        </section>
    }
}

fn header_rows(header: &HeaderDto, block_hash: &str) -> impl IntoView + use<> {
    view! {
        <span>{format!("高さ {}", header.height)}</span>
        <span>{format!("{} 票", header.ballot_count)}</span>
        <span>{shared_types::time::format_minute_utc(header.sealed_at_minute)}</span>
        <code title=block_hash.to_string()>{chain::short_hash(block_hash)}</code>
    }
}

/// `/chain`: シャードの一覧と、それぞれの先頭ブロック。
#[component]
pub fn ChainIndexPage() -> impl IntoView {
    let data = RwSignal::new(None::<Result<shared_types::ChainsResponse, ApiFailure>>);
    leptos::task::spawn_local(async move { data.set(Some(api::chains().await)) });
    let signer = Signal::derive(move || {
        data.with(|d| {
            d.as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|c| c.signer_public_key.clone())
        })
    });

    view! {
        <section>
            <h1>"ブロックチェーン"</h1>
            <p class="hint">
                "封印された票のブロックを、だれでも確認できます。投票者を特定できる情報は含まれません。"
            </p>
            <p><A href=ANCHORS_PATH>"アンカーの一覧"</A></p>
            {move || match data.get() {
                None => view! { <p>"読み込み中…"</p> }.into_any(),
                Some(Err(failure)) => view! { <p class="error">{error::alert_text(chain::failure_message(failure))}</p> }.into_any(),
                Some(Ok(chains)) => {
                    let items = chains
                        .shards
                        .iter()
                        .map(|s| {
                            let link = chain::shard_path(s.shard);
                            let latest = match &s.head {
                                Some(head) => header_rows(&head.header, &head.block_hash).into_any(),
                                None => view! { <span class="hint">"まだブロックがありません"</span> }.into_any(),
                            };
                            view! {
                                <li>
                                    <A href=link>{format!("シャード {}", s.shard)}</A>
                                    {latest}
                                </li>
                            }
                        })
                        .collect_view();
                    view! {
                        <h2>"シャード"</h2>
                        <p class="hint">"それぞれの先頭（最新）のブロックです。"</p>
                        <ul class="chain-list">{items}</ul>
                        {chains.signer_public_key.as_ref().map(|key| view! {
                            <p class="hint">"署名者の公開鍵: "<code>{key.clone()}</code></p>
                        })}
                    }
                    .into_any()
                }
            }}
        </section>
        <VerifyBox public_key=signer />
    }
}

/// `/chain/:shard`: ブロックの一覧（新しい順・ページ送り）。
#[component]
pub fn ChainShardPage() -> impl IntoView {
    let params = use_params_map();
    let shard = move || {
        params
            .read()
            .get("shard")
            .and_then(|s| chain::parse_shard(&s))
    };

    let blocks = RwSignal::new(Vec::<BlockSummaryDto>::new());
    let next = RwSignal::new(None::<u64>);
    let error = RwSignal::new(None::<ApiFailure>);
    let loaded = RwSignal::new(false);
    let busy = RwSignal::new(false);

    // 開いたとき（と、シャードが変わったとき）、先頭のページを取り直す。
    Effect::new(move |_| {
        let Some(shard) = shard() else { return };
        blocks.set(Vec::new());
        next.set(None);
        error.set(None);
        loaded.set(false);
        leptos::task::spawn_local(async move {
            match api::blocks_page(shard, None).await {
                Ok(page) => {
                    next.set(page.next_before_height);
                    blocks.set(page.blocks);
                }
                Err(failure) => error.set(Some(failure)),
            }
            loaded.set(true);
        });
    });

    let load_more = move |_| {
        let (Some(shard), Some(before)) = (shard(), next.get_untracked()) else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            match api::blocks_page(shard, Some(before)).await {
                Ok(page) => {
                    next.set(page.next_before_height);
                    blocks.update(|list| list.extend(page.blocks));
                }
                Err(failure) => error.set(Some(failure)),
            }
            busy.set(false);
        });
    };

    view! {
        <section>
            {move || match shard() {
                None => view! {
                    <h1>"見つかりません"</h1>
                    <p><A href=CHAIN_HOME>"ブロックチェーンの一覧へ"</A></p>
                }.into_any(),
                Some(shard) => view! {
                    <Crumbs>" › "{format!("シャード {shard}")}</Crumbs>
                    <h1>{format!("シャード {shard} のブロック")}</h1>
                    <p class="hint">"新しいブロックから順に表示します。"</p>
                    {move || error.get().map(|f| view! { <p class="error" role="alert">{error::alert_text(chain::failure_message(f))}</p> })}
                    <ul class="chain-list">
                        <For
                            each=move || blocks.get()
                            key=|b| b.block_hash.clone()
                            children=move |b| {
                                let link = chain::block_path(shard, b.header.height);
                                view! {
                                    <li>
                                        <A href=link>{format!("高さ {}", b.header.height)}</A>
                                        <span>{format!("{} 票", b.header.ballot_count)}</span>
                                        <span>{shared_types::time::format_minute_utc(b.header.sealed_at_minute)}</span>
                                        <code title=b.block_hash.clone()>{chain::short_hash(&b.block_hash)}</code>
                                    </li>
                                }
                            }
                        />
                    </ul>
                    {move || (loaded.get() && error.with(Option::is_none) && blocks.with(Vec::is_empty))
                        .then(|| view! { <p>"まだブロックがありません。"</p> })}
                    {move || next.get().map(|before| view! {
                        <button class="secondary" disabled=move || busy.get() on:click=load_more>
                            {format!("さらに古いブロックを表示（高さ {} 以下）", before.saturating_sub(1))}
                        </button>
                    })}
                }.into_any(),
            }}
        </section>
    }
}

/// `/chain/:shard/blocks/:height`: ブロックの詳細。
#[component]
pub fn ChainBlockPage() -> impl IntoView {
    let params = use_params_map();
    let target = move || {
        let p = params.read();
        Some((
            chain::parse_shard(&p.get("shard")?)?,
            chain::parse_height(&p.get("height")?)?,
        ))
    };
    let data = RwSignal::new(None::<Result<shared_types::BlockDto, ApiFailure>>);

    Effect::new(move |_| {
        let Some((shard, height)) = target() else {
            return;
        };
        data.set(None);
        leptos::task::spawn_local(async move { data.set(Some(api::block(shard, height).await)) });
    });

    view! {
        <section>
            {move || match target() {
                None => view! {
                    <h1>"見つかりません"</h1>
                    <p><A href=CHAIN_HOME>"ブロックチェーンの一覧へ"</A></p>
                }.into_any(),
                Some((shard, height)) => view! {
                    <Crumbs>
                        " › "<A href=chain::shard_path(shard)>{format!("シャード {shard}")}</A>
                        {format!(" › 高さ {height}")}
                    </Crumbs>
                    <h1>{format!("ブロック（シャード {shard}・高さ {height}）")}</h1>
                    {move || match data.get() {
                        None => view! { <p>"読み込み中…"</p> }.into_any(),
                        Some(Err(f)) => view! { <p class="error" role="alert">{error::alert_text(chain::failure_message(f))}</p> }.into_any(),
                        Some(Ok(block)) => block_detail(shard, block).into_any(),
                    }}
                }.into_any(),
            }}
        </section>
    }
}

fn block_detail(shard: u16, block: shared_types::BlockDto) -> impl IntoView {
    // 返す `view!` は `'static` でなければならないので、`block` を借用せず、必要な値を所有して持ち出す
    // （借用のままだと、関数を抜けるときに `block` が破棄されて、ビューが dangling になる）。
    let h = block.header.clone();
    let prev = chain::prev_block_path(shard, h.height);
    let prev_height = h.height.saturating_sub(1);
    let genesis = chain::is_genesis(&h);
    let minute = shared_types::time::format_minute_utc(h.sealed_at_minute);
    let notice = chain::hidden_ballots_notice(&block);
    let rows = chain::ballot_rows(shard, &block.ballots, labels::blank_name());
    let counts = chain::choice_counts(&block.ballots, labels::blank_name());
    let ballots = if block.ballots_revealed {
        // 白票は候補者ではないので、別の書式（class "blank"）で表示する。
        let body = rows
            .into_iter()
            .map(|r| {
                // 行の id は、再投票の票からのリンク（「#<前の票> を置き換え」）の飛び先。
                view! {
                    <tr class=r.blank.then_some("blank") id=chain::ballot_anchor(&r.ballot_id)>
                        <td>{r.index}</td>
                        <td>{r.district}</td>
                        <td>
                            {r.candidate}
                            {r.replaces.map(|link| view! {
                                <br /><a class="replaces" href=link.path>{link.label}</a>
                            })}
                        </td>
                        <td><code title=r.ballot_id.clone()>{chain::short_hash(&r.ballot_id)}</code></td>
                    </tr>
                }
            })
            .collect_view();
        // 投票先別の件数。白票は、候補者の後の別の行。
        let summary = counts
            .into_iter()
            .map(|c| {
                view! {
                    <tr class=c.blank.then_some("blank")>
                        <td>{c.district}</td>
                        <td>{c.choice}</td>
                        <td>{c.count}</td>
                    </tr>
                }
            })
            .collect_view();
        view! {
            <h2>{format!("票の一覧（{} 票。ballot_id のハッシュ順）", block.ballots.len())}</h2>
            {(block.ballots.is_empty()).then(|| view! { <p>"このブロックに票はありません（ジェネシスなど）。"</p> })}
            <table class="ballots">
                <thead><tr><th>"#"</th><th>"選挙区"</th><th>"投票先"</th><th>"ballot_id"</th></tr></thead>
                <tbody>{body}</tbody>
            </table>
            {(!block.ballots.is_empty()).then(|| view! {
                <h2>"このブロックの投票先別の票数"</h2>
                <table class="ballots counts">
                    <thead><tr><th>"選挙区"</th><th>"投票先"</th><th>"票数"</th></tr></thead>
                    <tbody>{summary}</tbody>
                </table>
            })}
        }
        .into_any()
    } else {
        view! {
            <h2>"票の一覧"</h2>
            <p class="notice" role="status">{notice}</p>
        }
        .into_any()
    };

    view! {
        <dl class="block-detail">
            <dt>"高さ"</dt><dd>{h.height}</dd>
            <dt>"ブロックハッシュ"</dt><dd><code>{block.block_hash.clone()}</code></dd>
            <dt>"前のブロックのハッシュ"</dt>
            <dd>
                <code>{h.prev_hash.clone()}</code>
                {prev.map(|path| view! { <p><A href=path>"← 前のブロック（高さ "{prev_height}"）"</A></p> })}
                {genesis.then(|| view! { <span class="hint">"（ジェネシス: 前のブロックはありません）"</span> })}
            </dd>
            <dt>"Merkle 根"</dt><dd><code>{h.merkle_root.clone()}</code></dd>
            <dt>"票数"</dt><dd>{h.ballot_count}</dd>
            <dt>"封印時刻（分単位）"</dt><dd>{minute}</dd>
            <dt>"署名"</dt><dd><code>{block.signature.clone()}</code></dd>
            <dt>"署名者の公開鍵"</dt>
            <dd>{match &block.signer_public_key {
                Some(key) => view! { <code>{key.clone()}</code> }.into_any(),
                None => view! { <span class="hint">"（不明）"</span> }.into_any(),
            }}</dd>
        </dl>
        {ballots}
        <VerifyBox public_key=Signal::derive({
            let key = block.signer_public_key.clone();
            move || key.clone()
        }) />
    }
}

/// `/chain/anchors`: アンカーの一覧と、各アンカーが指す先頭ブロックへのリンク。
#[component]
pub fn ChainAnchorsPage() -> impl IntoView {
    let data = RwSignal::new(None::<Result<shared_types::AnchorsResponse, ApiFailure>>);
    leptos::task::spawn_local(async move { data.set(Some(api::anchors().await)) });

    view! {
        <section>
            <Crumbs>" › アンカー"</Crumbs>
            <h1>"アンカーの一覧"</h1>
            <p class="hint">
                "アンカーは、全シャードの先頭ブロックをまとめて署名したものです。あとからチェーンが巻き戻されたり、"
                "差し替えられたりしていないことの確認に使います。"
            </p>
            {move || match data.get() {
                None => view! { <p>"読み込み中…"</p> }.into_any(),
                Some(Err(f)) => view! { <p class="error" role="alert">{error::alert_text(chain::failure_message(f))}</p> }.into_any(),
                Some(Ok(list)) if list.anchors.is_empty() => view! {
                    <p>"まだアンカーがありません（ブロックが追加されると作られます）。"</p>
                }.into_any(),
                Some(Ok(list)) => {
                    let items = list.anchors.iter().map(|a| {
                        let links = chain::anchor_head_links(a)
                            .into_iter()
                            .map(|l| view! {
                                <li>
                                    <A href=l.path>{l.label}</A>
                                    " "<code title=l.block_hash.clone()>{chain::short_hash(&l.block_hash)}</code>
                                </li>
                            })
                            .collect_view();
                        view! {
                            <li class="anchor">
                                <strong>{format!("アンカー #{}", a.seq)}</strong>
                                <span>{shared_types::time::format_minute_utc(a.anchor_minute)}</span>
                                <code title=a.anchor_hash.clone()>{chain::short_hash(&a.anchor_hash)}</code>
                                <span class="hint">"前のアンカー: "<code title=a.prev_anchor_hash.clone()>{chain::short_hash(&a.prev_anchor_hash)}</code></span>
                                <ul class="head-links">{links}</ul>
                            </li>
                        }
                    }).collect_view();
                    view! { <ul class="chain-list anchors">{items}</ul> }.into_any()
                }
            }}
        </section>
    }
}
