//! App shell: hash routing, home page, profile editor, global inputs
//! (keyboard + swipe), and the websocket lifecycle tied to the route.

use crate::compute_page::compute_page;
use crate::games_page::home_page;
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
use wasm_bindgen::JsValue;

thread_local! {
    static TOUCH_START: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// True while the profile modal has pushed a history entry, so the
    /// mobile back button/gesture closes the modal instead of navigating.
    static MODAL_HISTORY: Cell<bool> = const { Cell::new(false) };
}

/// Close the profile editor. If we pushed a history entry when opening, go
/// back to consume it — popstate then actually closes the modal, so the
/// history stack never ends up with a stale entry.
fn close_profile_editor(store: Store) {
    if MODAL_HISTORY.with(|c| c.get()) {
        if let Some(w) = web_sys::window() {
            if let Ok(h) = w.history() {
                let _ = h.back();
                return;
            }
        }
    }
    store.show_profile_editor.set(false);
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

    net::watch_main_thread();

    // The play screen sizes itself against whatever is above it. Measuring
    // beats guessing: a long name or a bigger font can wrap the header onto
    // two lines, and a constant here would quietly push the d-pad off the
    // bottom of the screen on exactly the phones that can least afford it.
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;
        let measure = move || {
            let Some(w) = web_sys::window() else { return };
            let Some(doc) = w.document() else { return };
            // Document-relative, not viewport-relative: a bounding rect
            // moves with the scroll position, and measuring mid-scroll gave
            // a header nine pixels tall and a play area taller than the
            // screen.
            let bottom = doc
                .query_selector(".topbar")
                .ok()
                .flatten()
                .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
                .map(|e| (e.offset_top() + e.offset_height()) as f64)
                .filter(|h| (24.0..320.0).contains(h))
                .unwrap_or(104.0);
            if let Some(root) = doc.document_element() {
                let style = root.unchecked_ref::<web_sys::HtmlElement>().style();
                let _ = style.set_property("--topbar-h", &format!("{bottom}px"));
            }
        };
        measure();
        let cb = Closure::<dyn FnMut()>::new(move || measure());
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
            let _ =
                w.add_event_listener_with_callback("orientationchange", cb.as_ref().unchecked_ref());
        }
        cb.forget();
        // The header isn't in the DOM on the very first pass; catch it once
        // it is, and again after fonts settle.
        for delay in [0, 250, 1_000] {
            let cb = Closure::<dyn FnMut()>::new(move || measure());
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    delay,
                );
            }
            cb.forget();
        }
    }

    // --- shared coarse clock + liveness watchdog ---------------------------
    let clock = Interval::new(200, move || {
        store.now_ms.set(net::now());
        let visible = web_sys::window()
            .and_then(|w| w.document())
            .map(|d| !d.hidden())
            .unwrap_or(true);
        if visible {
            net::check_liveness(store);
        }
    });
    clock.forget();

    // --- coming back to the app reconnects immediately --------------------
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;
        let cb = Closure::<dyn FnMut()>::new(move || {
            let visible = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| !d.hidden())
                .unwrap_or(true);
            net::report_availability(visible);
            if visible {
                net::nudge(store);
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("focus", cb.as_ref().unchecked_ref());
            let _ = w.add_event_listener_with_callback("online", cb.as_ref().unchecked_ref());
            let _ = w.add_event_listener_with_callback("pageshow", cb.as_ref().unchecked_ref());
            if let Some(d) = w.document() {
                let _ = d.add_event_listener_with_callback(
                    "visibilitychange",
                    cb.as_ref().unchecked_ref(),
                );
            }
        }
        cb.forget();
    }

    // --- the connection follows the route ---------------------------------
    Effect::new(move |_| {
        let route = store.route.get();
        let profile = store.profile.get();
        match route {
            Route::Session(name) if !profile.name.trim().is_empty() => {
                net::ensure_session(store, &name);
            }
            // Session route with no name: the full-page setup is showing;
            // we simply don't connect until a profile exists.
            Route::Session(_) => {}
            // Home watches the live games list.
            Route::Home => net::ensure_watch(store),
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

    // Whenever the modal opens, push one history entry so mobile back
    // closes it; popstate is the single place that actually closes.
    Effect::new(move |_| {
        if store.show_profile_editor.get() {
            MODAL_HISTORY.with(|c| {
                if !c.get() {
                    if let Some(w) = web_sys::window() {
                        if let Ok(h) = w.history() {
                            if h.push_state(&JsValue::NULL, "").is_ok() {
                                c.set(true);
                            }
                        }
                    }
                }
            });
        }
    });
    window_event_listener(ev::popstate, move |_| {
        if MODAL_HISTORY.with(|c| c.get()) {
            MODAL_HISTORY.with(|c| c.set(false));
            store.show_profile_editor.set(false);
        } else {
            // Back/forward across pushState entries (e.g. the clean "/"
            // home URL) — sync the route to wherever we landed.
            read_route();
        }
    });

    let share_copied = RwSignal::new(false);
    view! {
        <div class="container">
            {move || {
                // No player yet: the setup form IS the page. Nothing else
                // renders (and the server refuses nameless joins anyway).
                if store.profile.get().name.trim().is_empty() {
                    return view! {
                        <div>
                            <h1 style="padding-top: 28px;">"snek 🐍"</h1>
                            <p>"introduce yourself"</p>
                            {profile_form()}
                        </div>
                    }
                        .into_any();
                }
                view! {
                    <div>
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
                                                    {move || {
                                                        if share_copied.get() { "copied ✓" } else { "share" }
                                                    }}
                                                </span>
                                            },
                                        )
                                    } else {
                                        None
                                    }
                                }}
                                <span
                                    class="profile-chip"
                                    style=move || {
                                        format!("background: {};", store.profile.get().colour)
                                    }
                                    on:click=move |_| store.show_profile_editor.set(true)
                                >
                                    {move || {
                                        let p = store.profile.get();
                                        format!("{} {}", p.name, p.emoji)
                                    }}
                                </span>
                            </span>
                        </div>
                        {move || store.error.get().map(|e| view! {
                            <div class="banner error">{e}</div>
                        })}
                        // Shown only when we have actually established it:
                        // the API answered over TCP while WebTransport would
                        // not connect at all.
                        {move || store.quic_blocked.get().then(|| view! {
                            <div class="banner error">
                                <strong>"📵 this network is blocking snek"</strong>
                                <br />
                                "It lets normal web traffic through but drops the UDP "
                                "that snek needs to talk to the server. Work and school "
                                "Wi-Fi usually do this on purpose."
                                <br />
                                <br />
                                "Turn Wi-Fi off and use mobile data."
                            </div>
                        })}
                        {move || match store.route.get() {
                            Route::Home => home_page().into_any(),
                            Route::Session(name) => session_page(name).into_any(),
                            Route::History => history_page().into_any(),
                            Route::Replay(id) => replay_page(id).into_any(),
                            Route::Compute => compute_page().into_any(),
                        }}
                        {move || store.show_profile_editor.get().then(profile_editor)}
                    </div>
                }
                    .into_any()
            }}
        </div>
    }
}

