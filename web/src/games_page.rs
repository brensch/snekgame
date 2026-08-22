//! The home screen: live games to join (paginated), or start your own with
//! one tap — games aren't named, they're known by their host, and the URL
//! carries only an opaque session id.
//! The list is pushed by the server over WebTransport the moment it changes
//! (the route effect in app.rs opens the watch connection).

use crate::palette;
use crate::store::{new_session_id, Store};
use leptos::prelude::*;
use snek_shared::protocol::SessionSummary;

const PAGE_SIZE: usize = 6;

pub fn home_page() -> impl IntoView {
    let store = Store::use_store();
    let sessions = store.sessions;
    let page = RwSignal::new(0usize);

    let create = move || {
        let id = new_session_id();
        let _ = web_sys::window()
            .unwrap()
            .location()
            .set_hash(&format!("#/s/{id}"));
    };

    // tac's RotatingEmoji: the actionish send list, hopping rather than
    // marching so it feels random. (Reduce in u64 — ms-since-epoch overflows
    // a 32-bit usize cast on wasm.)
    let rotating = move || {
        let t = (store.now_ms.get() / 400.0) as u64;
        let i = (t.wrapping_mul(7) % palette::SEND_EMOJIS.len() as u64) as usize;
        palette::SEND_EMOJIS[i]
    };

    view! {
        <div>
            <h1 style="padding-top: 28px;">"Nokia snake,"<br />"but violent*"</h1>

            {move || {
                match sessions.get() {
                    // Nothing (or nothing yet): no list section at all.
                    None => ().into_any(),
                    Some(list) if list.is_empty() => ().into_any(),
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
                            <table class="games">
                                <tbody>
                                    {slice
                                        .into_iter()
                                        .map(|s| {
                                            let href = format!("#/s/{}", s.id);
                                            view! {
                                                // Two-line row: big host emoji spans both lines,
                                                // counts and status stack, Join fills the height.
                                                <tr>
                                                    <td class="host-emoji">{s.host_emoji}</td>
                                                    <td>
                                                        <b>{s.host_name} "'s"<br />"game"</b>
                                                    </td>
                                                    <td class="right">
                                                        <div>"🧑 " {s.players}</div>
                                                        {(s.bots > 0)
                                                            .then(|| {
                                                                view! { <div>"🤖 " {s.bots}</div> }
                                                            })}
                                                    </td>
                                                    <td class="right">
                                                        "in"<br />
                                                        {if s.playing { "game" } else { "lobby" }}
                                                    </td>
                                                    <td class="right join-cell">
                                                        <a href=href>
                                                            <button
                                                                class="join"
                                                                style=format!(
                                                                    "background: {};",
                                                                    store.profile.get_untracked().colour,
                                                                )
                                                            >
                                                                "Join"
                                                            </button>
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

            <button
                class="big"
                style=move || {
                    format!(
                        "margin-top: 16px; font-size: 24px; background: {};",
                        store.profile.get().colour,
                    )
                }
                on:click=move |_| create()
            >
                "Start new game\u{00A0}" {rotating}
            </button>

            <p style="margin-top: 24px;">
                <a href="#/history">"Game history →"</a>
            </p>
            <p>
                <a href="#/compute">"Botnet →"</a>
            </p>

            <div style="margin-top: 40px;">
                <p>"* no toes involved"</p>
                <p>"A game by Brendo"</p>
            </div>
        </div>
    }
}
