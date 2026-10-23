//! The session page: lobby (ready-up, config, roster) and the live game
//! (board, countdown, results, scrubber). Mirrors tac's GameSetup/GameActive.

use crate::board::board_view;
use crate::net;
use crate::store::{ConnStatus, GameView, Store};
use leptos::prelude::*;
use snek_shared::protocol::{ClientMsg, LobbyState};
use snek_shared::{BotDifficulty, Dir, GameConfig, Pos, Turn, is_bot, is_draw};

const BOARD_SIZES: &[(&str, u8)] = &[("Small", 11), ("Medium", 13), ("Large", 17)];
const SPEEDS: &[(&str, u32)] = &[
    ("Chill (2s)", 2000),
    ("Normal (1s)", 1000),
    ("Fast (0.5s)", 500),
];

pub(crate) fn session_page(_session: String) -> impl IntoView {
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
            // What's on screen is the server's last word, and while it can't
            // reach us that word may be out of date. Say so by greying it
            // all out and refusing taps, rather than let anyone act on (or
            // be shown) a state other phones no longer share.
            <div class:offline=move || store.conn.get() != ConnStatus::Connected>
            {move || {
                if let Some(game) = store.game.get() {
                    return view! { <div>{game_view(&game)}</div> }.into_any();
                }
                store
                    .lobby
                    .get()
                    .map_or_else(
                        || view! { <div class="banner">"Joining session…"</div> }.into_any(),
                        |lobby| view! { <div>{lobby_view(&lobby)}</div> }.into_any(),
                    )
            }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Lobby
// ---------------------------------------------------------------------------

/// Agreeing to play, wherever you do it. The lobby and the end of a game
/// are the same decision, so they get the same button, the same rules and
/// the same list of who everyone is still waiting on.
fn ready_panel(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    let me = store.me.get();
    let humans: Vec<_> = lobby
        .players
        .iter()
        .filter(|p| !is_bot(&p.id))
        .cloned()
        .collect();
    let i_am_in = humans.iter().any(|p| p.id == me);
    // Exactly what the server says, on every phone alike. While we are not
    // connected the whole page is greyed out and can't be touched (see
    // `session_page`), so there is never a tap the server hasn't heard.
    let i_am_ready = lobby.ready.contains(&me);
    let is_ready = |id: &str| lobby.ready.iter().any(|r| r == id);
    let all_ready = !humans.is_empty() && humans.iter().all(|p| is_ready(&p.id));
    let my_colour = store.profile.get_untracked().colour;
    let not_ready: Vec<String> = humans
        .iter()
        .filter(|p| !is_ready(&p.id))
        .map(|p| p.name.clone())
        .collect();

    let toggle = move |_| net::send(&ClientMsg::SetReady(!i_am_ready));

    view! {
        <div>
            // A question and a switch with both answers on it, so which one
            // you've given is never in doubt.
            <div class="ready-row">
                <span class="ready-q">"Ready?"</span>
                <button
                    class="ready-switch"
                    class:on=i_am_ready
                    role="switch"
                    aria-checked=if i_am_ready { "true" } else { "false" }
                    aria-label="ready"
                    style=format!("--mine: {my_colour};")
                    disabled=!i_am_in
                    on:click=toggle
                >
                    <span class="no">"no"</span>
                    <span class="yes">"yes"</span>
                </button>
            </div>
            {all_ready
                .then(|| {
                    view! {
                        <button
                            class="big shake"
                            style=format!("margin-top: 12px; background: {my_colour};")
                            on:click=move |_| net::send(&ClientMsg::Start)
                        >
                            "Start game"
                        </button>
                    }
                })}
            {(i_am_ready && !not_ready.is_empty())
                .then(|| {
                    view! {
                        <p style="color: #c00; margin: 8px 0 0;">
                            "Waiting on: " {not_ready.join(", ")}
                        </p>
                    }
                })}
        </div>
    }
}

/// Who last touched the settings. Changing them un-readies everyone, so
/// whoever did it is worth knowing — and the moment it happens the name
/// flashes, so a toggle flipping back to "no" never comes out of nowhere.
fn settings_by(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    let who = lobby.config_changed_by.as_ref().map(|id| {
        lobby.players.iter().find(|p| &p.id == id).map_or_else(
            || ("someone who left".to_owned(), "#fff".to_owned()),
            |p| (format!("{} {}", p.emoji, p.name), p.colour.clone()),
        )
    });
    // The lobby is rebuilt whenever anything in it changes, so the flash
    // picks up where it is rather than starting again from the top.
    let since = net::now() - store.settings_flash_at.get_untracked();
    let flashing = since < SETTINGS_FLASH_MS;
    view! {
        <div class="changed-by">
            {match who {
                None => view! { <span class="footnote">"nobody's touched these yet"</span> }.into_any(),
                Some((name, colour)) => {
                    view! {
                        <span class="footnote">"last changed by"</span>
                        <span
                            class="who"
                            class:flash=flashing
                            style=format!(
                                "background: {colour}; animation-delay: -{:.0}ms;",
                                since.max(0.0),
                            )
                        >
                            {name}
                        </span>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

const SETTINGS_FLASH_MS: f64 = 1800.0;

/// How the last game in this lobby ended, if there was one.
fn last_game_banner(lobby: &LobbyState) -> Option<AnyView> {
    let winner = lobby.last_winners.first()?;
    if is_draw(&lobby.last_winners) {
        return Some(
            view! {
                <div class="banner rainbow">"💀 Last game: a draw — nobody made it out"</div>
            }
            .into_any(),
        );
    }
    let name = lobby
        .players
        .iter()
        .find(|p| p.id == winner.player)
        .map_or_else(
            || "someone who left".to_owned(),
            |p| format!("{} {}", p.name, p.emoji),
        );
    let turns = winner.survival_turns;
    Some(
        view! {
            <div class="banner rainbow">
                "🏆 Last game: " {name} " survived " {turns} " turns"
            </div>
        }
        .into_any(),
    )
}

/// The foods on offer, as the server dealt them, so every phone shows the
/// same hand. Shuffling asks the server for a new one; whatever is picked
/// always survives into it, at the front.
fn food_picker(config: &GameConfig, food_hand: Vec<String>) -> impl IntoView + use<> {
    let current_width = config.board_width;
    let current_tick = config.tick_ms;
    let picked = config.food_emoji.clone();
    view! {
        <div style="margin-top: 10px;">
            // Heading and shuffle on one line, the hand full width beneath —
            // the same shape as the profile editor's emoji picker.
            <div class="row" style="align-items: center;">
                <label style="flex: 1 1 auto;">"Food"</label>
                <button
                    class="mini"
                    style="flex: 0 0 auto;"
                    on:click=move |_| net::send(&ClientMsg::ShuffleFood)
                >
                    "🎲 shuffle"
                </button>
            </div>
            <div>
                <div class="food-hand">
                    {food_hand
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
                                        {f}
                                    </div>
                                }
                            })
                            .collect_view()}
                </div>
            </div>
        </div>
    }
}

/// Board size and speed.
fn config_row(config: &GameConfig) -> impl IntoView + use<> {
    let current_width = config.board_width;
    let current_tick = config.tick_ms;
    let food_for_board = config.food_emoji.clone();
    let food_for_speed = config.food_emoji.clone();
    view! {
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
    }
}

/// Everyone in the lobby, whether they're ready, a bin to kick them, and
/// the way to add a bot.
fn roster(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    view! {
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
                        <button class="mini" on:click=move |_| store.show_bot_picker.set(true)>
                            "➕ add a bot"
                        </button>
                    </td>
                </tr>
            </tbody>
        </table>
    }
}

fn lobby_view(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();

    // A game is known by its host: the first human in the lobby. Bots are
    // always ready; only humans gate the start.
    let host_line = lobby.players.iter().find(|p| !is_bot(&p.id)).map_or_else(
        || "new game".to_owned(),
        |p| format!("{} {}'s game", p.emoji, p.name),
    );
    let current_food = lobby.config.food_emoji.clone();

    view! {
        <div>
            <h2 style="margin-top: 22px; margin-bottom: 14px;">{host_line}</h2>
            {ready_panel(lobby)}
            {last_game_banner(lobby)}
            <h3 style="margin-top: 24px;">"Players"</h3>
            {roster(lobby)}
            <h3 style="margin-top: 24px;">"Settings"</h3>
            {settings_by(lobby)}
            <div>
                {config_row(&lobby.config)}
                {food_picker(&lobby.config, lobby.food_hand.clone())}
            </div>
            {move || store.show_bot_picker.get().then(bot_picker)}
            <p class="footnote" style="margin-top: 18px;">
                "Steer with swipes, taps, arrows or WASD. Eat " {current_food}
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
fn emoji_rain(emoji: &str) -> impl IntoView + use<> {
    let drops = (0..36)
        .map(|i| {
            let left = (i * 37 + 13) % 100;
            let delay_ms = (i * 211) % 2800;
            let duration_ms = 2600 + (i * 137) % 2200;
            let size = 20 + (i * 61) % 22;
            // Every drop spins its own way: random-ish amount, either
            // direction.
            let spin = (180 + (i * 97) % 420) * if (i * 7 + 3) % 2 == 0 { 1 } else { -1 };
            view! {
                <span style=format!(
                    "left: {left}%; animation-delay: {delay_ms}ms; \
                     animation-duration: {duration_ms}ms; font-size: {size}px; \
                     --spin: {spin}deg;",
                )>{emoji.to_owned()}</span>
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

/// Which way to steer for a tap on `pos`: towards it from `head`, along
/// whichever axis it is further away on. A tap on the head itself says
/// nothing.
fn dir_towards(head: Pos, pos: Pos, width: i32) -> Option<Dir> {
    let (hx, hy) = (i32::from(head) % width, i32::from(head) / width);
    let (cx, cy) = (i32::from(pos) % width, i32::from(pos) / width);
    let (dx, dy) = (cx - hx, cy - hy);
    if dx == 0 && dy == 0 {
        return None;
    }
    Some(if dx.abs() >= dy.abs() {
        if dx > 0 { Dir::Right } else { Dir::Left }
    } else if dy > 0 {
        Dir::Down
    } else {
        Dir::Up
    })
}

/// "ready", "steady", "spagheddi!" a second each, after the board has
/// spotlit you for a few seconds. The clock is read *inside* the closure:
/// otherwise the whole game view (board SVG included) would re-render five
/// times a second with the shared clock.
fn countdown(show_countdown: bool) -> impl IntoView {
    let store = Store::use_store();
    // A memo, so the word's card is only rebuilt (and pops in) when the
    // word changes, not on every tick of the clock.
    let word = Memo::new(move |_| {
        if !show_countdown {
            return None;
        }
        match store.countdown_ends.get() - store.now_ms.get() {
            r if r > 3000.0 => None,
            r if r > 2000.0 => Some("ready"),
            r if r > 1000.0 => Some("steady"),
            r if r > 0.0 => Some("spagheddi!"),
            _ => None,
        }
    });
    move || {
        word.get().map(|word| {
            view! {
                <div class="overlay countdown">
                    <span>{word}</span>
                </div>
            }
        })
    }
}

/// Anyone in the game can call a halt — it is a game among friends, not a
/// tournament. The server owns the state, so every screen agrees.
fn pause_button() -> impl IntoView {
    let store = Store::use_store();
    view! {
        <div class="pause-row">
            <button
                class="icon-btn"
                aria-label=move || if store.paused.get() { "resume" } else { "pause" }
                title=move || if store.paused.get() { "resume" } else { "pause" }
                on:click=move |_| {
                    net::send(&ClientMsg::SetPaused(!store.paused.get()));
                }
            >
                {move || if store.paused.get() { "▶️" } else { "⏸️" }}
            </button>
        </div>
    }
}

/// Results, ranked, once the game is over — under a rain of the winner's
/// emoji, as is tradition.
fn results(
    game: &GameView,
    last_turn: &Turn,
    lobby_now: Option<&LobbyState>,
) -> impl IntoView + use<> {
    let store = Store::use_store();
    let draw = is_draw(&last_turn.winners);
    // Draws rain skulls; wins rain the winner's emoji.
    let rain = if draw {
        Some(emoji_rain("💀"))
    } else {
        last_turn
            .winners
            .first()
            .and_then(|w| game.players.iter().find(|p| p.id == w.player))
            .map(|p| emoji_rain(&p.emoji))
    };
    let rows = last_turn
        .winners
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let p = game.players.iter().find(|p| p.id == w.player);
            let name = p.map_or_else(|| "???".to_owned(), |p| format!("{} {}", p.name, p.emoji));
            let colour = p.map(|p| p.colour.clone()).unwrap_or_default();
            let medal = if draw {
                "💀" // no winner to allocate
            } else {
                ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("💀")
            };
            let is_human = !is_bot(&w.player);
            let ready_now = lobby_now.is_some_and(|l| l.ready.contains(&w.player));
            let still_here = lobby_now.is_some_and(|l| l.players.iter().any(|p| p.id == w.player));
            view! {
                <tr style=format!("background: {colour};")>
                    <td>
                        {medal} " " {name}
                        // How they did is the row; whether they are up
                        // for another goes underneath, so neither costs
                        // the other a column on a phone.
                        {(is_human && still_here)
                            .then(|| {
                                view! {
                                    <br />
                                    <span class="footnote">
                                        {if ready_now { "ready" } else { "not ready" }}
                                    </span>
                                }
                            })}
                    </td>
                    <td class="right">{w.survival_turns} " turns"</td>
                    <td class="right">"length " {w.length}</td>
                </tr>
            }
        })
        .collect_view();

    view! {
        <div class="results">
            {rain}
            <h3>{if draw { "Game over — it's a draw" } else { "Game over" }}</h3>
            // The next game first: it is what most people want from
            // here. Agreeing to a rematch is agreeing to a game, so it is
            // the lobby's control, not a second one that behaves almost
            // the same.
            {lobby_now.map(ready_panel)}
            <table class="players">
                <tbody>{rows}</tbody>
            </table>
            <div class="row">
                <button on:click=move |_| {
                    store.game.set(None);
                    store.view_turn.set(None);
                }>"Back to lobby"</button>
            </div>
        </div>
    }
}

/// Replay, right under the board it moves, once the game is over.
fn scrubber(viewing: usize, last: usize) -> impl IntoView {
    let store = Store::use_store();
    view! {
        <div class="replay-controls">
            <input
                type="range"
                class="scrubber"
                aria-label="replay"
                min="0"
                max=last.to_string()
                prop:value=viewing.to_string()
                on:input=move |ev| {
                    if let Ok(i) = event_target_value(&ev).parse::<usize>() {
                        store.view_turn.set(Some(i));
                    }
                }
            />
            <span class="turn-count">{format!("{viewing}/{last}")}</span>
        </div>
    }
}

/// The live game (or the one just finished). Nothing to show for a game
/// with no turns, which the server never sends.
fn game_view(game: &GameView) -> Option<impl IntoView + use<>> {
    let store = Store::use_store();
    let me = store.me.get();

    let turn_count = game.turns.len();
    let last = turn_count.saturating_sub(1);
    let viewing = store.view_turn.get().unwrap_or(last).min(last);
    let turn: Turn = game.turns.get(viewing)?.clone();
    let last_turn = game.turns.last()?;
    let over = last_turn.game_over();

    // Before the first tick: the board spotlights you (see board_view) for
    // a few seconds, then the countdown.
    let show_countdown = !over && turn_count == 1;

    // Direction input: tap a cell relative to my head.
    let my_head: Option<Pos> = turn.snakes.get(&me).and_then(|s| s.first().copied());
    let width = i32::from(game.config.board_width);
    let on_cell = move |pos: Pos| {
        if let Some(dir) = my_head.and_then(|head| dir_towards(head, pos, width)) {
            net::send_move(store, dir);
        }
    };

    let spectating = !game.players.iter().any(|p| p.id == me);
    let i_died = !spectating && !over && !last_turn.alive.contains(&me);
    let show_dpad = !over && !spectating && !i_died;
    let lobby_now = store.lobby.get();

    Some(view! {
        // Once it's over nothing has to fit on one screen any more, so the
        // page simply grows and scrolls as a page — never a box of its own.
        <div class=if over { "play over" } else { "play" }>
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
            {board_view(&game.config, &game.players, &turn, &me, show_countdown, on_cell)}
            {over.then(|| scrubber(viewing, last))}
            // One bottom half, whichever state the game is in. Two layouts
            // meant two sets of rules for the same board, and the board
            // quietly changed size the moment the game ended.
            <div class="controls">
                {(!over && !show_countdown).then(pause_button)}
                {show_dpad.then(dpad)}
                {over.then(|| results(game, last_turn, lobby_now.as_ref()))}
            </div>
            {countdown(show_countdown)}
        </div>
    })
}
