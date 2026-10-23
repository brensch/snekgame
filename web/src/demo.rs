//! The rules, as steps to page through: how to steer, what the numbers on
//! a snek mean, eating, and every way to die, then the one rule that
//! matters. The board steps are played by the real engine and drawn by the
//! real board, so each death looks exactly as it will in a game; a moving
//! step plays from its start to the end, holds, and plays again. Driven off
//! the shared clock, so it needs no timer of its own and stops when the
//! popup closes.

use crate::board::board_svg;
use crate::store::{Popup, Store};
use leptos::prelude::*;
use snek_shared::{Dir, Engine, GameConfig, PlayerId, PlayerProfile, Pos, Turn};
use std::cell::Cell;
use std::collections::BTreeMap;

thread_local! {
    /// When the step showing was shown. Kept here rather than in the
    /// popup's view: the lobby redraws its popups whenever anything in it
    /// changes, and a step shouldn't start over every time it does.
    static STARTED: Cell<f64> = const { Cell::new(0.0) };
}

/// Open the rules at the first step.
pub(crate) fn open(store: Store) {
    show(store, 0);
}

/// Show step `step`, from its start.
pub(crate) fn show(store: Store, step: usize) {
    STARTED.with(|s| s.set(crate::net::now()));
    store.popup.set(Some(Popup::Rules(step)));
}

/// A 6×6 playing field (the board's walls sit just outside it).
const BOARD: u8 = 8;
const YOU: &str = "you";
const THEM: &str = "them";

const MOVE_MS: f64 = 420.0;
/// How long a moving step holds on how it ended before playing again.
const END_MS: f64 = 2_000.0;

/// A cell on the playing field, from (0, 0) at its top left.
fn at(x: u16, y: u16) -> Pos {
    (y + 1) * u16::from(BOARD) + x + 1
}

fn config(food: &str) -> GameConfig {
    GameConfig {
        board_width: BOARD,
        board_height: BOARD,
        food_emoji: food.to_owned(),
        ..GameConfig::default()
    }
}

/// Where most scenes start: you along the bottom heading right at the
/// food, the other snek (shorter) along the top heading left.
fn start() -> Turn {
    position(
        vec![at(2, 4), at(1, 4), at(0, 4), at(0, 3)],
        vec![at(3, 1), at(4, 1), at(5, 1)],
    )
}

/// Head to head's start: you heading straight up one column, the other
/// snek about to head straight down it.
fn facing() -> Turn {
    let mut turn = position(
        vec![at(2, 4), at(2, 5), at(1, 5), at(0, 5)],
        vec![at(2, 0), at(1, 0), at(0, 0)],
    );
    // No food: the sizes are labelled where it would be.
    turn.food.clear();
    turn
}

/// Into a body's start: you along the bottom about to climb the left side,
/// the other snek along the top about to come down it, into the pocket
/// you'll have made against the wall.
fn pocket() -> Turn {
    position(
        vec![at(0, 4), at(1, 4), at(2, 4), at(3, 4), at(4, 4)],
        vec![at(2, 0), at(3, 0), at(4, 0)],
    )
}

/// Both sneks where given (head first), the food where it always is.
fn position(you: Vec<Pos>, them: Vec<Pos>) -> Turn {
    Turn {
        turn_number: 1,
        snakes: BTreeMap::from([(YOU.to_owned(), you), (THEM.to_owned(), them)]),
        health: BTreeMap::from([(YOU.to_owned(), 90), (THEM.to_owned(), 95)]),
        alive: vec![YOU.to_owned(), THEM.to_owned()],
        food: vec![at(5, 4)],
        moves: BTreeMap::new(),
        clashes: Vec::new(),
        winners: Vec::new(),
    }
}

