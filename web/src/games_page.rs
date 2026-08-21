//! The home screen: live games to join (paginated), or start your own —
//! named in one flow, with the URL carrying only an opaque session id.
//! The list is pushed by the server over /ws/games the moment it changes.

use crate::palette;
use crate::store::{new_session_id, Store};
use leptos::prelude::*;
use snek_shared::protocol::SessionSummary;
use std::cell::{Cell, RefCell};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

const PAGE_SIZE: usize = 6;

thread_local! {
    /// Generation guard for the watch socket: bumping it orphans any
    /// callbacks still attached to an older socket.
    static WATCH_GEN: Cell<u64> = const { Cell::new(0) };
    static WATCH_WS: RefCell<Option<web_sys::WebSocket>> = const { RefCell::new(None) };
}

fn watch_url() -> String {
    let loc = web_sys::window().unwrap().location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
    format!("{proto}://{}/ws/games", loc.host().unwrap_or_default())
}

/// Open (or re-open) the live games feed. The server pushes the full list on
/// connect and again whenever it changes; on drop we retry every 2s while
/// the home page is still the active generation.
fn open_games_watch(sessions: RwSignal<Option<Vec<SessionSummary>>>) {
    let generation = WATCH_GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    connect_games_watch(generation, sessions);
}

fn close_games_watch() {
    WATCH_GEN.with(|g| g.set(g.get() + 1));
    WATCH_WS.with(|w| {
        if let Some(ws) = w.borrow_mut().take() {
            let _ = ws.close();
        }
    });
}

fn connect_games_watch(generation: u64, sessions: RwSignal<Option<Vec<SessionSummary>>>) {
    let current = move || WATCH_GEN.with(|g| g.get()) == generation;
    if !current() {
        return;
    }
    let Ok(ws) = web_sys::WebSocket::new(&watch_url()) else {
        schedule_watch_retry(generation, sessions);
        return;
    };
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
        move |ev: web_sys::MessageEvent| {
            if !current() {
                return;
            }
            let Ok(buf) = ev.data().dyn_into::<js_sys::ArrayBuffer>() else {
                return;
            };
            let bytes = js_sys::Uint8Array::new(&buf).to_vec();
            if let Ok(list) = postcard::from_bytes::<Vec<SessionSummary>>(&bytes) {
                sessions.set(Some(list));
            }
        },
    );
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        schedule_watch_retry(generation, sessions);
    });
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    WATCH_WS.with(|w| *w.borrow_mut() = Some(ws));
}

fn schedule_watch_retry(generation: u64, sessions: RwSignal<Option<Vec<SessionSummary>>>) {
    gloo_timers::callback::Timeout::new(2000, move || {
        connect_games_watch(generation, sessions);
    })
    .forget();
}

pub fn home_page() -> impl IntoView {
    let store = Store::use_store();
    let sessions = RwSignal::new(Option::<Vec<SessionSummary>>::None);
    let page = RwSignal::new(0usize);
    let new_name = RwSignal::new(String::new());

    open_games_watch(sessions);
    on_cleanup(close_games_watch);

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

    // tac's RotatingEmoji, driven off the shared coarse clock. (Careful:
    // ms-since-epoch overflows a 32-bit usize cast on wasm, so reduce in u64.)
    let rotating = move || {
        let i = ((store.now_ms.get() / 400.0) as u64 % palette::EMOJIS.len() as u64) as usize;
        palette::EMOJIS[i]
    };

    view! {
        <div>
            <h1 style="padding-top: 28px;">"A game entirely unrelated to toes*"</h1>

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
                            <h3>"Games on right now"</h3>
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
                                                        {(s.bots > 0)
                                                            .then(|| format!(" + {} 🤖", s.bots))}
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

            {move || {
                let any = sessions.get().map(|l| !l.is_empty()).unwrap_or(false);
                any.then(|| view! { <h3 style="margin-top: 28px;">"or start your own lobby"</h3> })
            }}
            <input
                style="margin-top: 16px;"
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
                "Start game\u{00A0}" {rotating}
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
