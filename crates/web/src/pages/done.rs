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
///
/// 再投票を認める選挙で、すべての投票用紙に投票済みで、投票期間内なら、「投票をやり直す」（`labels.revote_button`）も
/// 出す（ADR 0022。`flow::can_revote`）。
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

    // やり直し: すべて投票済みで、再投票を受け付けているときだけ。
    let revote = state.ballots.with_untracked(|ballots| {
        ballots.as_deref().is_some_and(|list| {
            state
                .revote
                .with_untracked(|r| flow::can_revote(list, r.as_ref()))
        })
    });
    let navigate_revote = navigate.clone();
    let on_revote = move |_| {
        state.last_voted.set(None);
        navigate_revote(&Page::Revote.path(), replace());
    };

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
            <div class="actions">
                {revote.then(|| view! {
                    <button class="secondary" on:click=on_revote>{crate::labels::revote_button()}</button>
                })}
                <button on:click=on_continue>{label}</button>
            </div>
        </section>
    }
}
