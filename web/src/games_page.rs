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
    // Creating: join in the background while the button shows a loading
    // state; navigate only once the lobby has actually arrived, so the
    // session page renders fully populated on its first frame.
    let creating = RwSignal::new(Option::<String>::None);

    let create = move || {
        if creating.get_untracked().is_some() {
            return;
        }
        let id = new_session_id();
        creating.set(Some(id.clone()));
        crate::net::ensure_session(store, &id);
        // Fallback: if the join is slow, navigate anyway and let the
        // session page's machinery take over.
        let fallback_id = id.clone();
        gloo_timers::callback::Timeout::new(3_000, move || {
            if creating.try_get_untracked().flatten() == Some(fallback_id.clone()) {
                let _ = web_sys::window()
                    .unwrap()
                    .location()
                    .set_hash(&format!("#/s/{fallback_id}"));
            }
        })
        .forget();
    };

    // The moment our new lobby lands, transition.
    Effect::new(move |_| {
        let Some(id) = creating.get() else { return };
        let joined = store
            .lobby
            .with(|l| l.as_ref().map(|l| l.session == id).unwrap_or(false));
        if joined {
            creating.set(None);
            let _ = web_sys::window()
                .unwrap()
                .location()
                .set_hash(&format!("#/s/{id}"));
        }
    });

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
            <h1 style="padding-top: 28px;">"Snake,"<br />"but violent*"</h1>

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
                        let count = list.len();
                        view! {
                            <h3 class="games-heading">
                                "Live games " <span class="count">{count}</span>
                            </h3>
                            <ul
                                class="games"
                                style=format!("--mine: {};", store.profile.get_untracked().colour)
                            >
                                {slice.into_iter().map(game_card).collect_view()}
                            </ul>
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
                disabled=move || creating.get().is_some()
                on:click=move |_| create()
            >
                {move || if creating.get().is_some() { "Starting" } else { "Start new game" }}
                "\u{00A0}"
                {rotating}
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

/// One game in the list. Whose game it is reads first and biggest; whether
/// you can still get in, and who's in it, sit quietly underneath; the
/// button says what joining actually does.
fn game_card(s: SessionSummary) -> impl IntoView {
    let href = format!("#/s/{}", s.id);
    // Everyone's face, host first, bots last: who's in it at a glance,
    // without counting. A crowd is cut short rather than wrapped.
    const FACES: usize = 6;
    let extra = s.emojis.len().saturating_sub(FACES);
    let faces: String = s.emojis.iter().take(FACES).cloned().collect();
    view! {
        <li class="game-card" class:playing=s.playing>
            <a href=href>
                <span class="host-tile" style=format!("background: {};", s.host_colour)>
                    {s.host_emoji}
                </span>
                <span class="game-main">
                    <span class="game-title">{s.host_name} "'s game"</span>
                    <span class="game-meta">
                        <span class="status">{if s.playing { "playing" } else { "lobby" }}</span>
                        <span class="faces">
                            {faces}
                            {(extra > 0).then(|| format!(" +{extra}"))}
                        </span>
                    </span>
                </span>
                <span class="join">{if s.playing { "watch" } else { "join" }}</span>
            </a>
        </li>
    }
}
