//! 投票画面（候補者の選択 → 確認 → 送信）。
//!
//! 選んだ候補者は、この画面のシグナル（`VotePhase`）にだけ持つ。URL・履歴・ストレージ・ログ・
//! 共有状態には置かない。投票を受理されたら段階を初期化して捨て、完了画面へ履歴を置き換えて移る。
//!
//! この画面に入れるのは「今」の投票用紙（表示順で先頭の未投票）だけ。順番は選べない（`flow::guard`）。
//!
//! 白票: API が `allow_blank`（open の時点で固定した選挙のルール）を返したときだけ、候補者一覧の最後に白票の選択肢
//! （設定 `labels.blank_option`）を置く（`flow::choices`）。確認画面では「白票として投票します」
//! （`labels.blank_confirm`）と、候補者とは違う文言で示す（`flow::confirm_message`）。
//!
//! やり直し（再投票。ADR 0022）: `/revote/{contest_id}`（[`RevoteVotePage`]）も同じ画面を使う。入れるのは、投票済みで上限に
//! 達していない投票用紙だけ（`flow::guard`）。確認画面では「前回の投票内容を変更します」（`labels.revote_confirm`）を出し、
//! 前回の投票内容は出さない（API も返さない）。送信には、画面が見た受理済みの票の数を添える（二重送信・同時送信で、
//! 2 回やり直したことにならない）。

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params};
use leptos_router::params::Params;
use shared_types::CandidatesResponse;

use crate::api;
use crate::app::{AppState, replace, use_guard};
use crate::error::{self, ApiFailure};
use crate::flow::{self, Route as Page, VoteOutcome, VotePhase};
use crate::labels;

#[derive(Params, PartialEq, Clone, Debug)]
struct BallotParams {
    election_id: Option<String>,
    district_id: Option<String>,
}

/// 確認済みの候補者を送信し、結果に応じて遷移する。`revote` なら、やり直し（画面が見た票の数を添える）。
fn start_submit<N>(
    state: AppState,
    phase: RwSignal<VotePhase>,
    contest_id: String,
    revote: bool,
    navigate: N,
) where
    N: Fn(&str, NavigateOptions) + 'static,
{
    // 確認中以外では送信しない（二重送信の防止）。
    let Some((submitting, candidate_id)) = flow::submit(&phase.get_untracked()) else {
        return;
    };
    let Some(token) = state.token.get_untracked() else {
        return;
    };
    let expected = if revote {
        let seen = state.ballots.with_untracked(|ballots| {
            ballots
                .as_deref()
                .and_then(|list| flow::revote_expected(list, &contest_id))
        });
        // 一覧が無い（ここには来ないはず）: 送らずに、やり直しの一覧へ戻す。
        let Some(seen) = seen else {
            return;
        };
        Some(seen)
    } else {
        None
    };
    phase.set(submitting);
    leptos::task::spawn_local(async move {
        let outcome =
            flow::vote_outcome(api::vote(&token, &contest_id, &candidate_id, expected).await);
        // 段階の所有権を取り出して結果を反映する。画面を離れる場合、ここで候補者 ID は捨てられる
        // （`next_phase` は初期状態）。
        let (next_phase, route) = flow::apply_outcome(
            phase.try_update(std::mem::take).unwrap_or_default(),
            outcome,
        );
        phase.set(next_phase);

        match outcome {
            VoteOutcome::Accepted => {
                state.ballots.update(|ballots| {
                    if let Some(list) = ballots {
                        *list = if revote {
                            flow::mark_revoted(list, &contest_id)
                        } else {
                            flow::mark_voted(list, &contest_id)
                        };
                    }
                });
                state.last_voted.set(Some(contest_id.clone()));
            }
            VoteOutcome::SessionExpired => state.logout(),
            _ => {}
        }
        state
            .notice
            .set(flow::outcome_notice(outcome, labels::ballot_item()));
        let Some(mut route) = route else {
            return;
        };
        // 一覧が古かった（投票済み・存在しない・対象外）: 取り直して、先頭の未投票へ進む。
        // やり直しの失敗: 取り直して、やり直しの一覧へ戻る（一覧の画面が、開くときにも取り直す）。
        if route == Page::Progress
            && let Ok(status) = api::ballot_status(&token).await
        {
            route = flow::entry_route(&status.ballots);
            state.set_status(status);
        }
        navigate(&route.path(), replace());
    });
}

