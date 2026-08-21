//! Game history: list finished games from sqlite, replay any of them with a
//! scrubber. The replay blob is the same postcard `GameRecord` the engine
//! produced — decoded with the shared types.

use crate::board::board_view;
use crate::store::Store;
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::HistoryEntry;
use snek_shared::GameRecord;

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
                            <table>
                                <thead>
                                    <tr>
                                        <th>"Session"</th>
                                        <th>"Players"</th>
                                        <th class="right">"Turns"</th>
                                        <th class="right">"Watch"</th>
                                    </tr>
                                </thead>
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
                                            let winner = e
                                                .winners
                                                .first()
                                                .and_then(|w| {
                                                    e.players.iter().find(|p| p.id == w.player)
                                                })
                                                .map(|p| p.colour.clone())
                                                .unwrap_or_default();
                                            let href = format!("#/history/{}", e.id);
                                            view! {
                                                <tr style=format!("background: {winner};")>
                                                    <td>{e.session}</td>
                                                    <td>{who}</td>
                                                    <td class="right">{e.num_turns}</td>
                                                    <td class="right">
                                                        <a href=href>"▶"</a>
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
            <p>
                <a href="#/">"← home"</a>
            </p>
        </div>
    }
}

pub fn replay_page(id: i64) -> impl IntoView {
    let store = Store::use_store();
    let record = RwSignal::new(Option::<GameRecord>::None);
    let failed = RwSignal::new(false);
    let frame = RwSignal::new(0usize);
    let playing = RwSignal::new(true);

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

    // Advance the frame using the shared coarse clock so we don't need our
    // own timer: step when enough game-time has elapsed.
    let last_step = RwSignal::new(0.0f64);
    Effect::new(move |_| {
        let now = store.now_ms.get();
        let Some(r) = record.get() else { return };
        if !playing.get_untracked() {
            return;
        }
        let step_ms = (r.config.tick_ms as f64).max(200.0);
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
            <h2>"Replay #" {id}</h2>
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
                        <p>"Turn " {turn.turn_number} " / " {r.turns.len()}</p>
                        {board_view(r.config, r.players.clone(), &turn, String::new(), |_| {})}
                        <div class="row" style="margin-top: 10px;">
                            <button on:click=move |_| {
                                playing.update(|p| *p = !*p);
                            }>
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
                        </div>
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
