//! The home screen: live games to join (paginated), or start your own —
//! named in one flow, with the URL carrying only an opaque session id.

use crate::palette;
use crate::store::{new_session_id, Store};
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::SessionSummary;
use std::cell::Cell;

const PAGE_SIZE: usize = 6;

thread_local! {
    static LAST_FETCH: Cell<f64> = const { Cell::new(0.0) };
}

pub fn home_page() -> impl IntoView {
    let store = Store::use_store();
    let sessions = RwSignal::new(Option::<Vec<SessionSummary>>::None);
    let page = RwSignal::new(0usize);
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

    // tac's RotatingEmoji, driven off the shared coarse clock.
    let rotating = move || {
        let i = (store.now_ms.get() / 400.0) as usize % palette::EMOJIS.len();
        palette::EMOJIS[i]
    };

    view! {
        <div>
            <h1 style="padding-top: 28px;">"A game entirely unrelated to toes*"</h1>

            <h3>"Games on right now"</h3>
            {move || {
                match sessions.get() {
                    None => view! { <div class="banner">"Looking for games…"</div> }.into_any(),
                    Some(list) if list.is_empty() => {
                        view! {
                            <div class="banner">"Nothing going on. Start one below!"</div>
                        }
                            .into_any()
                    }
                    Some(list) => {
                        let pages = list.len().div_ceil(PAGE_SIZE).max(1);
                        let p = page.get().min(pages - 1);
                        let slice: Vec<SessionSummary> = list
                            .iter()
                            .skip(p * PAGE_SIZE)
                            .take(PAGE_SIZE)
                            .cloned()
                            .collect();
                        view! {
                            <table>
                                <tbody>
                                    {slice
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
                            {(pages > 1)
                                .then(|| {
                                    view! {
                                        <div class="row" style="margin-top: 8px;">
                                            <button
                                                disabled=p == 0
                                                on:click=move |_| page.update(|v| *v = v.saturating_sub(1))
                                            >
                                                "‹ prev"
                                            </button>
                                            <span style="text-align: center;">
                                                {format!("page {} / {}", p + 1, pages)}
                                            </span>
                                            <button
                                                disabled=p + 1 >= pages
                                                on:click=move |_| page.update(|v| *v += 1)
                                            >
                                                "next ›"
                                            </button>
                                        </div>
                                    }
                                })}
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
                "Create game\u{00A0}" {rotating}
            </button>

            <p style="margin-top: 24px;">
                <a href="#/history">"Game history →"</a>
            </p>

            <div style="margin-top: 40px;">
                <p>"* Toes may be involved"</p>
                <p>"A game by Brendo"</p>
            </div>
        </div>
    }
}