/// A note written on the board: where its first line starts, in field
/// cells (x along, y down, from the field's top left), and its two lines.
type Note = ((f64, f64), [&'static str; 2]);

/// A hold along the way, after turn `after`: cells ringed, and notes.
struct Pause {
    after: usize,
    rings: &'static [(u16, u16)],
    notes: &'static [Note],
    ms: f64,
}

/// Something played out on the board: where it starts, both sneks' moves
/// turn by turn, anything to hold on along the way, and the cell to ring
/// once it's over.
pub(crate) struct Scene {
    start: fn() -> Turn,
    moves: &'static [(Dir, Dir)],
    pauses: &'static [Pause],
    ends_on: Option<(u16, u16)>,
}

/// What a step shows.
pub(crate) enum Show {
    /// The arrow keys, drawn.
    Steer,
    /// The starting board: a cell ringed, notes on it.
    Still((u16, u16), &'static [Note]),
    /// A scene, on a loop.
    Play(Scene),
    /// No board at all.
    Fun,
}

/// One step: what it shows and what it says.
pub(crate) struct Step {
    pub show: Show,
    pub says: &'static str,
}

/// You eat: right along the bottom to the food, while the other snek comes
/// down towards where your butt will be.
const EAT: &[(Dir, Dir)] = &[
    (Dir::Right, Dir::Left),
    (Dir::Right, Dir::Down),
    (Dir::Right, Dir::Down),
];

/// Eating, then the other snek into the butt it left.
const INTO_THE_BUTT: &[(Dir, Dir)] = &[
    (Dir::Right, Dir::Left),
    (Dir::Right, Dir::Down),
    (Dir::Right, Dir::Down),
    (Dir::Up, Dir::Down),
];

pub(crate) const STEPS: &[Step] = &[
    Step {
        show: Show::Steer,
        says: "steer: swipe, tap the board, arrow keys or WASD",
    },
    Step {
        show: Show::Still((1, 4), &[((1.0, 3.88), ["length", ""])]),
        says: "eat to grow",
    },
    Step {
        show: Show::Still((0, 4), &[((0.05, 5.55), ["health", ""])]),
        says: "lose one health per move",
    },
    Step {
        show: Show::Play(Scene {
            start,
            moves: EAT,
            pauses: &[],
            ends_on: Some((3, 4)),
        }),
        says: "eat: health back to 100",
    },
    Step {
        show: Show::Play(Scene {
            start,
            moves: INTO_THE_BUTT,
            pauses: &[Pause {
                after: 3,
                rings: &[(2, 4)],
                notes: &[],
                ms: 2_400.0,
            }],
            ends_on: Some((2, 4)),
        }),
        says: "you ate, so your butt grows on the next turn. if you move there you'll run into it!",
    },
    Step {
        show: Show::Play(Scene {
            start,
            moves: &[
                (Dir::Right, Dir::Left),
                (Dir::Right, Dir::Left),
                (Dir::Up, Dir::Left),
                (Dir::Up, Dir::Left),
            ],
            pauses: &[],
            ends_on: None,
        }),
        says: "wall = ded",
    },
    Step {
        show: Show::Play(Scene {
            start: facing,
            moves: &[(Dir::Up, Dir::Down), (Dir::Up, Dir::Down)],
            // One move out, sizes up: their length and yours.
            pauses: &[Pause {
                after: 1,
                rings: &[(2, 0), (2, 4)],
                notes: &[
                    ((3.15, 0.3), ["smaller", "(cooked)"]),
                    ((3.15, 4.3), ["bigger", "(chillin)"]),
                ],
                ms: 2_600.0,
            }],
            ends_on: Some((2, 2)),
        }),
        says: "head to head: smaller = ded, same size = both ded",
    },
    Step {
        // You box it in against the wall: once it's in the pocket, every
        // way out is into you.
        show: Show::Play(Scene {
            start: pocket,
            moves: &[
                (Dir::Up, Dir::Left),
                (Dir::Right, Dir::Left),
                (Dir::Up, Dir::Down),
                (Dir::Up, Dir::Down),
                (Dir::Up, Dir::Down),
            ],
            pauses: &[],
            ends_on: Some((0, 3)),
        }),
        says: "into a body = ded (yours too)",
    },
    Step {
        show: Show::Fun,
        says: "most fun wins",
    },
];

/// One frame of a moving step: a turn, cells to ring, notes, and how long.
struct Frame {
    turn: Turn,
    rings: Vec<(u16, u16)>,
    notes: &'static [Note],
    ms: f64,
}

/// A scene's turns, from its start, as the engine plays them. The food is
/// put back where the script wants it after every move (the engine would
/// otherwise scatter more).
fn play(scene: &Scene) -> Vec<Turn> {
    let mut engine = Engine::new(config("🍎"), vec![YOU.to_owned(), THEM.to_owned()], 1);
    let mut turns = vec![(scene.start)()];
    for &(you, them) in scene.moves {
        let Some(prev) = turns.last() else { break };
        let food_left: Vec<Pos> = prev.food.clone();
        let moves: BTreeMap<PlayerId, Dir> =
            BTreeMap::from([(YOU.to_owned(), you), (THEM.to_owned(), them)]);
        let mut next = engine.next_turn(prev, &moves);
        // Whatever was eaten this move is gone; nothing new appears.
        next.food = food_left
            .into_iter()
            .filter(|f| next.snakes.values().all(|s| s.first() != Some(f)))
            .collect();
        turns.push(next);
    }
    turns
}

/// A scene's frames, start to end, with its holds.
fn frames(scene: &Scene) -> Vec<Frame> {
    let turns = play(scene);
    let last = turns.len() - 1;
    let mut out = Vec::new();
    for (i, turn) in turns.into_iter().enumerate() {
        for p in scene.pauses.iter().filter(|p| p.after == i) {
            out.push(Frame {
                turn: turn.clone(),
                rings: p.rings.to_vec(),
                notes: p.notes,
                ms: p.ms,
            });
        }
        let (rings, ms) = if i == last {
            (scene.ends_on.into_iter().collect(), END_MS)
        } else {
            (Vec::new(), MOVE_MS)
        };
        out.push(Frame {
            turn,
            rings,
            notes: &[],
            ms,
        });
    }
    out
}

/// A board's SVG with red rings round some cells and red notes on it. The board's view starts at the field's top left corner, so a
/// field cell (x, y) sits at (x + 1, y + 1).
fn marked(svg: &str, rings: &[(u16, u16)], notes: &[Note]) -> String {
    use std::fmt::Write as _;
    let mut extra = String::new();
    for &(x, y) in rings {
        let _ = write!(
            extra,
            r##"<rect x="{}" y="{}" width="1" height="1" fill="none" stroke="#e00" stroke-width="0.1" rx="0.15"/>"##,
            x + 1,
            y + 1
        );
    }
    for &((x, y), [first, second]) in notes {
        let _ = write!(
            extra,
            r##"<text x="{tx}" y="{y1}" font-size="0.42" font-weight="bold" font-family="snek mono, monospace" fill="#e00" stroke="#fff" stroke-width="0.1" paint-order="stroke">{first}</text><text x="{tx}" y="{y2}" font-size="0.42" font-weight="bold" font-family="snek mono, monospace" fill="#e00" stroke="#fff" stroke-width="0.1" paint-order="stroke">{second}</text>"##,
            tx = x + 1.0,
            y1 = y + 1.0,
            y2 = y + 1.45,
        );
    }
    svg.replacen("</svg>", &format!("{extra}</svg>"), 1)
}

/// The step's picture: a looping board, a still board, the arrow keys, or
/// nothing but a party. Always the same square, so paging never moves the
/// buttons.
pub(crate) fn stage(step: &Step, me: &PlayerProfile, food: &str) -> AnyView {
    let store = Store::use_store();
    let players = [
        PlayerProfile {
            id: YOU.to_owned(),
            ..me.clone()
        },
        PlayerProfile {
            id: THEM.to_owned(),
            name: "them".to_owned(),
            emoji: "🦄".to_owned(),
            colour: "hsl(200, 80%, 65%)".to_owned(),
        },
    ];
    let config = config(food);
    let draw = |turn: &Turn, rings: &[(u16, u16)], notes: &[Note]| {
        marked(
            &board_svg(&config, &players, turn, YOU, false, None),
            rings,
            notes,
        )
    };
    match &step.show {
        Show::Steer => view! {
            <div class="demo-stage steer">
                <div class="dpad dpad-mini">
                    <span class="key d-up"><span class="tri"></span></span>
                    <span class="key d-left"><span class="tri"></span></span>
                    <span class="key d-right"><span class="tri"></span></span>
                    <span class="key d-down"><span class="tri"></span></span>
                </div>
            </div>
        }
        .into_any(),
        Show::Fun => view! { <div class="demo-stage fun">"🥳"</div> }.into_any(),
        Show::Still(ring, notes) => {
            let svg = draw(&start(), &[*ring], notes);
            view! { <div class="demo-stage" inner_html=svg></div> }.into_any()
        }
        Show::Play(scene) => {
            let frames = frames(scene);
            let loop_ms: f64 = frames.iter().map(|f| f.ms).sum();
            let durations: Vec<f64> = frames.iter().map(|f| f.ms).collect();
            // Every frame drawn once, up front.
            let drawn: Vec<String> = frames
                .iter()
                .map(|f| draw(&f.turn, &f.rings, f.notes))
                .collect();
            // Which frame it is, as a memo: the board is redrawn when the
            // frame changes, not on every tick of the clock. (The shared
            // clock can lag the step's start by a tick, hence the floor.)
            let at_frame = Memo::new(move |_| {
                let since = store.now_ms.get() - STARTED.with(Cell::get);
                let mut t = since.max(0.0) % loop_ms;
                durations
                    .iter()
                    .position(|ms| {
                        t -= ms;
                        t < 0.0
                    })
                    .unwrap_or(0)
            });
            let drawn = StoredValue::new(drawn);
            view! {
                <div
                    class="demo-stage"
                    inner_html=move || drawn.with_value(|d| d[at_frame.get()].clone())
                ></div>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snek_shared::ClashReason;

    /// Every death step ends as it says: the other snek dead for the reason
    /// given, you alive, nobody dead before the end.
    #[test]
    fn every_death_ends_as_it_says() {
        let expect = [
            (4, ClashReason::Body),
            (5, ClashReason::Wall),
            (6, ClashReason::HeadOn),
            (7, ClashReason::Body),
        ];
        for (step, reason) in expect {
            let Show::Play(scene) = &STEPS[step].show else {
                panic!("step {step} doesn't play");
            };
            let turns = play(scene);
            let last = turns.last().unwrap();
            assert_eq!(last.alive, vec![YOU.to_owned()], "{}", STEPS[step].says);
            assert!(
                last.clashes
                    .iter()
                    .all(|c| c.reason == reason && c.players[0] == THEM),
                "{}: {:?}",
                STEPS[step].says,
                last.clashes
            );
            for t in &turns[..turns.len() - 1] {
                assert_eq!(t.alive.len(), 2, "{}: someone died early", STEPS[step].says);
            }
        }
    }

    /// Eating fills you up, and leaves the butt stacked for the next step's
    /// crash.
    #[test]
    fn eating_stacks_the_butt() {
        let Show::Play(scene) = &STEPS[3].show else {
            panic!("the eating step doesn't play");
        };
        let turns = play(scene);
        let last = turns.last().unwrap();
        assert_eq!(last.health[YOU], 100, "didn't eat");
        let you = &last.snakes[YOU];
        assert_eq!(you.last(), Some(&at(2, 4)));
        assert_eq!(you[you.len() - 2], at(2, 4), "the butt isn't stacked");
    }
}
