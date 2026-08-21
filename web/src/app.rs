//! App shell: hash routing, home page, profile editor, global inputs
//! (keyboard + swipe), and the websocket lifecycle tied to the route.

use crate::history_page::{history_page, replay_page};
use crate::net;
use crate::palette;
use crate::session_page::session_page;
use crate::store::{save_profile, Route, Store};
use gloo_timers::callback::Interval;
use leptos::ev;
use leptos::prelude::*;
use snek_shared::protocol::ClientMsg;
use snek_shared::Dir;
use std::cell::Cell;

thread_local! {
    static TOUCH_START: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

pub fn app() -> impl IntoView {
    let store = Store::new();
    provide_context(store);

    // --- routing ----------------------------------------------------------
    let read_route = move || {
        let hash = web_sys::window()
            .and_then(|w| w.location().hash().ok())
            .unwrap_or_default();
        store.route.set(parse_hash(&hash));
    };
    read_route();
    window_event_listener(ev::hashchange, move |_| read_route());

    // --- shared coarse clock ---------------------------------------------
    let clock = Interval::new(200, move || store.now_ms.set(net::now()));
    clock.forget();

    // --- websocket follows the route --------------------------------------
    Effect::new(move |_| {
        let route = store.route.get();
        let profile = store.profile.get();
        match route {
            Route::Session(name) if !profile.name.trim().is_empty() => {
                net::ensure_connected(store, &name);
            }
            Route::Session(_) => {
                // Must pick a name before joining.
                store.show_profile_editor.set(true);
            }
            _ => net::disconnect(store),
        }
    });

    // --- global direction input -------------------------------------------
    window_event_listener(ev::keydown, move |e: web_sys::KeyboardEvent| {
        if store.game.get_untracked().is_none() {
            return;
        }
        let dir = match e.key().as_str() {
            "ArrowUp" | "w" | "W" => Some(Dir::Up),
            "ArrowDown" | "s" | "S" => Some(Dir::Down),
            "ArrowLeft" | "a" | "A" => Some(Dir::Left),
            "ArrowRight" | "d" | "D" => Some(Dir::Right),
            _ => None,
        };
        if let Some(dir) = dir {
            e.prevent_default();
            net::send_move(store, dir);
        }
    });

    window_event_listener(ev::touchstart, move |e: web_sys::TouchEvent| {
        if let Some(t) = e.touches().item(0) {
            TOUCH_START.with(|c| c.set(Some((t.client_x(), t.client_y()))));
        }
    });
    window_event_listener(ev::touchend, move |e: web_sys::TouchEvent| {
        if store.game.get_untracked().is_none() {
            return;
        }
        let Some((sx, sy)) = TOUCH_START.with(|c| c.take()) else {
            return;
        };
        let Some(t) = e.changed_touches().item(0) else {
            return;
        };
        let (dx, dy) = (t.client_x() - sx, t.client_y() - sy);
        if dx.abs() < 30 && dy.abs() < 30 {
            return; // a tap — handled by cell clicks
        }
        let dir = if dx.abs() >= dy.abs() {
            if dx > 0 { Dir::Right } else { Dir::Left }
        } else if dy > 0 {
            Dir::Down
        } else {
            Dir::Up
        };
        net::send_move(store, dir);
    });

    // First visit: force the profile editor open.
    if store.profile.get_untracked().name.trim().is_empty() {
        store.show_profile_editor.set(true);
    }

    let share_copied = RwSignal::new(false);
    view! {
        <div class="container">
            <div class="topbar">
                <a class="brand" href="#/">"snek 🐍"</a>
                <span style="display: flex; gap: 8px;">
                    {move || {
                        if let Route::Session(name) = store.route.get() {
                            Some(
                                view! {
                                    <span
                                        class="profile-chip"
                                        on:click=move |_| share_session(&name, share_copied)
                                    >
                                        {move || if share_copied.get() { "copied ✓" } else { "share" }}
                                    </span>
                                },
                            )
                        } else {
                            None
                        }
                    }}
                    <span
                        class="profile-chip"
                        style=move || format!("background: {};", store.profile.get().colour)
                        on:click=move |_| store.show_profile_editor.set(true)
                    >
                        {move || {
                            let p = store.profile.get();
                            format!("{} {}", p.name, p.emoji)
                        }}
                    </span>
                </span>
            </div>
            {move || match store.route.get() {
                Route::Home => home_page().into_any(),
                Route::Session(name) => session_page(name).into_any(),
                Route::History => history_page().into_any(),
                Route::Replay(id) => replay_page(id).into_any(),
            }}
            {move || store.show_profile_editor.get().then(profile_editor)}
        </div>
    }
}

/// Native share sheet where available (mobile); clipboard fallback elsewhere.
fn share_session(word: &str, copied: RwSignal<bool>) {
    let Some(window) = web_sys::window() else { return };
    let loc = window.location();
    let url = format!(
        "{}//{}/#/s/{}",
        loc.protocol().unwrap_or_default(),
        loc.host().unwrap_or_default(),
        word
    );
    let nav = window.navigator();
    let has_share = js_sys::Reflect::has(nav.as_ref(), &"share".into()).unwrap_or(false);
    if has_share {
        let data = web_sys::ShareData::new();
        data.set_title("snek 🐍");
        data.set_text("come play snek with me");
        data.set_url(&url);
        let _ = nav.share_with_data(&data);
    } else {
        let _ = nav.clipboard().write_text(&url);
        copied.set(true);
        gloo_timers::callback::Timeout::new(1500, move || copied.set(false)).forget();
    }
}

fn parse_hash(hash: &str) -> Route {
    let path = hash.trim_start_matches('#').trim_matches('/');
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => Route::Home,
        ["s", name] => Route::Session(normalize_word(name)),
        ["history"] => Route::History,
        ["history", id] => id
            .parse()
            .map(Route::Replay)
            .unwrap_or(Route::History),
        _ => Route::Home,
    }
}

