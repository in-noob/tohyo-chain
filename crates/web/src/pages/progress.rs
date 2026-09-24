//! 進捗の画面（済み／今／これから）。**表示だけ**で、投票する順番は選べない（リンクはない）。
//!
//! 投票は、表示順の先頭の未投票のものから、順に進む。一覧は読み取り専用で、「続ける」ボタンは、先頭の未投票へ進む。

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::app::{AppState, replace, use_guard};
use crate::error::ApiFailure;
use crate::flow::{self, BallotState, Route as Page};
use crate::labels;

#[component]
pub fn ProgressPage() -> impl IntoView {
    let state = expect_context::<AppState>();
    use_guard(Page::Progress);
    let navigate = use_navigate();

    // 期間と今の状態（原則17・18）。取得できなくても、進捗の表示自体は継続する。
    let election = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(status) = api::election_status().await {
                election.set(Some(crate::election_status::summary_line(&status)));
            }
        });
    });

    // 一覧が無ければ取得する（ログイン直後は取得済み）。
    Effect::new(move |_| {
        let Some(token) = state.token.get() else {
            return;
        };
        if state.ballots.with_untracked(Option::is_some) {
            return;
        }
        leptos::task::spawn_local(async move {
            match api::ballot_status(&token).await {
                Ok(status) => state.set_status(status),
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
            <h1>{format!("{}の進捗", labels::ballot_item())}</h1>
            <p class="election-status hint">{move || election.get()}</p>
            {move || match state.ballots.get() {
                None => view! { <p>"読み込み中…"</p> }.into_any(),
                Some(ballots) if ballots.is_empty() => view! {
                    <p>{format!("投票できる{}がありません。", labels::ballot_item())}</p>
                }
                .into_any(),
                Some(ballots) => {
                    let (current, total) = flow::progress(&ballots);
                    let next = flow::current_contest(&ballots).map(str::to_string);
                    let navigate = navigate.clone();
                    let items = ballots
                        .iter()
                        .zip(flow::ballot_states(&ballots))
                        .map(|(b, ballot_state)| {
                            let class = match ballot_state {
                                BallotState::Done => "status voted",
                                BallotState::Current => "status current",
                                BallotState::Upcoming => "status",
                            };
                            // 「今」は、色に加えて、左の太い線（`current`）と、記号・太字でも示す。
                            let item_class = if ballot_state == BallotState::Current { "current" } else { "" };
                            view! {
                                <li class=item_class>
                                    <span class="contest-name">{b.name.clone()}</span>
                                    <span class="kind">{b.type_name.clone()}</span>
                                    <span class=class>{format!("{} {}", ballot_state.mark(), ballot_state.label())}</span>
                                </li>
                            }
                        })
                        .collect_view();
                    view! {
                        <p class="progress">
                            {if next.is_none() {
                                format!("すべての{}に投票済みです。", labels::ballot_item())
                            } else {
                                flow::format_progress(labels::progress_template(), current, total)
                            }}
                        </p>
                        <ul class="contest-list">{items}</ul>
                        {next.map(|id| {
                            view! {
                                <button on:click=move |_| {
                                    navigate(&Page::Ballot(id.clone()).path(), replace())
                                }>"続ける"</button>
                            }
                        })}
                    }
                    .into_any()
                }
            }}
        </section>
    }
}
