//! The session page: lobby (ready-up, config, roster) and the live game
//! (board, countdown, results, scrubber). Mirrors tac's GameSetup/GameActive.

use crate::board::board_view;
use crate::net;
use crate::store::{ConnStatus, GameView, Store};
use leptos::prelude::*;
use snek_shared::protocol::{ClientMsg, LobbyState};
use snek_shared::{Dir, GameConfig, Pos, Turn};

const BOARD_SIZES: &[(&str, u8)] = &[("Small", 11), ("Medium", 13), ("Large", 17)];
const SPEEDS: &[(&str, u32)] = &[("Chill (2s)", 2000), ("Normal (1s)", 1000), ("Fast (0.5s)", 500)];

pub fn session_page(_session: String) -> impl IntoView {
    let store = Store::use_store();

    view! {
        <div>
            {move || match store.conn.get() {
                ConnStatus::Reconnecting | ConnStatus::Connecting => Some(view! {
                    <div class="banner error">"Reconnecting… hold tight 🛟"</div>
                }),
                _ => None,
            }}
            {move || store.error.get().map(|e| view! {
                <div class="banner error">{e}</div>
            })}
            {move || {
                if let Some(game) = store.game.get() {
                    view! { <div>{game_view(game)}</div> }.into_any()
                } else if let Some(lobby) = store.lobby.get() {
                    view! { <div>{lobby_view(lobby)}</div> }.into_any()
                } else {
                    view! { <div class="banner">"Joining session…"</div> }.into_any()
                }
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Lobby
// ---------------------------------------------------------------------------

fn lobby_view(lobby: LobbyState) -> impl IntoView {
    let store = Store::use_store();
    let me = store.me.get();

    let humans: Vec<_> = lobby.players.clone();
    let i_am_in = humans.iter().any(|p| p.id == me);
    let i_am_ready = lobby.ready.contains(&me);
    let all_ready = !humans.is_empty() && humans.iter().all(|p| lobby.ready.contains(&p.id));
    let my_colour = store.profile.get_untracked().colour;

    let current_width = lobby.config.board_width;
    let current_tick = lobby.config.tick_ms;

    let not_ready: Vec<String> = humans
        .iter()
        .filter(|p| !lobby.ready.contains(&p.id))
        .map(|p| p.name.clone())
        .collect();

    let last_game = (!lobby.last_winners.is_empty()).then(|| {
        let winner = lobby.last_winners.first().unwrap().clone();
        let name = lobby
            .players
            .iter()
            .find(|p| p.id == winner.player)
            .map(|p| format!("{} {}", p.name, p.emoji))
            .unwrap_or_else(|| "someone who left".to_string());
        view! {
            <div class="banner rainbow">
                "🏆 Last game: " {name} " survived " {winner.survival_turns} " turns"
            </div>
        }
    });

    let ready_button = if !all_ready {
        view! {
            <button
                class="big"
                style=format!("background: {my_colour};")
                disabled=i_am_ready || !i_am_in
                on:click=move |_| net::send(&ClientMsg::SetReady(true))
            >
                {if i_am_ready { "Waiting" } else { "I'm ready!" }}
            </button>
        }
        .into_any()
    } else {
        view! {
            <button
                class="big shake"
                style=format!("background: {my_colour};")
                on:click=move |_| net::send(&ClientMsg::Start)
            >
                "Start game"
            </button>
        }
        .into_any()
    };

    let config_row = view! {
        <div class="row">
            <label>
                "Board"
                <select on:change=move |ev| {
                    let val: u8 = event_target_value(&ev).parse().unwrap_or(13);
                    net::send(&ClientMsg::Configure(GameConfig {
                        board_width: val,
                        board_height: val,
                        tick_ms: current_tick,
                    }));
                }>
                    {BOARD_SIZES
                        .iter()
                        .map(|(label, w)| {
                            view! {
                                <option value=w.to_string() selected=*w == current_width>
                                    {*label}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>
            <label>
                "Speed"
                <select on:change=move |ev| {
                    let val: u32 = event_target_value(&ev).parse().unwrap_or(1000);
                    net::send(&ClientMsg::Configure(GameConfig {
                        board_width: current_width,
                        board_height: current_width,
                        tick_ms: val,
                    }));
                }>
                    {SPEEDS
                        .iter()
                        .map(|(label, ms)| {
                            view! {
                                <option value=ms.to_string() selected=*ms == current_tick>
                                    {*label}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>
        </div>
    };

    let roster = view! {
        <table>
            <thead>
                <tr>
                    <th>"Player"</th>
                    <th class="right">"Ready"</th>
                    <th class="right">"Remove?"</th>
                </tr>
            </thead>
            <tbody>
                {humans
                    .iter()
                    .map(|p| {
                        let ready = lobby.ready.contains(&p.id);
                        let id = p.id.clone();
                        view! {
                            <tr style=format!("background: {};", p.colour)>
                                <td>{p.name.clone()} " " {p.emoji.clone()}</td>
                                <td class="right">{if ready { "Yeah" } else { "Nah" }}</td>
                                <td
                                    class="right"
                                    style="cursor: pointer;"
                                    on:click=move |_| net::send(&ClientMsg::Kick(id.clone()))
                                >
                                    "❌"
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    };

    view! {
        <div>
            {ready_button}
            {(i_am_ready && !not_ready.is_empty())
                .then(|| {
                    view! {
                        <p style="color: #c00;">"Not ready: " {not_ready.join(", ")}</p>
                    }
                })}
            {last_game}
            {config_row}
            <div class="banner">
                <b>"Rules"</b>
                <br />
                "Steer your snek with swipes, taps, arrows or WASD.\u{00A0}"
                "Eat 🎃 to grow and refill health; you lose 1 health per turn. "
                "Walls, bodies and bigger heads kill you. Outlive everyone."
            </div>
            {roster}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Live game
// ---------------------------------------------------------------------------

/// On-screen d-pad — the primary controller on touch screens. The selected
/// direction lights up in the player's colour.
fn dpad() -> impl IntoView {
    let store = Store::use_store();
    let press = move |dir: Dir| move |_| net::send_move(store, dir);
    let style = move |dir: Dir| {
        move || {
            if store.selected_dir.get() == Some(dir) {
                format!("background: {};", store.profile.get().colour)
            } else {
                String::new()
            }
        }
    };
    view! {
        <div class="dpad">
            <button class="d-up" style=style(Dir::Up) on:click=press(Dir::Up)>"▲"</button>
            <button class="d-left" style=style(Dir::Left) on:click=press(Dir::Left)>"◀"</button>
            <div class="d-mid"></div>
            <button class="d-right" style=style(Dir::Right) on:click=press(Dir::Right)>"▶"</button>
            <button class="d-down" style=style(Dir::Down) on:click=press(Dir::Down)>"▼"</button>
        </div>
    }
}

fn game_view(game: GameView) -> impl IntoView {
    let store = Store::use_store();
    let me = store.me.get();

    let turn_count = game.turns.len();
    let viewing = store
        .view_turn
        .get()
        .unwrap_or(turn_count.saturating_sub(1))
        .min(turn_count.saturating_sub(1));
    let turn: Turn = game.turns[viewing].clone();
    let last_turn = game.turns.last().unwrap().clone();
    let over = last_turn.game_over();

    // Countdown overlay before the first tick.
    let countdown = {
        let remaining = store.countdown_ends.get() - store.now_ms.get();
        (!over && turn_count == 1 && remaining > 0.0)
            .then(|| view! { <div class="overlay">{format!("{:.0}", (remaining / 1000.0).ceil())}</div> })
    };

    // Direction input: tap a cell relative to my head.
    let my_head: Option<Pos> = turn.snakes.get(&me).map(|s| s[0]);
    let width = game.config.board_width as i32;
    let on_cell = move |pos: Pos| {
        let Some(head) = my_head else { return };
        let (hx, hy) = (head as i32 % width, head as i32 / width);
        let (cx, cy) = (pos as i32 % width, pos as i32 / width);
        let (dx, dy) = (cx - hx, cy - hy);
        if dx == 0 && dy == 0 {
            return;
        }
        let dir = if dx.abs() >= dy.abs() {
            if dx > 0 { Dir::Right } else { Dir::Left }
        } else if dy > 0 {
            Dir::Down
        } else {
            Dir::Up
        };
        net::send_move(store, dir);
    };

    let spectating = !game.players.iter().any(|p| p.id == me);
    let i_died = !spectating && !over && !last_turn.alive.contains(&me);
    let show_dpad = !over && !spectating && !i_died;

    // Results, ranked, once the game is over.
    let results = over.then(|| {
        let rows = last_turn
            .winners
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let p = game.players.iter().find(|p| p.id == w.player);
                let name = p
                    .map(|p| format!("{} {}", p.name, p.emoji))
                    .unwrap_or_else(|| "???".to_string());
                let colour = p.map(|p| p.colour.clone()).unwrap_or_default();
                let medal = ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("💀");
                view! {
                    <tr style=format!("background: {colour};")>
                        <td>{medal} " " {name}</td>
                        <td class="right">{w.survival_turns} " turns"</td>
                        <td class="right">"length " {w.length}</td>
                    </tr>
                }
            })
            .collect_view();

        let scrub = view! {
            <input
                type="range"
                class="scrubber"
                min="0"
                max=(turn_count - 1).to_string()
                prop:value=viewing.to_string()
                on:input=move |ev| {
                    if let Ok(i) = event_target_value(&ev).parse::<usize>() {
                        store.view_turn.set(Some(i));
                    }
                }
            />
        };

        view! {
            <div>
                <h3>"Game over"</h3>
                <table>
                    <tbody>{rows}</tbody>
                </table>
                <p class="footnote">"Drag to replay:"</p>
                {scrub}
                <button
                    class="big"
                    on:click=move |_| {
                        store.game.set(None);
                        store.view_turn.set(None);
                    }
                >
                    "Back to lobby"
                </button>
            </div>
        }
    });

    view! {
        <div>
            <p>
                "Turn " {turn.turn_number}
                {spectating.then_some(" — spectating (next game starts from the lobby)")}
                {i_died.then_some(" — you died 💀 watch the rest")}
            </p>
            {board_view(game.config, game.players.clone(), &turn, me.clone(), on_cell)}
            {show_dpad.then(dpad)}
            {countdown}
            {results}
        </div>
    }
}