fn normalize_word(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(24)
        .collect()
}

// ---------------------------------------------------------------------------
// Home page — a faithful cover of tac's HomePage
// ---------------------------------------------------------------------------

fn home_page() -> impl IntoView {
    let store = Store::use_store();
    let word = RwSignal::new(String::new());
    let error = RwSignal::new(String::new());

    let play = move || {
        let w = word.get_untracked();
        if w.is_empty() {
            error.set("Friend, you need a session name.".to_string());
            return;
        }
        let _ = web_sys::window().unwrap().location().set_hash(&format!("#/s/{w}"));
    };

    view! {
        <div>
            <h1 style="padding-top: 40px;">"A game entirely unrelated to toes*"</h1>
            <p>
                "Pick a word for your session. If you want to play with someone, use the same word."
            </p>
            <input
                type="text"
                placeholder="sessionword"
                prop:value=move || word.get()
                on:input=move |ev| {
                    word.set(normalize_word(&event_target_value(&ev)));
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        play();
                    }
                }
            />
            <button
                class="big"
                style=move || format!("margin-top: 12px; background: {};", store.profile.get().colour)
                on:click=move |_| play()
            >
                "Play 🐍"
            </button>
            <p style="color: #c00;">{move || error.get()}</p>

            <div class="banner rainbow">
                <span style="font-size: 26px; margin-right: 10px;">"🕺"</span>
                "Ugly colour? Uninspired emoji? Edit it in the top right."
            </div>

            <p>
                <a href="#/history">"Game history →"</a>
            </p>

            <div style="margin-top: 60px;">
                <p>"* Toes may be involved"</p>
                <p>"A game by Brendo"</p>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Profile editor
// ---------------------------------------------------------------------------

fn profile_editor() -> impl IntoView {
    let store = Store::use_store();
    let draft = RwSignal::new(store.profile.get_untracked());

    let save = move || {
        let p = draft.get_untracked();
        if p.name.trim().is_empty() {
            return;
        }
        save_profile(&p);
        store.profile.set(p.clone());
        net::send(&ClientMsg::UpdateProfile(p));
        store.show_profile_editor.set(false);
    };

    view! {
        <div class="modal-back">
            <div class="modal">
                <h3>"Who are you?"</h3>
                <input
                    type="text"
                    placeholder="your name"
                    maxlength="20"
                    prop:value=move || draft.get().name
                    on:input=move |ev| {
                        draft.update(|p| p.name = event_target_value(&ev));
                    }
                />
                <h3>"Emoji"</h3>
                <div class="emoji-grid">
                    {palette::EMOJIS
                        .iter()
                        .map(|e| {
                            let e = e.to_string();
                            let pick = e.clone();
                            view! {
                                <div
                                    class="pick"
                                    class:selected=move || draft.get().emoji == e
                                    on:click=move |_| {
                                        draft.update(|p| p.emoji = pick.clone());
                                    }
                                >
                                    {e.clone()}
                                </div>
                            }
                        })
                        .collect_view()}
                </div>
                <h3>"Colour"</h3>
                <div class="swatches">
                    {palette::swatches()
                        .into_iter()
                        .map(|c| {
                            let sel = c.clone();
                            let pick = c.clone();
                            view! {
                                <div
                                    class="pick"
                                    class:selected=move || draft.get().colour == sel
                                    style=format!("background: {c};")
                                    on:click=move |_| {
                                        draft.update(|p| p.colour = pick.clone());
                                    }
                                ></div>
                            }
                        })
                        .collect_view()}
                </div>
                <button
                    class="big"
                    style=move || format!("margin-top: 14px; background: {};", draft.get().colour)
                    disabled=move || draft.get().name.trim().is_empty()
                    on:click=move |_| save()
                >
                    "That's me"
                </button>
            </div>
        </div>
    }
}