/// 投票画面（今の投票用紙）。
#[component]
pub fn VotePage() -> impl IntoView {
    vote_page(false)
}

/// やり直しの投票画面（投票済みの投票用紙）。
#[component]
pub fn RevoteVotePage() -> impl IntoView {
    vote_page(true)
}

fn vote_page(revote: bool) -> impl IntoView {
    let state = expect_context::<AppState>();
    let params = use_params::<BallotParams>();
    // 不正な ID は空として扱い、ルート保護が「今」の投票用紙へ戻す。
    let contest_id = params
        .read_untracked()
        .as_ref()
        .ok()
        .and_then(|p| {
            Some(format!(
                "{}/{}",
                p.election_id.as_ref()?,
                p.district_id.as_ref()?
            ))
        })
        .unwrap_or_default();
    use_guard(if revote {
        Page::RevoteBallot(contest_id.clone())
    } else {
        Page::Ballot(contest_id.clone())
    });
    let navigate = use_navigate();

    let candidates = RwSignal::new(None::<CandidatesResponse>);
    let load_error = RwSignal::new(None::<String>);
    let phase = RwSignal::new(VotePhase::default());

    Effect::new({
        let (navigate, contest_id) = (navigate.clone(), contest_id.clone());
        move |_| {
            let Some(token) = state.token.get_untracked() else {
                return;
            };
            let (navigate, contest_id) = (navigate.clone(), contest_id.clone());
            leptos::task::spawn_local(async move {
                match api::candidates(&token, &contest_id).await {
                    Ok(response) => candidates.set(Some(response)),
                    Err(ApiFailure::Unauthorized) => {
                        state.notice.set(flow::outcome_notice(
                            VoteOutcome::SessionExpired,
                            labels::ballot_item(),
                        ));
                        state.logout();
                    }
                    Err(ApiFailure::NotFound | ApiFailure::NotEligible) => {
                        navigate(&Page::Progress.path(), replace())
                    }
                    Err(_) => load_error.set(Some(
                        "候補者を取得できませんでした。ページを開き直してください。".to_string(),
                    )),
                }
            });
        }
    });

    let ballot_name = {
        let contest_id = contest_id.clone();
        move || {
            state.ballots.with(|ballots| {
                ballots
                    .as_ref()
                    .and_then(|list| list.iter().find(|b| b.contest_id == contest_id))
                    .map(|b| format!("{}（{}）", b.name, b.type_name))
            })
        }
    };
    // 進捗の表示（「{total}枚中{current}枚目」の型は設定 labels.progress）。
    let progress_text = move || {
        state.ballots.with(|ballots| {
            ballots.as_ref().map(|list| {
                let (current, total) = flow::progress(list);
                flow::format_progress(labels::progress_template(), current, total)
            })
        })
    };
    // 確認画面の注意書き: やり直しを認める選挙なら、この投票の後に、あと何回やり直せるか。
    let hint = {
        let contest_id = contest_id.clone();
        move || {
            let status = state.revote.get();
            let left_after = status.as_ref().map_or(0, |r| {
                state.ballots.with(|ballots| {
                    let seen = ballots
                        .as_ref()
                        .and_then(|list| list.iter().find(|b| b.contest_id == contest_id))
                        .map_or(0, |b| b.ballots_cast);
                    // この投票で、受理済みの票が 1 つ増える（1 票目を含めて max_revotes + 1 票まで）。
                    r.max_revotes
                        .saturating_add(1)
                        .saturating_sub(seen.saturating_add(1))
                })
            });
            flow::confirm_hint(status.as_ref(), left_after)
        }
    };
    let confirm_text = move |candidate_id: &str| {
        candidates.with(|response| {
            let list = response
                .as_ref()
                .map_or(&[][..], |r| r.candidates.as_slice());
            flow::confirm_message(candidate_id, list, labels::blank_confirm())
        })
    };

    view! {
        <section>
            <p class="progress">
                {progress_text}
                " "
                <a href="/progress">"進捗を見る"</a>
            </p>
            <h1>{move || ballot_name().unwrap_or_else(|| "投票".to_string())}</h1>
            <p class="error" role="alert">{move || load_error.get().map(|m| error::alert_text(&m))}</p>
            {move || {
                let navigate = navigate.clone();
                let contest_id = contest_id.clone();
                let Some(response) = candidates.get() else {
                    return view! { <p>"読み込み中…"</p> }.into_any();
                };
                match phase.get() {
                    VotePhase::Choosing { selected } => {
                        let has_selection = selected.is_some();
                        // 候補者の最後に、白票（allow_blank のときだけ）。
                        let options = flow::choices(
                            &response.candidates,
                            response.allow_blank,
                            labels::blank_option(),
                        )
                            .into_iter()
                            .map(|c| {
                                let id = c.candidate_id.clone();
                                let checked = selected.as_deref() == Some(c.candidate_id.as_str());
                                let party = c.party;
                                // 白票は候補者と区切って表示する（class に blank を足す。区切りの余白と破線の枠は style.css）。
                                let class = match (c.blank, checked) {
                                    (false, false) => "candidate",
                                    (false, true) => "candidate selected",
                                    (true, false) => "candidate blank",
                                    (true, true) => "candidate blank selected",
                                };
                                view! {
                                    <label class=class>
                                        <input
                                            type="radio"
                                            name="candidate"
                                            prop:checked=checked
                                            on:change=move |_| {
                                                let id = id.clone();
                                                phase.update(|p| *p = flow::pick(std::mem::take(p), id))
                                            }
                                        />
                                        <span>{c.label}</span>
                                        {party.map(|p| view! { <span class="party">{p}</span> })}
                                        // 選択中は、色（枠）だけでなく、太い枠・太字と、この「✓ 選択中」の文字でも示す。
                                        <span class="check" aria-hidden="true">{checked.then_some("✓ 選択中")}</span>
                                    </label>
                                }
                            })
                            .collect_view();
                        view! {
                            <fieldset>
                                <legend>"候補者を選んでください"</legend>
                                {options}
                            </fieldset>
                            <button
                                disabled=!has_selection
                                on:click=move |_| phase.update(|p| *p = flow::confirm(std::mem::take(p)))
                            >
                                "確認へ進む"
                            </button>
                        }
                        .into_any()
                    }
                    VotePhase::Confirming { candidate_id } => view! {
                        // やり直しでは、前回の投票内容は出さず、変更することだけを示す。
                        {revote.then(|| view! { <p class="confirm revote">{labels::revote_confirm()}</p> })}
                        <p class="confirm">
                            {confirm_text(&candidate_id)}
                        </p>
                        <p class="hint">{hint.clone()}</p>
                        <div class="actions">
                            <button class="secondary" on:click=move |_| phase.update(|p| *p = flow::cancel(std::mem::take(p)))>
                                "戻る"
                            </button>
                            <button on:click=move |_| start_submit(state, phase, contest_id.clone(), revote, navigate.clone())>
                                "投票する"
                            </button>
                        </div>
                    }
                    .into_any(),
                    VotePhase::Submitting { .. } => {
                        view! { <p role="status">"送信中です…"</p> }.into_any()
                    }
                    VotePhase::Failed { kind, .. } => view! {
                        <p class="error" role="alert">{error::alert_text(kind.message())}</p>
                        <div class="actions">
                            <button class="secondary" on:click=move |_| phase.update(|p| *p = flow::cancel(std::mem::take(p)))>
                                "選び直す"
                            </button>
                            {kind.can_retry().then(|| view! {
                                <button on:click=move |_| phase.update(|p| *p = flow::retry(std::mem::take(p)))>
                                    "もう一度試す"
                                </button>
                            })}
                        </div>
                    }
                    .into_any(),
                }
            }}
        </section>
    }
}
