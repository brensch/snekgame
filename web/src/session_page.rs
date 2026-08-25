//! The session page: lobby (ready-up, config, roster) and the live game
//! (board, countdown, results, scrubber). Mirrors tac's GameSetup/GameActive.

use crate::board::board_view;
use crate::net;
use crate::palette;
use crate::store::{ConnStatus, GameView, Store};
use leptos::prelude::*;
use snek_shared::protocol::{ClientMsg, LobbyState};
use snek_shared::{is_bot, is_draw, BotDifficulty, Dir, GameConfig, Pos, Turn};

const BOARD_SIZES: &[(&str, u8)] = &[("Small", 11), ("Medium", 13), ("Large", 17)];
const SPEEDS: &[(&str, u32)] = &[("Chill (2s)", 2000), ("Normal (1s)", 1000), ("Fast (0.5s)", 500)];

pub fn session_page(_session: String) -> impl IntoView {
    let store = Store::use_store();

    view! {
        <div>
            {move || {
                // Fresh joins show the quiet "Joining session…" body below;
                // the pigeons are reserved for genuinely lost connections.
                (store.conn.get() == ConnStatus::Reconnecting)
                    .then(|| {
                        view! {
                            <div class="banner error">"🕊️ carrier pigeons can't find you"</div>
                        }
                    })
            }}
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

    // Bots are always ready; only humans gate the start.
    let humans: Vec<_> = lobby
        .players
        .iter()
        .filter(|p| !is_bot(&p.id))
        .cloned()
        .collect();
    let i_am_in = humans.iter().any(|p| p.id == me);
    // Our own press counts immediately. The server is the truth, but while
    // a reconnect is in flight it has not heard us yet, and a button that
    // ignores a press looks broken even though the intent is safely held.
    let i_am_ready = lobby.ready.contains(&me) || store.want_ready.get();
    let all_ready = !humans.is_empty() && humans.iter().all(|p| lobby.ready.contains(&p.id));
    let my_colour = store.profile.get_untracked().colour;

    let current_width = lobby.config.board_width;
    let current_tick = lobby.config.tick_ms;
    let current_food = lobby.config.food_emoji.clone();

    let not_ready: Vec<String> = humans
        .iter()
        .filter(|p| !lobby.ready.contains(&p.id))
        .map(|p| p.name.clone())
        .collect();

    let last_game = (!lobby.last_winners.is_empty()).then(|| {
        if is_draw(&lobby.last_winners) {
            return view! {
                <div class="banner rainbow">
                    "💀 Last game: a draw — nobody made it out"
                </div>
            }
            .into_any();
        }
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
        .into_any()
    });

    let ready_button = if !all_ready {
        view! {
            <button
                class="big"
                style=format!("background: {my_colour};")
                disabled=i_am_ready || !i_am_in
                on:click=move |_| {
                    store.want_ready.set(true);
                    net::send(&ClientMsg::SetReady(true));
                }
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

    // The food on offer this round, held in the store so that picking from
    // it doesn't re-deal it. Shuffling deals a new hand; whatever is
    // currently picked always survives into it, at the front.
    let food_hand = store.food_hand;
    let food_for_board = current_food.clone();
    let food_for_speed = current_food.clone();
    let food_now = current_food.clone();
    let food_for_shuffle = current_food.clone();
    let food_picker = view! {
        <div style="margin-top: 10px;">
            // Heading and shuffle on one line, the hand full width beneath —
            // the same shape as the profile editor's emoji picker.
            <div class="row" style="align-items: center;">
                <label style="flex: 1 1 auto;">"Food"</label>
                <button
                    class="mini"
                    style="flex: 0 0 auto;"
                    on:click=move |_| {
                        food_hand
                            .set(
                                palette::shuffled_with(
                                    palette::FOODS,
                                    palette::FOOD_HAND,
                                    &food_for_shuffle,
                                ),
                            );
                    }
                >
                    "🎲 shuffle"
                </button>
            </div>
            <div>
                <div class="food-hand">
                    {move || {
                        let picked = food_now.clone();
                        palette::hand_showing(food_hand.get(), &picked, palette::FOOD_HAND)
                            .into_iter()
                            .map(|f| {
                                let is_picked = f == picked;
                                let send = f.clone();
                                view! {
                                    <div
                                        class="pick"
                                        class:selected=is_picked
                                        on:click=move |_| {
                                            net::send(
                                                &ClientMsg::Configure(GameConfig {
                                                    board_width: current_width,
                                                    board_height: current_width,
                                                    tick_ms: current_tick,
                                                    food_emoji: send.clone(),
                                                }),
                                            );
                                        }
                                    >
                                        {f.clone()}
                                    </div>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </div>
        </div>
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
                        food_emoji: food_for_board.clone(),
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
                        food_emoji: food_for_speed.clone(),
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
    let config_row = view! {
        <div>
            {config_row}
            {food_picker}
        </div>
    };

    let roster = view! {
        <table class="players">
            <tbody>
                {lobby
                    .players
                    .iter()
                    .map(|p| {
                        let ready = is_bot(&p.id) || lobby.ready.contains(&p.id);
                        let id = p.id.clone();
                        view! {
                            <tr style=format!("background: {};", p.colour)>
                                <td>{p.emoji.clone()} " " {p.name.clone()}</td>
                                <td class="right">{if ready { "ready" } else { "not ready" }}</td>
                                <td
                                    class="right kick"
                                    on:click=move |_| net::send(&ClientMsg::Kick(id.clone()))
                                >
                                    "🗑️"
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
                <tr class="add-bot-row">
                    <td colspan="3">
                        <button
                            class="mini"
                            on:click=move |_| store.show_bot_picker.set(true)
                        >
                            "➕ add a bot"
                        </button>
                    </td>
                </tr>
            </tbody>
        </table>
    };

    // A game is known by its host: the first human in the lobby.
    let host_line = humans
        .first()
        .map(|p| format!("{} {}'s game", p.emoji, p.name))
        .unwrap_or_else(|| "new game".to_string());

    view! {
        <div>
            <h2 style="margin-top: 22px; margin-bottom: 14px;">{host_line}</h2>
            {ready_button}
            {(i_am_ready && !not_ready.is_empty())
                .then(|| {
                    view! {
                        <p style="color: #c00; margin: 8px 0 0;">
                            "Waiting on: " {not_ready.join(", ")}
                        </p>
                    }
                })}
            {last_game}
            <h3 style="margin-top: 24px;">"Players"</h3>
            {roster}
            <h3 style="margin-top: 24px;">"Settings"</h3>
            {config_row}
            {move || store.show_bot_picker.get().then(bot_picker)}
            <p class="footnote" style="margin-top: 18px;">
                "Steer with swipes, taps, arrows or WASD. Eat " {current_food.clone()}
                " to grow and "
                "refill health; you lose 1 health per turn. Walls, bodies and "
                "bigger heads kill you. Outlive everyone."
            </p>
        </div>
    }
}

/// Difficulty chooser for the house bots — big obvious buttons.
fn bot_picker() -> impl IntoView {
    let store = Store::use_store();
    let pick = move |d: BotDifficulty| {
        move |_| {
            net::send(&ClientMsg::AddBot(d));
            store.show_bot_picker.set(false);
        }
    };
    view! {
        <div class="modal-back" on:click=move |_| store.show_bot_picker.set(false)>
            <div class="modal" on:click=move |ev| ev.stop_propagation()>
                <h3 style="margin-top: 0;">"Add a bot"</h3>
                <button class="big" style="margin-bottom: 10px;" on:click=pick(BotDifficulty::Easy)>
                    "🐛 easy"
                </button>
                <button
                    class="big"
                    style="margin-bottom: 10px;"
                    on:click=pick(BotDifficulty::Medium)
                >
                    "🤖 medium"
                </button>
                <button class="big" on:click=pick(BotDifficulty::Hard)>"🐲 hard"</button>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Live game
// ---------------------------------------------------------------------------

/// The winner's emoji rains down over the results, tac-style. Positions and
/// timings are pseudo-random from the index so no timers are needed.
fn emoji_rain(emoji: String) -> impl IntoView {
    let drops = (0..36)
        .map(|i| {
            let left = (i * 37 + 13) % 100;
            let delay_ms = (i * 211) % 2800;
            let duration_ms = 2600 + (i * 137) % 2200;
            let size = 20 + (i * 61) % 22;
            // Every drop spins its own way: random-ish amount, either
            // direction.
            let spin = (180 + (i * 97) % 420) as i32 * if (i * 7 + 3) % 2 == 0 { 1 } else { -1 };
            view! {
                <span style=format!(
                    "left: {left}%; animation-delay: {delay_ms}ms; \
                     animation-duration: {duration_ms}ms; font-size: {size}px; \
                     --spin: {spin}deg;",
                )>{emoji.clone()}</span>
            }
        })
        .collect_view();
    view! { <div class="emoji-rain">{drops}</div> }
}

/// On-screen d-pad — the primary controller on touch screens. The selected
/// direction lights up in the player's colour.
fn dpad() -> impl IntoView {
    let store = Store::use_store();
    // pointerdown, not click: click waits for the release, and on a touch
    // screen that wait is most of what makes a control feel sluggish. The
    // move goes the instant the thumb lands.
    let press = move |dir: Dir| move |_: web_sys::PointerEvent| net::send_move(store, dir);
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
            <button class="d-up" style=style(Dir::Up) on:pointerdown=press(Dir::Up)>
                <span class="tri"></span>
            </button>
            <button class="d-left" style=style(Dir::Left) on:pointerdown=press(Dir::Left)>
                <span class="tri"></span>
            </button>
            <button class="d-right" style=style(Dir::Right) on:pointerdown=press(Dir::Right)>
                <span class="tri"></span>
            </button>
            <button class="d-down" style=style(Dir::Down) on:pointerdown=press(Dir::Down)>
                <span class="tri"></span>
            </button>
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

    // Countdown overlay before the first tick. Read the clock *inside* a
    // closure: otherwise this whole view (board SVG included) re-renders
    // five times a second with the shared clock.
    let show_countdown = !over && turn_count == 1;
    let countdown = move || {
        if !show_countdown {
            return None;
        }
        let remaining = store.countdown_ends.get() - store.now_ms.get();
        (remaining > 0.0).then(|| {
            view! { <div class="overlay">{format!("{:.0}", (remaining / 1000.0).ceil())}</div> }
        })
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

    // Anyone in the game can call a halt — it is a game among friends, not a
    // tournament. The server owns the state, so every screen agrees.
    let pause_button = (!over && !show_countdown).then(|| {
        view! {
            <div class="pause-row">
                <button
                    class="mini"
                    on:click=move |_| {
                        net::send(&ClientMsg::SetPaused(!store.paused.get()));
                    }
                >
                    {move || if store.paused.get() { "▶️ resume" } else { "⏸️ pause" }}
                </button>
            </div>
        }
    });

    // Results, ranked, once the game is over — under a rain of the winner's
    // emoji, as is tradition.
    let results = over.then(|| {
        let draw = is_draw(&last_turn.winners);
        // Draws rain skulls; wins rain the winner's emoji.
        let rain = if draw {
            Some(emoji_rain("💀".to_string()))
        } else {
            last_turn
                .winners
                .first()
                .and_then(|w| game.players.iter().find(|p| p.id == w.player))
                .map(|p| emoji_rain(p.emoji.clone()))
        };
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
                let medal = if draw {
                    "💀" // no winner to allocate
                } else {
                    ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("💀")
                };
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
                {rain}
                <h3>{if draw { "Game over — it's a draw" } else { "Game over" }}</h3>
                <table class="players">
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
        <div class=move || if over { "" } else { "play" }>
            // Only what has to be said. The turn number was costing a line
            // of a phone screen to tell you something the board already
            // shows.
            {move || {
                let note = if store.paused.get() {
                    Some("held ⏸️")
                } else if spectating {
                    Some("spectating — next game starts from the lobby")
                } else if i_died {
                    Some("you died 💀 watch the rest")
                } else {
                    None
                };
                note.map(|n| view! { <p class="play-note">{n}</p> })
            }}
            {board_view(game.config, game.players.clone(), &turn, me.clone(), on_cell)}
            <div class="controls">
                {pause_button}
                {show_dpad.then(dpad)}
            </div>
            {countdown}
            {results}
        </div>
    }
}
