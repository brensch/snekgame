//! The active-games screen: every live session (lobby or mid-game), or an
//! invitation to start one. Creating a game names it and drops you straight
//! into its lobby — the URL carries only an opaque session id.

use crate::store::{new_session_id, Store};
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::SessionSummary;
use std::cell::Cell;

thread_local! {
    static LAST_FETCH: Cell<f64> = const { Cell::new(0.0) };
}

pub fn games_page() -> impl IntoView {
    let store = Store::use_store();
    let sessions = RwSignal::new(Option::<Vec<SessionSummary>>::None);
    let new_name = RwSignal::new(String::new());

    // Poll the list every couple of seconds while this page is up, driven by
    // the shared coarse clock (no timer to manage).
    LAST_FETCH.with(|c| c.set(0.0));
    Effect::new(move |_| {
        let now = store.now_ms.get();
        if now - LAST_FETCH.with(|c| c.get()) < 2000.0 {
            return;
        }
        LAST_FETCH.with(|c| c.set(now));
        spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/sessions").send().await {
                if let Ok(list) = resp.json::<Vec<SessionSummary>>().await {
                    sessions.set(Some(list));
                }
            }
        });
    });

    let create = move || {
        let name = new_name.get_untracked().trim().to_string();
        if name.is_empty() {
            return;
        }
        store.pending_name.set(Some(name));
        let id = new_session_id();
        let _ = web_sys::window()
            .unwrap()
            .location()
            .set_hash(&format!("#/s/{id}"));
    };

    view! {
        <div>
            <h2 style="margin-top: 24px;">"Games on right now"</h2>
            {move || {
                match sessions.get() {
                    None => view! { <div class="banner">"Looking for games…"</div> }.into_any(),
                    Some(list) if list.is_empty() => {
                        view! {
                            <div class="banner">
                                "Nothing going on. Start a game and share it!"
                            </div>
                        }
                            .into_any()
                    }
                    Some(list) => {
                        view! {
                            <table>
                                <tbody>
                                    {list
                                        .into_iter()
                                        .map(|s| {
                                            let href = format!("#/s/{}", s.id);
                                            view! {
                                                <tr>
                                                    <td>
                                                        <b>{s.name}</b>
                                                    </td>
                                                    <td class="right">
                                                        {s.players}
                                                        {if s.players == 1 { " player" } else { " players" }}
                                                    </td>
                                                    <td class="right">
                                                        {if s.playing { "playing 🐍" } else { "in lobby" }}
                                                    </td>
                                                    <td class="right">
                                                        <a href=href>
                                                            <button>"Join"</button>
                                                        </a>
                                                    </td>
                                                </tr>
                                            }
                                        })
                                        .collect_view()}
                                </tbody>
                            </table>
                        }
                            .into_any()
                    }
                }
            }}

            <h3 style="margin-top: 28px;">"Start a new one"</h3>
            <input
                type="text"
                placeholder="name your game"
                maxlength="32"
                prop:value=move || new_name.get()
                on:input=move |ev| new_name.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        create();
                    }
                }
            />
            <button
                class="big"
                style=move || format!("margin-top: 10px; background: {};", store.profile.get().colour)
                disabled=move || new_name.get().trim().is_empty()
                on:click=move |_| create()
            >
                "Create game"
            </button>

            <p style="margin-top: 24px;">
                <a href="#/history">"Game history →"</a>
            </p>
        </div>
    }
}
