//! ログイン画面。

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::app::{AppState, replace, use_guard};
use crate::flow::{self, Route as Page};

#[component]
pub fn LoginPage() -> impl IntoView {
    let state = expect_context::<AppState>();
    use_guard(Page::Login);
    let navigate = use_navigate();

    let voter_id = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let my_number = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    // 期間と今の状態（原則17・18）。取得できなくても、ログイン自体は試せるようにする。
    let election = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(status) = api::election_status().await {
                election.set(Some(crate::election_status::summary_line(&status)));
            }
        });
    });

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if busy.get_untracked() {
            return;
        }
        let id = voter_id.get_untracked();
        if let Err(problem) = flow::validate_voter_id(&id) {
            error.set(Some(problem.message().to_string()));
            return;
        }
        // パスワードとマイナンバーは、送信に使う分だけ取り出し、入力欄はすぐに空にする。
        let secret = password.get_untracked();
        password.set(String::new());
        let secret = (!secret.is_empty()).then_some(secret);
        let number = my_number.get_untracked();
        my_number.set(String::new());
        let number = (!number.is_empty()).then_some(number);

        busy.set(true);
        error.set(None);
        state.notice.set(None);
        let navigate = navigate.clone();
        leptos::task::spawn_local(async move {
            let result = match api::login(&id, secret.as_deref(), number.as_deref()).await {
                Ok(login) => api::ballot_status(&login.token)
                    .await
                    .map(|status| (login.token, status)),
                Err(failure) => Err(failure),
            };
            match result {
                Ok((token, status)) => {
                    // 投票する順番は固定: 先頭の未投票の投票用紙へ自動で進む（すべて済み・無ければ進捗の画面）。
                    let target = flow::entry_route(&status.ballots);
                    state.set_status(status);
                    state.token.set(Some(token));
                    navigate(&target.path(), replace());
                }
                Err(failure) => {
                    error.set(Some(flow::login_failure_message(failure).to_string()));
                }
            }
            busy.set(false);
        });
    };

    view! {
        <section>
            <h1>{crate::labels::login_heading()}</h1>
            <p class="election-status hint">{move || election.get()}</p>
            <form on:submit=on_submit>
                <label for="voter-id">"ログイン ID"</label>
                <input
                    id="voter-id"
                    type="text"
                    autocomplete="off"
                    autocapitalize="off"
                    spellcheck="false"
                    prop:value=move || voter_id.get()
                    on:input:target=move |ev| voter_id.set(ev.target().value())
                />
                <label for="password">"パスワード"</label>
                <input
                    id="password"
                    type="password"
                    autocomplete="off"
                    autocapitalize="off"
                    spellcheck="false"
                    prop:value=move || password.get()
                    on:input:target=move |ev| password.set(ev.target().value())
                />
                <label for="my-number">"マイナンバー（任意）"</label>
                <input
                    id="my-number"
                    type="password"
                    inputmode="numeric"
                    autocomplete="off"
                    prop:value=move || my_number.get()
                    on:input:target=move |ev| my_number.set(ev.target().value())
                />
                <p class="hint">
                    "郵送された ID とパスワードを入力してください。マイナンバーは使われず、保存もされません。"
                </p>
                <button type="submit" disabled=move || busy.get()>"ログイン"</button>
            </form>
            <p class="error" role="alert">{move || error.get().map(|m| crate::error::alert_text(&m))}</p>
        </section>
    }
}