/// Built-in browser share sheet where it exists (mobile / secure contexts);
/// clipboard where that exists; a plain prompt as the everywhere-fallback.
/// Feature-checked with Reflect so nothing here can throw on plain http.
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
    let has = |name: &str| js_sys::Reflect::has(nav.as_ref(), &name.into()).unwrap_or(false);
    if has("share") {
        let data = web_sys::ShareData::new();
        data.set_title("snek 🐍");
        data.set_text("come play snek with me");
        data.set_url(&url);
        let _ = nav.share_with_data(&data);
    } else if has("clipboard")
        && !js_sys::Reflect::get(nav.as_ref(), &"clipboard".into())
            .map(|c| c.is_undefined())
            .unwrap_or(true)
    {
        let _ = nav.clipboard().write_text(&url);
        copied.set(true);
        gloo_timers::callback::Timeout::new(1500, move || copied.set(false)).forget();
    } else {
        let _ = window.prompt_with_message_and_default("Copy this link:", &url);
    }
}

fn parse_hash(hash: &str) -> Route {
    let path = hash.trim_start_matches('#').trim_matches('/');
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] | ["games"] => Route::Home,
        ["s", name] => Route::Session(normalize_word(name)),
        ["history"] => Route::History,
        ["compute"] => Route::Compute,
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
// Profile editor
// ---------------------------------------------------------------------------

/// The profile form itself — used as the entire first-run page and inside
/// the edit modal. Saving commits the profile and closes the modal if open.
fn profile_form() -> impl IntoView {
    let store = Store::use_store();
    let draft = RwSignal::new(store.profile.get_untracked());
    // A shufflable window into the (large) emoji list. It changes only when
    // shuffle is pressed — picking from it must leave it alone, or the cell
    // you were aiming at moves as you tap.
    let shown = RwSignal::new(palette::random_emoji_set(
        palette::EMOJI_HAND,
        &draft.get_untracked().emoji,
    ));

    let save = move || {
        let p = draft.get_untracked();
        if p.name.trim().is_empty() {
            return;
        }
        save_profile(&p);
        store.profile.set(p.clone());
        net::send(&ClientMsg::UpdateProfile(p));
        if store.show_profile_editor.get_untracked() {
            close_profile_editor(store);
        }
    };

    view! {
        <div class="profile-form">
            <input
                type="text"
                placeholder="your name"
                maxlength="20"
                prop:value=move || draft.get().name
                on:input=move |ev| {
                    draft.update(|p| p.name = event_target_value(&ev));
                }
            />
                <div class="row" style="margin-top: 10px; margin-bottom: 6px;">
                    <h3 style="margin: 0; align-self: center;">"Emoji"</h3>
                    <button
                        class="mini"
                        style="flex: 0 0 auto;"
                        on:click=move |_| {
                            shown
                                .set(
                                    palette::random_emoji_set(
                                        palette::EMOJI_HAND,
                                        &draft.get_untracked().emoji,
                                    ),
                                )
                        }
                    >
                        "🎲 shuffle"
                    </button>
                </div>
                <div class="emoji-grid">
                    {move || {
                        let current = draft.get().emoji;
                        palette::hand_showing(shown.get(), &current, palette::EMOJI_HAND)
                            .into_iter()
                            .map(|e| {
                                let sel = e.clone();
                                let pick = e.clone();
                                view! {
                                    <div
                                        class="pick"
                                        class:selected=move || draft.get().emoji == sel
                                        on:click=move |_| {
                                            draft.update(|p| p.emoji = pick.clone());
                                        }
                                    >
                                        {e.clone()}
                                    </div>
                                }
                            })
                            .collect_view()
                    }}
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
    }
}

/// The edit-profile modal (opened from the top-right chip) — the same form,
/// wrapped with backdrop dismiss + back-button close.
fn profile_editor() -> impl IntoView {
    let store = Store::use_store();
    view! {
        <div class="modal-back" on:click=move |_| close_profile_editor(store)>
            <div class="modal" on:click=move |ev| ev.stop_propagation()>
                <h3 style="margin-top: 0;">"Who are you?"</h3>
                {profile_form()}
            </div>
        </div>
    }
}
