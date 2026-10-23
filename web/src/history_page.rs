//! Game history: list finished games from sqlite, replay any of them with a
//! scrubber. The replay blob is the same postcard `GameRecord` the engine
//! produced — decoded with the shared types.

use crate::board::{Extras, board_view};
use crate::num;
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::HistoryEntry;
use snek_shared::{GameRecord, is_draw};

/// Player chips ranked by placement + "game #N in <session>" line.
fn replay_footer(id: i64, meta: &HistoryEntry) -> impl IntoView + use<> {
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

/// One finished game in the list, in its winner's colour; tap to replay it.
fn history_row(e: &HistoryEntry) -> impl IntoView + use<> {
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
            .and_then(|w| e.players.iter().find(|p| p.id == w.player))
            .map(|p| p.colour.clone())
            .unwrap_or_default()
    };
    let id = e.id;
    view! {
        <tr
            class="clickable"
            style=format!("background: {winner};")
            on:click=move |_| crate::app::navigate(&format!("#/history/{id}"))
        >
            <td>{who}</td>
            <td class="right">{e.num_turns} " turns"</td>
        </tr>
    }
}

pub(crate) fn history_page() -> impl IntoView {
    let entries = RwSignal::new(Option::<Vec<HistoryEntry>>::None);
    let next = RwSignal::new(Option::<i64>::None);
    // Cursor trail: the `before` values of the pages we came through, so
    // "newer" can walk back without numbered pages or COUNT queries.
    let trail = RwSignal::new(Vec::<Option<i64>>::new());
    let current = RwSignal::new(Option::<i64>::None);
    let failed = RwSignal::new(false);

    let fetch = move |before: Option<i64>| {
        current.set(before);
        spawn_local(async move {
            let url = before.map_or_else(
                || "/api/history?limit=10".to_owned(),
                |b| format!("/api/history?before={b}&limit=10"),
            );
            match gloo_net::http::Request::get(&url).send().await {
                Ok(resp) => match resp.json::<snek_shared::protocol::HistoryPage>().await {
                    Ok(page) => {
                        entries.set(Some(page.entries));
                        next.set(page.next);
                    }
                    Err(_) => failed.set(true),
                },
                Err(_) => failed.set(true),
            }
        });
    };
    fetch(None);

    let older = move |_| {
        if let Some(n) = next.get_untracked() {
            trail.update(|t| t.push(current.get_untracked()));
            fetch(Some(n));
        }
    };
    let newer = move |_| {
        if let Some(prev) = trail.try_update(Vec::pop).flatten() {
            fetch(prev);
        }
    };

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
                                <tbody>{list.iter().map(history_row).collect_view()}</tbody>
                            </table>
                        }
                            .into_any()
                    }
                }
            }}
            {move || {
                let has_older = next.get().is_some();
                let has_newer = !trail.get().is_empty();
                (has_older || has_newer)
                    .then(|| {
                        view! {
                            <div class="row" style="margin-top: 10px;">
                                <button disabled=!has_newer on:click=newer>"‹ newer"</button>
                                <button
                                    disabled=!has_older
                                    style="text-align: right;"
                                    on:click=older
                                >
                                    "older ›"
                                </button>
                            </div>
                        }
                    })
            }}
            <p>
                <a href="#/">"← home"</a>
            </p>
        </div>
    }
}

/// How much faster than real time each labelled replay speed actually runs.
const SPEED_SCALE: u32 = 8;

/// Advance the frame on our own fast timer: the shared clock only ticks five
/// times a second, far too coarse for the faster speeds. Frames owed are
/// counted from the real time elapsed, so a late timer catches up rather
/// than slowing the replay down. Stops with the page.
fn advance_frames(
    record: RwSignal<Option<GameRecord>>,
    playing: RwSignal<bool>,
    speed: RwSignal<u32>,
    frame: RwSignal<usize>,
) {
    let last_step = StoredValue::new(0.0f64);
    let timer = set_interval_with_handle(
        move || {
            let now = crate::net::now();
            let Some(len) = record.with_untracked(|r| r.as_ref().map(|r| r.turns.len())) else {
                return;
            };
            if !playing.get_untracked() {
                last_step.set_value(now);
                return;
            }
            // The game's own pace at this point in it: games speed up as
            // they go, and so does the replay. A cerebral game had no pace
            // (each turn took as long as its players did), so it replays at
            // a Normal game's.
            let shown = frame.get_untracked();
            let tick_ms = record
                .with_untracked(|r| {
                    r.as_ref().map(|r| {
                        if r.config.cerebral {
                            1000
                        } else {
                            r.config
                                .tick_ms_at(u32::try_from(shown).unwrap_or(u32::MAX))
                        }
                    })
                })
                .unwrap_or(1000);
            let step_ms =
                (f64::from(tick_ms) / f64::from(speed.get_untracked() * SPEED_SCALE)).max(15.0);
            let owed = ((now - last_step.get_value()) / step_ms).floor();
            if owed < 1.0 {
                return;
            }
            // After a pause (or a backgrounded tab) start afresh rather than
            // jumping ahead by everything that was missed.
            let owed = if owed > 8.0 { 1 } else { num::to_usize(owed) };
            last_step.set_value(if owed == 1 {
                now
            } else {
                num::from_count(owed).mul_add(step_ms, last_step.get_value())
            });
            frame.update(|f| {
                *f = (*f + owed).min(len - 1);
                if *f + 1 >= len {
                    playing.set(false);
                }
            });
        },
        std::time::Duration::from_millis(16),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = timer {
            t.clear();
        }
    });
}

/// Play/pause, the scrubber, where we are, and the speed buttons.
fn replay_controls(
    i: usize,
    turns: usize,
    turn_number: u32,
    frame: RwSignal<usize>,
    playing: RwSignal<bool>,
    speed: RwSignal<u32>,
) -> impl IntoView {
    let max = turns - 1;
    view! {
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
                {turn_number} "/" {turns}
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
    }
}

pub(crate) fn replay_page(id: i64) -> impl IntoView {
    let record = RwSignal::new(Option::<GameRecord>::None);
    let meta = RwSignal::new(Option::<HistoryEntry>::None);
    let failed = RwSignal::new(false);
    let frame = RwSignal::new(0usize);
    let playing = RwSignal::new(true);
    // Playback multiplier as labelled (1x, 2x, 4x). Each is really
    // SPEED_SCALE times that over the game's real tick rate: watching a game
    // back at anything like the pace it was played is a slog.
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
        if let Ok(resp) = gloo_net::http::Request::get(&url).send().await
            && let Ok(m) = resp.json::<HistoryEntry>().await
        {
            meta.set(Some(m));
        }
    });

    advance_frames(record, playing, speed, frame);

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
                view! {
                    <div>
                        {board_view(&r.config, &r.players, &turn, "", Extras::none(), |_| {})}
                        {replay_controls(i, r.turns.len(), turn.turn_number, frame, playing, speed)}
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
