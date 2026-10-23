//! The session page: lobby (ready-up, config, roster) and the live game
//! (board, countdown, results, scrubber). Mirrors tac's GameSetup/GameActive.

use crate::board::{Extras, board_view};
use crate::net;
use crate::palette;
use crate::store::{ConnStatus, GameView, Popup, Store};
use leptos::prelude::*;
use snek_shared::protocol::{ClientMsg, LobbyState, PAUSES_PER_GAME};
use snek_shared::{BotDifficulty, Dir, GameConfig, PlayerProfile, Pos, Turn, is_bot, is_draw};

pub(crate) const BOARD_SIZES: &[(&str, u8)] = &[("Small", 11), ("Medium", 13), ("Large", 17)];
/// The speeds on offer, by the tick each game starts at. Every game then
/// speeds up as it goes (`GameConfig::tick_ms_at`), so the name is all the
/// lobby shows.
pub(crate) const SPEEDS: &[(&str, u32)] = &[("Chill", 2000), ("Normal", 1000), ("Fast", 500)];

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
            // Outside the greyed-out part: you can always leave.
            {move || match store.popup.get() {
                Some(Popup::Leave(target)) => Some(leave_confirm(target)),
                _ => None,
            }}
        </div>
    }
}

/// "Are you sure?" before leaving the game for `target`.
fn leave_confirm(target: String) -> impl IntoView {
    let store = Store::use_store();
    let name = store
        .lobby
        .with_untracked(|l| l.as_ref().map(|l| l.name.clone()))
        .unwrap_or_else(|| "this game".to_owned());
    let stay = move |_| store.popup.set(None);
    view! {
        <div class="modal-back" on:click=stay>
            <div class="modal" on:click=move |ev| ev.stop_propagation()>
                <p class="confirm-q">"are you sure you want to leave " <b>{name}</b> "?"</p>
                <div class="row confirm-row">
                    <button on:click=stay>"stay"</button>
                    <button class="danger" on:click=move |_| net::leave_to(store, target.clone())>
                        "leave"
                    </button>
                </div>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Lobby
// ---------------------------------------------------------------------------

/// Agreeing to play, wherever you do it (the lobby and the end of a game
/// are the same decision): one switch the width of the screen with both
/// answers on it, so which one you've given is never in doubt. Exactly what
/// the server says, on every phone alike: while we are not connected the
/// whole page is greyed out and can't be touched (see `session_page`).
fn ready_switch(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    let me = store.me.get();
    let i_am_in = lobby.players.iter().any(|p| p.id == me);
    let i_am_ready = lobby.ready.contains(&me);
    let my_colour = store.profile.get_untracked().colour;
    view! {
        <div class="ready-row">
            <button
                class="ready-switch"
                class:on=i_am_ready
                role="switch"
                aria-checked=if i_am_ready { "true" } else { "false" }
                aria-label="ready"
                style=format!("--mine: {my_colour};")
                disabled=!i_am_in
                on:click=move |_| net::send(&ClientMsg::SetReady(!i_am_ready))
            >
                <span class="no">"not ready"</span>
                <span class="yes">"ready"</span>
            </button>
        </div>
    }
}

/// The start button, always there. Until every human is ready it can't be
/// pressed and says, in red, who isn't; then it goes live (and shakes).
fn start_button(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    let my_colour = store.profile.get_untracked().colour;
    let waiting_on: Vec<String> = lobby
        .players
        .iter()
        .filter(|p| !is_bot(&p.id) && !lobby.ready.contains(&p.id))
        .map(|p| p.name.clone())
        .collect();
    let all_ready = waiting_on.is_empty();
    view! {
        <button
            class="big start-button"
            class:shake=all_ready
            style=all_ready.then(|| format!("background: {my_colour};"))
            disabled=!all_ready
            on:click=move |_| net::send(&ClientMsg::Start)
        >
            {if all_ready {
                "Start game".to_owned()
            } else {
                format!("not ready: {}", waiting_on.join(", "))
            }}
        </button>
    }
}

/// How the last game in this lobby ended, if there was one. Tapping it
/// opens that game's end-of-game screen (results and replay).
fn last_game_banner(lobby: &LobbyState) -> Option<AnyView> {
    let winner = lobby.last_winners.first()?;
    let store = Store::use_store();
    let open = lobby
        .last_game_id
        .map(|id| move |_| open_finished_game(store, id));
    if is_draw(&lobby.last_winners) {
        return Some(
            view! {
                <button class="banner rainbow last-game" disabled=open.is_none() on:click=move |ev| {
                    if let Some(open) = open {
                        open(ev);
                    }
                }>
                    "💀 Last game: a draw — nobody made it out"
                </button>
            }
            .into_any(),
        );
    }
    // Looked up among who played, not who's here now: the winner may well
    // have left since.
    let name = lobby
        .last_players
        .iter()
        .find(|p| p.id == winner.player)
        .map_or_else(
            || "someone who left".to_owned(),
            |p| format!("{} {}", p.name, p.emoji),
        );
    let turns = winner.survival_turns;
    Some(
        view! {
            <button class="banner rainbow last-game" disabled=open.is_none() on:click=move |ev| {
                if let Some(open) = open {
                    open(ev);
                }
            }>
                "🏆 Last game: " {name} " survived " {turns} " turns"
            </button>
        }
        .into_any(),
    )
}

/// Show a finished game's end-of-game screen, fetched from history: the
/// same screen everyone saw when it ended, results, replay and all.
fn open_finished_game(store: Store, id: i64) {
    leptos::task::spawn_local(async move {
        let url = format!("/api/history/{id}");
        let Ok(resp) = gloo_net::http::Request::get(&url).send().await else {
            return;
        };
        let Ok(bytes) = resp.binary().await else {
            return;
        };
        if let Ok(record) = postcard::from_bytes::<snek_shared::GameRecord>(&bytes) {
            store.view_turn.set(None);
            store.game.set(Some(GameView {
                config: record.config,
                players: record.players,
                turns: record.turns,
            }));
            crate::autoplay::start(store, None);
        }
    });
}

/// The foods on offer. The hand is this phone's own (see
/// [`palette::FoodHand`]): shuffling deals a new one here and tells no one.
/// Picking from it changes the lobby's food for everyone.
fn food_picker(config: &GameConfig) -> impl IntoView + use<> {
    let store = Store::use_store();
    let current_width = config.board_width;
    let current_tick = config.tick_ms;
    let picked = config.food_emoji.clone();
    let for_shuffle = picked.clone();
    view! {
        <div style="margin-top: 10px;">
            // Heading and shuffle on one line, the hand full width beneath —
            // the same shape as the profile editor's emoji picker.
            <div class="row" style="align-items: center;">
                <label style="flex: 1 1 auto;">"Food"</label>
                <button
                    class="mini"
                    style="flex: 0 0 auto;"
                    on:click=move |_| store.food_hand.set(palette::FoodHand::dealt(&for_shuffle))
                >
                    "🎲 shuffle"
                </button>
            </div>
            <div class="food-hand">
                {move || {
                    store
                        .food_hand
                        .with(|hand| hand.foods().to_vec())
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
                        .collect_view()
                }}
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

/// Everyone in the lobby as chips that wrap onto as many lines as they
/// need: emoji, name, and an × to remove them
/// (asked about first, see [`kick_confirm`]). Nobody gets an × on their own
/// chip. Then the ways to add someone.
fn roster(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();
    let me = store.me.get_untracked();
    let session = lobby.session.clone();
    let copied = RwSignal::new(false);
    view! {
        <ul class="player-chips">
            {lobby
                .players
                .iter()
                .map(|p| {
                    let removable = p.id != me;
                    let who = p.clone();
                    view! {
                        <li class="player-chip" style=format!("background: {};", p.colour)>
                            <span>{p.emoji.clone()} " " {p.name.clone()}</span>
                            {removable
                                .then(|| {
                                    view! {
                                        <button
                                            class="chip-x"
                                            aria-label=format!("remove {}", who.name)
                                            on:click=move |_| store.popup.set(Some(Popup::Kick(who.clone())))
                                        >
                                            "×"
                                        </button>
                                    }
                                })}
                        </li>
                    }
                })
                .collect_view()}
        </ul>
        <div class="add-row">
            <button class="mini" on:click=move |_| store.popup.set(Some(Popup::BotPicker))>
                "🤖 add a bot"
            </button>
            // A human is added the only way one can be: by sending them the
            // link. Same as the share button up top.
            <button class="mini" on:click=move |_| crate::app::share_session(&session, copied)>
                {move || if copied.get() { "copied ✓" } else { "🧑 add a human" }}
            </button>
        </div>
    }
}

/// "Are you sure?" before anyone is taken out of the lobby.
fn kick_confirm(who: &PlayerProfile) -> impl IntoView + use<> {
    let store = Store::use_store();
    let close = move |_| store.popup.set(None);
    let id = who.id.clone();
    view! {
        <div class="modal-back" on:click=close>
            <div class="modal" on:click=move |ev| ev.stop_propagation()>
                <p class="confirm-q">
                    "are you sure you want to remove " <b>{who.emoji.clone()} " " {who.name.clone()}</b>
                    " from the lobby?"
                </p>
                <div class="row confirm-row">
                    <button on:click=close>"keep"</button>
                    <button
                        class="danger"
                        on:click=move |_| {
                            net::send(&ClientMsg::Kick(id.clone()));
                            store.popup.set(None);
                        }
                    >
                        "remove"
                    </button>
                </div>
            </div>
        </div>
    }
}

fn lobby_view(lobby: &LobbyState) -> impl IntoView + use<> {
    let store = Store::use_store();

    let config = lobby.config.clone();
    let name = lobby.name.clone();
    let food = lobby.config.food_emoji.clone();

    view! {
        <div>
            <h2 style="margin-top: 22px; margin-bottom: 14px;">{lobby.name.clone()}</h2>
            {ready_switch(lobby)}
            {last_game_banner(lobby)}
            // The settings and the rules, side by side, each a button that
            // opens its own popup, so the lobby itself is just who's playing.
            <div class="lobby-buttons">
                <button class="settings-button" on:click=move |_| store.popup.set(Some(Popup::Settings))>
                    {settings_summary(&lobby.config)}
                </button>
                <button class="rules-button" on:click=move |_| crate::demo::open(store)>
                    <span class="book">{move || book(store.now_ms.get())}</span>
                    "rules"
                </button>
            </div>
            <h3 style="margin-top: 22px;">"Players"</h3>
            {roster(lobby)}
            // Last thing on the page, always there: the game starts here.
            <div class="start-row">{start_button(lobby)}</div>
            {move || {
                store
                    .popup
                    .get()
                    .map(|popup| match popup {
                        Popup::BotPicker => bot_picker().into_any(),
                        Popup::Settings => settings_popup(&config, &name).into_any(),
                        Popup::Rules(step) => crate::rules::rules_popup(&food, step).into_any(),
                        Popup::Kick(who) => kick_confirm(&who).into_any(),
                        // Drawn by the session page, game or lobby alike.
                        Popup::Leave(_) => ().into_any(),
                    })
            }}
        </div>
    }
}

/// The settings as they stand, as the settings button's label: the food,
/// big, then the board and the speed, each named.
fn settings_summary(config: &GameConfig) -> impl IntoView + use<> {
    let board = BOARD_SIZES
        .iter()
        .find(|(_, w)| *w == config.board_width)
        .map_or("Custom", |(name, _)| name);
    let speed = SPEEDS
        .iter()
        .find(|(_, ms)| *ms == config.tick_ms)
        .map_or("Custom", |(name, _)| name);
    view! {
        <span class="food">{config.food_emoji.clone()}</span>
        <span class="values">"size " <b>{board}</b> <br /> "speed " <b>{speed}</b></span>
    }
}

/// Something rule-ish for the rules button (books, scales, judges, ...),
/// a different one every so often.
fn book(now_ms: f64) -> &'static str {
    const RULEY: &[&str] = &[
        "📖",
        "⚖️",
        "📚",
        "🧑‍⚖️",
        "📕",
        "🔨",
        "📗",
        "👩‍⚖️",
        "📘",
        "🏛️",
        "📙",
        "👨‍⚖️",
        "📜",
        "📏",
        "📓",
        "📋",
    ];
    const EVERY_MS: f64 = 700.0;
    RULEY[crate::num::to_usize(now_ms / EVERY_MS) % RULEY.len()]
}

/// Board, speed and food, in a popup. Any change goes straight to the
/// lobby (and everyone else gets a toast saying so).
fn settings_popup(config: &GameConfig, name: &str) -> impl IntoView + use<> {
    let store = Store::use_store();
    let close = move |_| store.popup.set(None);
    view! {
        <div class="modal-back" on:click=close>
            <div class="modal" on:click=move |ev| ev.stop_propagation()>
                // The name goes when it's settled (enter, or leaving the
                // box), not with every letter: each one would be a rename,
                // and a toast, for everyone else.
                <label>
                    "Name"
                    <input
                        type="text"
                        maxlength="40"
                        prop:value=name.to_owned()
                        on:change=move |ev| {
                            let name = event_target_value(&ev);
                            if snek_shared::protocol::valid_game_name(&name) {
                                net::send(&ClientMsg::Rename(name));
                            }
                        }
                    />
                </label>
                <div style="margin-top: 12px;">{config_row(config)}</div>
                {food_picker(config)}
                <button class="big" style="margin-top: 18px;" on:click=close>
                    "done"
                </button>
            </div>
        </div>
    }
}

/// Difficulty chooser for the house bots — big obvious buttons.
fn bot_picker() -> impl IntoView {
    let store = Store::use_store();
    let pick = move |d: BotDifficulty| {
        move |_| {
            net::send(&ClientMsg::AddBot(d));
            store.popup.set(None);
        }
    };
    view! {
        <div class="modal-back" on:click=move |_| store.popup.set(None)>
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
                    "🐍 medium"
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

/// The countdown words, a second each, which take the last three seconds
/// of the server's countdown; the "you!" spotlight has the time before.
const WORDS: [&str; 3] = ["ready", "steady", "spagheddi!"];
const WORD_MS: f64 = 1000.0;
const WORDS_MS: f64 = 3.0 * WORD_MS;
/// How long a starting game's board takes to fade in (styles.css matches).
const BOARD_FADE_MS: f64 = 900.0;
/// The spotlight's three seconds, before the words: shown, then fading
/// out over the last `SPOT_FADE_MS` (styles.css's `.spot` transition).
const SPOTLIGHT_MS: f64 = 3_000.0;
const SPOT_FADE_MS: f64 = 600.0;

/// Where the start of a game has got to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Coming back from a pause: just "resuming", for a second.
    Resuming,
    /// The board spotlights you.
    You,
    /// The spotlight fading away, nothing else, so it's gone before "ready".
    Clearing,
    /// The plain board, with the countdown over it.
    Countdown,
    /// Under way (or not starting at all).
    Done,
}

/// The start's phase (a game's, or a resume's), from the shared clock. A memo, so the board and the
/// words only hear about it when the phase changes, not five times a
/// second: the whole game view (board SVG included) would otherwise re-render
/// with every tick of the clock.
fn start_phase(starting: bool) -> Memo<Start> {
    let store = Store::use_store();
    Memo::new(move |_| {
        if !starting {
            return Start::Done;
        }
        let countdown = store.countdown.get();
        // The shared clock lags by up to a tick, so "remaining" can read a
        // little over the full countdown: it's still the start (not a
        // resume), and still the spotlight.
        let remaining = countdown.ends - store.now_ms.get();
        if remaining <= 0.0 {
            Start::Done
        } else if countdown.resume && remaining > WORDS_MS + SPOTLIGHT_MS {
            Start::Resuming
        } else if remaining > WORDS_MS + SPOT_FADE_MS {
            Start::You
        } else if remaining > WORDS_MS {
            Start::Clearing
        } else {
            Start::Countdown
        }
    })
}

/// "ready", "steady", "spagheddi!", laid over the middle of the board, each
/// fading in and out in its own second. All three are there from the start
/// with their own delays, so each one gets the whole animation: swapping one
/// word's text for the next would only animate the first. Anyone arriving
/// partway through joins the animation where it is (a negative delay).
fn countdown_words() -> impl IntoView {
    let store = Store::use_store();
    let left = (store.countdown.get_untracked().ends - store.now_ms.get_untracked()).max(0.0);
    let elapsed = WORDS_MS - left;
    view! {
        <div class="countdown">
            {WORDS
                .iter()
                .zip([0.0, WORD_MS, 2.0 * WORD_MS])
                .map(|(word, start)| {
                    view! {
                        <span style=format!("animation-delay: {:.0}ms;", start - elapsed)>
                            {*word}
                        </span>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// Anyone in the game can call a halt — it is a game among friends, not a
/// tournament. The server owns the state, so every screen agrees.
fn pause_button(me: String) -> impl IntoView {
    let store = Store::use_store();
    // What the server says: who has it paused and what's left of mine.
    // Paused by me, I can resume it; paused by someone else, I can't;
    // running, I can pause it while I have pauses left, and once I've used
    // one it says how many.
    move || {
        let (by, left) = store
            .hold
            .with(|h| (h.by.clone(), h.pauses_left.get(&me).copied().unwrap_or(0)));
        let (icon, label, resume) = match by {
            Some(by) if by == me => ("▶️", "resume", true),
            None if left > 0 => ("⏸️", "pause", false),
            _ => return None,
        };
        let spent = !resume && left < PAUSES_PER_GAME;
        Some(view! {
            <div class="pause-row">
                <button
                    class="icon-btn"
                    aria-label=label
                    title=label
                    on:click=move |_| net::send(&ClientMsg::SetPaused(!resume))
                >
                    {icon}
                </button>
                {spent.then(|| view! { <span class="pauses-left">{format!("({left} left)")}</span> })}
            </div>
        })
    }
}

/// Paused: a card over the greyed-out board saying who did it.
fn paused_card(by: &str, players: &[PlayerProfile]) -> impl IntoView + use<> {
    let who = players.iter().find(|p| p.id == by).map_or_else(
        || "someone".to_owned(),
        |p| format!("{} {}", p.emoji, p.name),
    );
    view! {
        <div class="countdown">
            <span class="still">"⏸ " {who} " paused"</span>
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
            {lobby_now
                .map(|lobby| {
                    view! {
                        {ready_switch(lobby)}
                        <div style="margin-top: 12px;">{start_button(lobby)}</div>
                    }
                })}
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
fn scrubber(last: usize) -> impl IntoView {
    let store = Store::use_store();
    let viewing = move || store.view_turn.get().unwrap_or(last).min(last);
    view! {
        <div class="replay-controls">
            // Highlights playing: stop them. Stopped: play them from here.
            <button
                class="icon"
                aria-label=move || if store.autoplay.get() { "stop" } else { "play highlights" }
                on:click=move |_| {
                    if store.autoplay.get_untracked() {
                        crate::autoplay::stop(store);
                    } else {
                        crate::autoplay::start(store, Some(viewing()));
                    }
                }
            >
                {move || if store.autoplay.get() { "⏸" } else { "▶" }}
            </button>
            // Grabbing the slider takes over from the highlights.
            <input
                type="range"
                class="scrubber"
                aria-label="replay"
                min="0"
                max=last.to_string()
                prop:value=move || viewing().to_string()
                on:pointerdown=move |_| crate::autoplay::stop(store)
                on:input=move |ev| {
                    crate::autoplay::stop(store);
                    if let Ok(i) = event_target_value(&ev).parse::<usize>() {
                        store.view_turn.set(Some(i));
                    }
                }
            />
            <span class="turn-count">{move || format!("{}/{last}", viewing())}</span>
        </div>
    }
}

/// A finished game's board: whichever turn the slider or the highlights
/// are on, redrawn as that changes and nothing else.
fn finished_board(game: &GameView, me: &str, last: usize) -> impl IntoView + use<> {
    let store = Store::use_store();
    let shown = StoredValue::new((
        game.config.clone(),
        game.players.clone(),
        game.turns.clone(),
    ));
    let me = me.to_owned();
    move || {
        let i = store.view_turn.get().unwrap_or(last).min(last);
        shown.with_value(|(config, players, turns)| {
            turns
                .get(i)
                .map(|turn| board_view(config, players, turn, &me, Extras::none(), |_| {}))
        })
    }
}

/// The live game (or the one just finished). Nothing to show for a game
/// with no turns, which the server never sends.
fn game_view(game: &GameView) -> Option<impl IntoView + use<>> {
    let store = Store::use_store();
    let me = store.me.get();

    let turn_count = game.turns.len();
    let last = turn_count.saturating_sub(1);
    // Live, the board is the latest turn. Over, it's whichever the slider
    // (or the highlights) shows, and only the board follows it (below): the
    // rest of the screen, the winner's emoji rain included, stays put.
    let turn: Turn = game.turns.get(last)?.clone();
    let last_turn = game.turns.last()?;
    let over = last_turn.game_over();

    // Before the first tick, and again after a pause: the board spotlights
    // you (see board_view) for a second, then the countdown.
    let phase = start_phase(!over);
    // A game starting fades its board in (already spotlit), from wherever
    // in the fade we are.
    let countdown = store.countdown.get_untracked();
    let since_start = net::now() - countdown.began;
    let entering_ms =
        (!over && !countdown.resume && turn_count == 1 && since_start < BOARD_FADE_MS)
            .then_some(since_start.max(0.0));
    let players = game.players.clone();

    // Direction input: tap a cell relative to my head.
    let my_head: Option<Pos> = turn.snakes.get(&me).and_then(|s| s.first().copied());
    let width = i32::from(game.config.board_width);
    let on_cell = move |pos: Pos| {
        if let Some(dir) = my_head.and_then(|head| dir_towards(head, pos, width)) {
            net::send_move(store, dir);
        }
    };

    let spectating = !game.players.iter().any(|p| p.id == me);
    let me_for_pause = me.clone();
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
                let note = if spectating {
                    Some("spectating — you can join next game")
                } else if i_died {
                    Some("you died 💀 watch the rest")
                } else {
                    None
                };
                note.map(|n| view! { <p class="play-note">{n}</p> })
            }}
            {if over {
                finished_board(game, &me, last).into_any()
            } else {
                board_view(
                &game.config,
                &game.players,
                &turn,
                &me,
                Extras {
                    spotlight: !over,
                    spot_on: Signal::derive(move || {
                        phase.get() == Start::You && store.hold.with(|h| h.by.is_none())
                    }),
                    held: Signal::derive(move || store.hold.with(|h| h.by.is_some())),
                    entering_ms,
                    overlay: move || {
                        if let Some(by) = store.hold.with(|h| h.by.clone()) {
                            return Some(paused_card(&by, &players).into_any());
                        }
                        match phase.get() {
                            Start::Resuming => Some(
                                view! {
                                    <div class="countdown">
                                        <span class="still">"resuming"</span>
                                    </div>
                                }
                                    .into_any(),
                            ),
                            Start::Countdown => Some(countdown_words().into_any()),
                            _ => None,
                        }
                    },
                },
                on_cell,
            )
                .into_any()
            }}
            {over.then(|| scrubber(last))}
            // One bottom half, whichever state the game is in. Two layouts
            // meant two sets of rules for the same board, and the board
            // quietly changed size the moment the game ended.
            <div class="controls">
                {move || {
                    (!over && phase.get() == Start::Done && !spectating)
                        .then(|| pause_button(me_for_pause.clone()))
                }}
                {show_dpad.then(dpad)}
                {over.then(|| results(game, last_turn, lobby_now.as_ref()))}
            </div>
        </div>
    })
}
