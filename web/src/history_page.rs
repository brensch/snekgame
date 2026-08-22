//! Game history: list finished games from sqlite, replay any of them with a
//! scrubber. The replay blob is the same postcard `GameRecord` the engine
//! produced — decoded with the shared types.

use crate::board::board_view;
use crate::store::Store;
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::HistoryEntry;
use snek_shared::{is_draw, GameRecord};

/// Player chips ranked by placement + "game #N in <session>" line.
fn replay_footer(id: i64, meta: &HistoryEntry) -> impl IntoView {
    // Winners are stored ranked best-first and cover every player.
    let ranked: Vec<_> = meta
        .winners
        .iter()
        .filter_map(|w| meta.players.iter().find(|p| p.id == w.player))
        .collect();
    let players = if ranked.len() == meta.players.len() {
        ranked
    } else {
        meta.players.iter().collect()
    };
    let draw = is_draw(&meta.winners);
    let chips = players
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let medal = if draw {
                "💀"
            } else {
                ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("💀")
            };
            view! {
                <span class="player-chip" style=format!("background: {};", p.colour)>
                    {medal} " " {p.emoji.clone()} " " {p.name.clone()}
                </span>
            }
        })
        .collect_view();
    view! {
        <div class="chip-row">{chips}</div>
        <p class="footnote" style="margin-top: 6px;">"game #" {id}</p>
    }
}

pub fn history_page() -> impl IntoView {
    let entries = RwSignal::new(Option::<Vec<HistoryEntry>>::None);
    let failed = RwSignal::new(false);

    spawn_local(async move {
        match gloo_net::http::Request::get("/api/history").send().await {
            Ok(resp) => match resp.json::<Vec<HistoryEntry>>().await {
                Ok(list) => entries.set(Some(list)),
                Err(_) => failed.set(true),
            },
            Err(_) => failed.set(true),
        }
    });

    view! {
        <div>
            <h2>"Game history"</h2>
            {move || {
                if failed.get() {
                    return view! { <div class="banner error">"Couldn't load history."</div> }
                        .into_any();
                }
                match entries.get() {
                    None => view! { <div class="banner">"Loading…"</div> }.into_any(),
                    Some(list) if list.is_empty() => {
                        view! { <div class="banner">"No games yet. Go play one!"</div> }
                            .into_any()
                    }
                    Some(list) => {
                        view! {
                            <p class="footnote">"Tap a game to watch the replay."</p>
                            <table class="players">
                                <tbody>
                                    {list
                                        .into_iter()
                                        .map(|e| {
                                            let who = e
                                                .players
                                                .iter()
                                                .map(|p| format!("{}{}", p.emoji, p.name))
                                                .collect::<Vec<_>>()
                                                .join(", ");
                                            // Draws get no winner colour.
                                            let winner = if is_draw(&e.winners) {
                                                String::new()
                                            } else {
                                                e.winners
                                                    .first()
                                                    .and_then(|w| {
                                                        e.players.iter().find(|p| p.id == w.player)
                                                    })
                                                    .map(|p| p.colour.clone())
                                                    .unwrap_or_default()
                                            };
                                            let id = e.id;
                                            view! {
                                                <tr
                                                    class="clickable"
                                                    style=format!("background: {winner};")
                                                    on:click=move |_| {
                                                        let _ = web_sys::window()
                                                            .unwrap()
                                                            .location()
                                                            .set_hash(&format!("#/history/{id}"));
                                                    }
                                                >
                                                    <td>{who}</td>
                                                    <td class="right">{e.num_turns} " turns"</td>
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
            <p>
                <a href="#/">"← home"</a>
            </p>
        </div>
    }
}

pub fn replay_page(id: i64) -> impl IntoView {
    let store = Store::use_store();
    let record = RwSignal::new(Option::<GameRecord>::None);
    let meta = RwSignal::new(Option::<HistoryEntry>::None);
    let failed = RwSignal::new(false);
    let frame = RwSignal::new(0usize);
    let playing = RwSignal::new(true);
    // Playback multiplier over the game's real tick rate. Watching at real
    // speed is a slog, so default to 2x.
    let speed = RwSignal::new(2u32);

    spawn_local(async move {
        let url = format!("/api/history/{id}");
        match gloo_net::http::Request::get(&url).send().await {
            Ok(resp) if resp.ok() => match resp.binary().await {
                Ok(bytes) => match postcard::from_bytes::<GameRecord>(&bytes) {
                    Ok(r) => record.set(Some(r)),
                    Err(_) => failed.set(true),
                },
                Err(_) => failed.set(true),
            },
            _ => failed.set(true),
        }
    });
    spawn_local(async move {
        let url = format!("/api/history/{id}/meta");
        if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
            if let Ok(m) = resp.json::<HistoryEntry>().await {
                meta.set(Some(m));
            }
        }
    });

    // Advance the frame using the shared coarse clock so we don't need our
    // own timer: step when enough game-time has elapsed.
    let last_step = RwSignal::new(0.0f64);
    Effect::new(move |_| {
        let now = store.now_ms.get();
        let Some(r) = record.get() else { return };
        if !playing.get_untracked() {
            return;
        }
        let step_ms =
            (r.config.tick_ms as f64 / speed.get_untracked() as f64).max(120.0);
        if now - last_step.get_untracked() >= step_ms {
            last_step.set(now);
            frame.update(|f| {
                if *f + 1 < r.turns.len() {
                    *f += 1;
                } else {
                    playing.set(false);
                }
            });
        }
    });

    view! {
        <div>
            <h2 style="margin-top: 22px;">"Replay"</h2>
            {move || {
                if failed.get() {
                    return view! { <div class="banner error">"Couldn't load this game."</div> }
                        .into_any();
                }
                let Some(r) = record.get() else {
                    return view! { <div class="banner">"Loading…"</div> }.into_any();
                };
                let i = frame.get().min(r.turns.len() - 1);
                let turn = r.turns[i].clone();
                let max = r.turns.len() - 1;
                view! {
                    <div>
                        {board_view(r.config, r.players.clone(), &turn, String::new(), |_| {})}
                        <div class="replay-controls">
                            <button
                                class="icon"
                                on:click=move |_| {
                                    playing.update(|p| *p = !*p);
                                }
                            >
                                {move || if playing.get() { "⏸" } else { "▶" }}
                            </button>
                            <input
                                type="range"
                                class="scrubber"
                                min="0"
                                max=max.to_string()
                                prop:value=i.to_string()
                                on:input=move |ev| {
                                    playing.set(false);
                                    if let Ok(v) = event_target_value(&ev).parse::<usize>() {
                                        frame.set(v);
                                    }
                                }
                            />
                            <span class="footnote turn-count">
                                {turn.turn_number} "/" {r.turns.len()}
                            </span>
                            <span class="speed-group">
                                {[1u32, 2, 4]
                                    .into_iter()
                                    .map(|s| {
                                        view! {
                                            <button
                                                class="speed"
                                                class:selected=move || speed.get() == s
                                                on:click=move |_| {
                                                    speed.set(s);
                                                    playing.set(true);
                                                }
                                            >
                                                {s} "x"
                                            </button>
                                        }
                                    })
                                    .collect_view()}
                            </span>
                        </div>
                        {move || meta.get().map(|m| replay_footer(id, &m))}
                    </div>
                }
                    .into_any()
            }}
            <p>
                <a href="#/history">"← history"</a>
            </p>
        </div>
    }
}
