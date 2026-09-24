//! 投票のやり直し（再投票。ADR 0022）: 投票済みの投票用紙の一覧（固定の順番）から 1 枚を選ぶ画面。
//!
//! 前回の投票内容（投票先）は、API も返さないので、ここにも出さない。上限に達した投票用紙は選べず、理由
//! （`labels.revote_limit_reached`）を表示する。一覧は、開くたびに API から取り直す（別の画面でやり直した分を反映する）。

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::app::{AppState, replace, use_guard};
use crate::error::ApiFailure;
use crate::flow::{self, Route as Page};
use crate::labels;

#[component]
pub fn RevotePage() -> impl IntoView {
    let state = expect_context::<AppState>();
    use_guard(Page::Revote);
    let navigate = use_navigate();
    let loaded = RwSignal::new(false);

    Effect::new(move |_| {
        let Some(token) = state.token.get() else {
            return;
        };
        leptos::task::spawn_local(async move {
            match api::ballot_status(&token).await {
                Ok(status) => {
                    state.set_status(status);
                    loaded.set(true);
                }
                Err(ApiFailure::Unauthorized) => {
                    state.notice.set(Some(
                        "セッションの有効期限が切れました。もう一度ログインしてください。".into(),
                    ));
                    state.logout();
                }
                Err(_) => state.notice.set(Some(format!(
                    "{}の一覧を取得できませんでした。ページを開き直してください。",
                    labels::ballot_item()
                ))),
            }
        });
    });

    view! {
        <section>
            <h1>{labels::revote_button()}</h1>
            {move || {
                if !loaded.get() {
                    return view! { <p>"読み込み中…"</p> }.into_any();
                }
                let ballots = state.ballots.get().unwrap_or_default();
                let Some(revote) = state.revote.get().filter(|r| r.open) else {
                    // 再投票を認めない選挙、または期間外。
                    return view! {
                        <p class="notice" role="status">"現在、投票のやり直しは受け付けていません。"</p>
                        <p><a href=Page::Progress.path()>{format!("{}の進捗へ", labels::ballot_item())}</a></p>
                    }
                    .into_any();
                };
                let navigate = navigate.clone();
                let items = flow::revote_items(&ballots, revote.max_revotes, labels::revote_limit_reached())
                    .into_iter()
                    .map(|item| {
                        let navigate = navigate.clone();
                        let target = Page::RevoteBallot(item.contest_id.clone()).path();
                        match item.blocked {
                            // 上限に達したものは、ボタンを無効にし、理由を文字で示す（色だけに頼らない: ⚠）。
                            Some(reason) => view! {
                                <li class="blocked">
                                    <span class="contest-name">{item.name}</span>
                                    <span class="kind">{item.type_name}</span>
                                    <button class="secondary" disabled=true>"選べません"</button>
                                    <span class="reason">{crate::error::alert_text(&reason)}</span>
                                </li>
                            }
                            .into_any(),
                            None => view! {
                                <li>
                                    <span class="contest-name">{item.name}</span>
                                    <span class="kind">{item.type_name}</span>
                                    <button on:click=move |_| navigate(&target, replace())>
                                        {format!("やり直す（あと {} 回）", item.left)}
                                    </button>
                                </li>
                            }
                            .into_any(),
                        }
                    })
                    .collect_view();
                view! {
                    <p>{format!("やり直す{}を選んでください。前回の投票内容は表示しません。", labels::ballot_item())}</p>
                    <ul class="contest-list revote-list">{items}</ul>
                    <p><a href=Page::Progress.path()>{format!("{}の進捗へ", labels::ballot_item())}</a></p>
                }
                .into_any()
            }}
        </section>
    }
}
