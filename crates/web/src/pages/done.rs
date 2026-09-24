//! 投票完了画面。

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use crate::app::{AppState, replace, use_guard};
use crate::flow::{self, Continue, Route as Page};

/// 投票完了画面。
///
/// 表示するのは、設定 `labels.done_message`（既定は [`flow::DONE_MESSAGE`]:「投票を受け付けました」）**だけ**。
///
/// この文言は、票がサーバに受理されたことだけを表す。ブロックの封印（sealer が非同期に行う）や、
/// 封印後の改ざん検証の結果とは無関係で、「封印済み」「検証済み」を意味しない。
/// 秘密投票のため、候補者名・ballot_id・投票用紙の名前などは一切表示しない。
/// ボタンは次の操作のための案内であり、投票内容を示さない。
#[component]
pub fn DonePage() -> impl IntoView {
    let state = expect_context::<AppState>();
    use_guard(Page::Done);
    let navigate = use_navigate();

    // 直前に投票した投票用紙の「次」（表示順で、先頭の未投票）を決める。行き先の判断は flow に任せる。
    let next = state
        .ballots
        .get_untracked()
        .map(|ballots| flow::continue_after_vote(&ballots));
    let label = next.as_ref().map_or_else(
        || format!("{}の進捗へ", crate::labels::ballot_item()),
        |next| flow::continue_label(next, crate::labels::ballot_item()),
    );

    let on_continue = move |_| {
        state.last_voted.set(None);
        match &next {
            Some(Continue::Ballot(id)) => navigate(&Page::Ballot(id.clone()).path(), replace()),
            Some(Continue::Finish) => {
                state.logout();
                navigate(&Page::Login.path(), replace());
            }
            None => navigate(&Page::Progress.path(), replace()),
        }
    };

    view! {
        <section class="done">
            <h1 class="done-message" role="status">{crate::labels::done_message()}</h1>
            <button on:click=on_continue>{label}</button>
        </section>
    }
}
