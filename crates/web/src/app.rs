//! アプリ全体: セッション状態、ルータ、ルート保護のフック。

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::hooks::use_navigate;
use leptos_router::path;
use shared_types::BallotStatusDto;

use crate::flow::{self, GuardContext, Route as Page};
use crate::pages::{
    ChainAnchorsPage, ChainBlockPage, ChainIndexPage, ChainShardPage, DonePage, LoginPage,
    ProgressPage, VotePage,
};
use crate::theme::{self, Mode};

/// 画面をまたいで共有する状態。すべてメモリ上のシグナルで、ブラウザのストレージには保存しない
/// （リロードや共用端末でセッションが残らないようにするため）。
///
/// 候補者の情報はここに持たない。持つのは投票用紙の ID までで、候補者は投票画面の中だけで扱う。
#[derive(Clone, Copy)]
pub struct AppState {
    /// セッショントークン。`None` なら未ログイン。
    pub token: RwSignal<Option<String>>,
    /// 取得済みの投票用紙の一覧（表示順・固定。投票済みフラグ付き）。有権者に関係するものだけ。
    pub ballots: RwSignal<Option<Vec<BallotStatusDto>>>,
    /// たった今投票を受理された投票用紙の ID（完了画面の表示条件）。
    pub last_voted: RwSignal<Option<String>>,
    /// 次の画面で一度だけ見せる案内。
    pub notice: RwSignal<Option<String>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            token: RwSignal::new(None),
            ballots: RwSignal::new(None),
            last_voted: RwSignal::new(None),
            notice: RwSignal::new(None),
        }
    }

    /// セッションに関わる状態をすべて破棄する。
    pub fn logout(&self) {
        self.token.set(None);
        self.ballots.set(None);
        self.last_voted.set(None);
    }
}

/// 履歴に残さない遷移（戻るボタンで投票画面などに戻れないようにする）。
pub fn replace() -> NavigateOptions {
    NavigateOptions {
        replace: true,
        ..Default::default()
    }
}

/// ルート保護。マウント時に [`flow::guard`] で判定し、必要なら遷移する。
///
/// 追跡するのはログイン状態だけ。投票の受理で一覧が更新されるたびに再判定すると、
/// 完了画面への遷移と競合して意図しないリダイレクトが起きるため、他は追跡しない。
pub fn use_guard(page: Page) {
    let state = expect_context::<AppState>();
    let navigate = use_navigate();
    Effect::new(move |_| {
        let logged_in = state.token.with(Option::is_some);
        let ballots = state.ballots.get_untracked();
        let last_voted = state.last_voted.get_untracked();
        let ctx = GuardContext {
            logged_in,
            ballots: ballots.as_deref(),
            last_voted: last_voted.as_deref(),
        };
        if let Some(target) = flow::guard(&ctx, &page) {
            navigate(&target.path(), replace());
        }
    });
}

/// テーマの切り替え（ライト / ダーク / OSに合わせる）。選択中は、色だけでなく、✓・太い枠・太字（`selected`）と
/// `aria-pressed` でも示す。選んだ内容は、`localStorage` に保存する（`theme::apply`）。
#[component]
fn ThemeSwitch() -> impl IntoView {
    let mode = RwSignal::new(theme::load_mode());
    let buttons = Mode::ALL
        .into_iter()
        .map(|m| {
            let selected = move || mode.get() == m;
            view! {
                <button
                    type="button"
                    class="secondary theme-option"
                    class:selected=selected
                    aria-pressed=move || selected().to_string()
                    on:click=move |_| {
                        mode.set(m);
                        theme::apply(m);
                    }
                >
                    <span class="check" aria-hidden="true">{move || selected().then_some("✓ ")}</span>
                    {m.label()}
                </button>
            }
        })
        .collect_view();
    view! {
        <div class="theme-switch" role="group" aria-label="表示テーマ">{buttons}</div>
    }
}

#[component]
fn NoticeBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        {move || {
            state.notice.get().map(|message| {
                view! {
                    <p class="notice" role="status">
                        {message}
                        <button class="link" on:click=move |_| state.notice.set(None)>
                            "閉じる"
                        </button>
                    </p>
                }
            })
        }}
    }
}

#[component]
fn NotFound() -> impl IntoView {
    view! {
        <section>
            <h1>"ページが見つかりません"</h1>
            <p><a href="/">"ログイン画面へ"</a></p>
        </section>
    }
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    view! {
        <Router>
            <header class="site-header">
                <span class="site-title">{crate::labels::site_title()}</span>
                <A href=crate::chain::CHAIN_HOME>"ブロックチェーン"</A>
                <ThemeSwitch />
                <Show when=move || state.token.with(Option::is_some)>
                    <button class="link" on:click=move |_| state.logout()>"ログアウト"</button>
                </Show>
            </header>
            <main>
                <NoticeBar />
                <Routes fallback=NotFound>
                    <Route path=path!("/") view=LoginPage />
                    <Route path=path!("/progress") view=ProgressPage />
                    <Route path=path!("/ballots/:election_id/:district_id") view=VotePage />
                    <Route path=path!("/done") view=DonePage />
                    // ブロックチェーンのビューア（ログイン不要。ルート保護の対象外）。
                    <Route path=path!("/chain") view=ChainIndexPage />
                    <Route path=path!("/chain/anchors") view=ChainAnchorsPage />
                    <Route path=path!("/chain/:shard") view=ChainShardPage />
                    <Route path=path!("/chain/:shard/blocks/:height") view=ChainBlockPage />
                </Routes>
            </main>
        </Router>
    }
}
