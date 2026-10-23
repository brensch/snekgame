//! Highlights: a finished game, played back so the good bits get their
//! time. Two parts, both plain functions of the turns so they can be tested
//! without a screen:
//!
//! - [`tense_moments`] finds the few turns most worth watching: every
//!   death, then the best of the near misses (heads closing in) and tight
//!   spots (a snek down to its last way out), about one per twenty turns.
//! - [`plan`] turns a game into a timeline of which turn to show for how
//!   long: a quick rewind to the start, then play that races through the
//!   dull stretches and eases down to watching speed around each tense
//!   moment, holding on the end before it all goes round again.

use crate::types::{GameConfig, Pos, Turn};
use std::collections::HashSet;

/// Why a turn is worth slowing down for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tension {
    /// Two heads closing in, near enough to meet next move.
    Close,
    /// A snek that just went down to its last way out.
    Tight,
    /// Someone dies on the next turn, or just did.
    Death,
}

/// How many highlights besides the deaths a game of `turns` turns gets:
/// about one per forty turns, and at least two.
fn budget(turns: usize) -> usize {
    (turns / 40).max(2)
}

/// Highlights this close together would make one long slow stretch.
const SPACING: usize = 12;

/// The few turns of a game most worth watching, by index into `turns`, in
/// order: every death, and the best of the near misses and tight spots up
/// to the game's budget, never two close together.
#[must_use]
pub fn tense_moments(config: &GameConfig, turns: &[Turn]) -> Vec<(usize, Tension)> {
    let mut scored: Vec<(u32, usize, Tension)> = turns
        .iter()
        .enumerate()
        .filter_map(|(i, turn)| {
            let prev = i.checked_sub(1).and_then(|p| turns.get(p));
            let death_next = turns
                .get(i + 1)
                .is_some_and(|next| !next.clashes.is_empty());
            if death_next || !turn.clashes.is_empty() {
                return Some((100, i, Tension::Death));
            }
            if let Some(closeness) = prev.and_then(|prev| heads_closing(config, prev, turn)) {
                return Some((30 + closeness, i, Tension::Close));
            }
            prev.filter(|prev| newly_cornered(config, prev, turn))
                .map(|_| (20, i, Tension::Tight))
        })
        .collect();
    // Best first (earlier first among equals); deaths always make it.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<(usize, Tension)> = Vec::new();
    let limit = budget(turns.len());
    let mut highlights = 0;
    for (_, i, tension) in scored {
        if tension == Tension::Death {
            picked.push((i, tension));
            continue;
        }
        if highlights >= limit || picked.iter().any(|&(p, _)| p.abs_diff(i) < SPACING) {
            continue;
        }
        picked.push((i, tension));
        highlights += 1;
    }
    picked.sort_unstable_by_key(|&(i, _)| i);
    picked
}

fn xy(config: &GameConfig, pos: Pos) -> (i32, i32) {
    let w = u16::from(config.board_width);
    (i32::from(pos % w), i32::from(pos / w))
}

fn heads<'a>(config: &GameConfig, turn: &'a Turn) -> Vec<(&'a str, (i32, i32))> {
    turn.alive
        .iter()
        .filter_map(|p| Some((p.as_str(), xy(config, *turn.snakes.get(p)?.first()?))))
        .collect()
}

const fn apart(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs() + (a.1 - b.1).abs()
}

/// Two heads that closed in this turn to two steps apart with a free
/// cell between them: next move, both could take it and meet head-on.
/// Sneks cruising side by side don't count (they aren't getting closer),
/// nor do heads already touching (no cell both can reach). As a score:
/// 10, plus 10 when they're the same length (a head-on kills both), plus
/// 5 when both moved in (each was going for it).
fn heads_closing(config: &GameConfig, prev: &Turn, turn: &Turn) -> Option<u32> {
    let before = heads(config, prev);
    let now = heads(config, turn);
    let was = |a: &str, b: &str| {
        let find = |p: &str| before.iter().find(|(q, _)| *q == p).map(|(_, h)| *h);
        Some(apart(find(a)?, find(b)?))
    };
    let len = |p: &str| turn.snakes.get(p).map_or(0, Vec::len);
    let free = open_cells(config, turn);
    now.iter()
        .enumerate()
        .flat_map(|(i, a)| now[i + 1..].iter().map(move |b| (a, b)))
        .filter_map(|(&(pa, ha), &(pb, hb))| {
            let was = was(pa, pb)?;
            let between = neighbours(ha)
                .into_iter()
                .any(|c| apart(c, hb) == 1 && free(c));
            (apart(ha, hb) == 2 && was > 2 && between).then(|| {
                10 + if len(pa) == len(pb) { 10 } else { 0 } + if was >= 4 { 5 } else { 0 }
            })
        })
        .max()
}

const fn neighbours((x, y): (i32, i32)) -> [(i32, i32); 4] {
    [(x, y - 1), (x, y + 1), (x - 1, y), (x + 1, y)]
}

/// Whether a cell can be moved into: not a wall (the border cells), not a
/// body (tails are about to move on, so they're free unless the snek just
/// ate).
fn open_cells(config: &GameConfig, turn: &Turn) -> impl Fn((i32, i32)) -> bool {
    let mut blocked: HashSet<Pos> = HashSet::new();
    for snake in turn.snakes.values() {
        let stacked = snake.len() >= 2 && snake[snake.len() - 1] == snake[snake.len() - 2];
        let keep = if stacked {
            snake.len()
        } else {
            snake.len().saturating_sub(1)
        };
        blocked.extend(snake.iter().take(keep));
    }
    let w = i32::from(config.board_width);
    let h = i32::from(config.board_height);
    move |(x, y)| {
        x > 0
            && y > 0
            && x < w - 1
            && y < h - 1
            && !blocked.contains(&u16::try_from(y * w + x).unwrap_or(u16::MAX))
    }
}

/// How many ways out each living snek's head has.
fn ways_out(config: &GameConfig, turn: &Turn) -> Vec<(String, usize)> {
    let free = open_cells(config, turn);
    turn.alive
        .iter()
        .filter_map(|p| {
            let &head = turn.snakes.get(p)?.first()?;
            let open = neighbours(xy(config, head))
                .into_iter()
                .filter(|&c| free(c))
                .count();
            Some((p.clone(), open))
        })
        .collect()
}

/// A snek that had a choice last turn and is down to one way out (or
/// none) now.
fn newly_cornered(config: &GameConfig, prev: &Turn, turn: &Turn) -> bool {
    let before = ways_out(config, prev);
    ways_out(config, turn)
        .iter()
        .any(|(p, open)| *open <= 1 && before.iter().any(|(q, was)| q == p && *was >= 2))
}

/// How long a turn shows racing through a dull stretch.
pub const FAST_MS: f64 = 40.0;
/// How long a turn shows at the slowest, around a tense one.
pub const SLOW_MS: f64 = 420.0;
/// The rewind to the start, however long the game.
pub const REWIND_MS: f64 = 500.0;
/// The hold on the final turn, how it ended, before going round again.
pub const END_HOLD_MS: f64 = 3_000.0;
/// How many turns either side of a tense one are slowed, easing in and
/// out: two before, one after.
const LEAD_IN: usize = 2;
const LEAD_OUT: usize = 1;

/// One step of the timeline: the turn to show and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    /// Which turn, by index.
    pub turn: usize,
    /// How long it shows.
    pub ms: f64,
}

/// The highlights timeline for a game with `turns` turns (indices
/// `0..turns`).
///
/// The rewind from the last to the first, then each turn in order at its
/// speed, then the hold on the end. Played on a loop, it rewinds again
/// straight after the hold.
#[must_use]
pub fn plan(turns: usize, tense: &[(usize, Tension)]) -> Vec<Beat> {
    let Some(last) = turns.checked_sub(1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // The rewind: every turn on the way down, shared out over REWIND_MS
    // (a frame each at most; long games skip turns, like a fast drag).
    let steps = last.min(30);
    if steps > 0 {
        let each = REWIND_MS / f64::from(u32::try_from(steps).unwrap_or(30));
        for k in 0..steps {
            let turn = last - (last * k) / steps;
            out.push(Beat { turn, ms: each });
        }
    }
    // The play: fast, but slow around anything tense.
    let slowness = |i: usize| -> f64 {
        tense
            .iter()
            .map(|&(t, _)| {
                if i + LEAD_IN < t || i > t + LEAD_OUT {
                    0.0
                } else if i == t || i + 1 == t {
                    1.0
                } else if i < t {
                    // Easing in: 1/3, 2/3 of the way down to slow.
                    f64::from(u32::try_from(LEAD_IN + 1 - (t - i)).unwrap_or(0))
                        / f64::from(u32::try_from(LEAD_IN).unwrap_or(1))
                } else {
                    // Easing out after.
                    1.0 - f64::from(u32::try_from(i - t).unwrap_or(0))
                        / f64::from(u32::try_from(LEAD_OUT + 1).unwrap_or(1))
                }
            })
            .fold(0.0, f64::max)
    };
    for i in 0..last {
        out.push(Beat {
            turn: i,
            ms: (SLOW_MS - FAST_MS).mul_add(slowness(i), FAST_MS),
        });
    }
    out.push(Beat {
        turn: last,
        ms: END_HOLD_MS,
    });
    out
}

/// Where in the timeline playing from turn `turn` begins: its first beat
/// after the rewind.
#[must_use]
pub fn start_of(plan: &[Beat], turn: usize) -> f64 {
    let mut t = 0.0;
    let mut rewinding = true;
    for (i, beat) in plan.iter().enumerate() {
        // The rewind ends where the turns stop going down.
        if rewinding && plan.get(i + 1).is_some_and(|next| next.turn > beat.turn) {
            rewinding = false;
            t += beat.ms;
            continue;
        }
        if !rewinding && beat.turn >= turn {
            return t;
        }
        t += beat.ms;
    }
    t
}

/// The beat showing `ms` into the timeline (which loops).
#[must_use]
pub fn beat_at(plan: &[Beat], ms: f64) -> Option<Beat> {
    let total: f64 = plan.iter().map(|b| b.ms).sum();
    if total <= 0.0 {
        return plan.first().copied();
    }
    let mut t = ms.rem_euclid(total);
    plan.iter()
        .find(|b| {
            t -= b.ms;
            t < 0.0
        })
        .or_else(|| plan.last())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::types::Dir;
    use std::collections::BTreeMap;

    /// A real game: two sneks heading at each other until one dies.
    fn head_on_game() -> (GameConfig, Vec<Turn>) {
        let config = GameConfig::default();
        let mut engine = Engine::new(config.clone(), vec!["a".into(), "b".into()], 3);
        let mut turns = vec![engine.first_turn()];
        for _ in 0..200 {
            let last = turns.last().unwrap();
            if last.game_over() {
                break;
            }
            let moves = BTreeMap::from([("a".to_owned(), Dir::Right), ("b".to_owned(), Dir::Left)]);
            turns.push(engine.next_turn(last, &moves));
        }
        (config, turns)
    }

    #[test]
    fn a_death_is_tense_and_so_is_the_turn_before() {
        let (config, turns) = head_on_game();
        let moments = tense_moments(&config, &turns);
        let last = turns.len() - 1;
        assert!(moments.contains(&(last, Tension::Death)), "{moments:?}");
        assert!(moments.contains(&(last - 1, Tension::Death)), "{moments:?}");
        // Not everything is tense.
        assert!(moments.len() < turns.len(), "{moments:?}");
    }

    fn turn(config: &GameConfig, snakes: &[(&str, &[(u16, u16)])]) -> Turn {
        let w = u16::from(config.board_width);
        Turn {
            turn_number: 5,
            alive: snakes.iter().map(|(p, _)| (*p).to_owned()).collect(),
            health: snakes.iter().map(|(p, _)| ((*p).to_owned(), 90)).collect(),
            snakes: snakes
                .iter()
                .map(|(p, s)| ((*p).to_owned(), s.iter().map(|&(x, y)| y * w + x).collect()))
                .collect(),
            food: Vec::new(),
            moves: BTreeMap::new(),
            clashes: Vec::new(),
            winners: Vec::new(),
        }
    }

    #[test]
    fn heads_closing_in_count_but_cruising_side_by_side_doesnt() {
        let config = GameConfig::default();
        let far = turn(
            &config,
            &[("a", &[(2, 5), (1, 5)]), ("b", &[(6, 5), (7, 5)])],
        );
        let near = turn(
            &config,
            &[("a", &[(3, 5), (2, 5)]), ("b", &[(5, 5), (6, 5)])],
        );
        // Same length, both moved in: as tense as it gets.
        assert_eq!(heads_closing(&config, &far, &near), Some(25));
        // Only one moved in, and they differ in length: still a moment.
        let longer = turn(
            &config,
            &[("a", &[(2, 5), (1, 5)]), ("b", &[(5, 5), (6, 5), (7, 5)])],
        );
        let met = turn(
            &config,
            &[("a", &[(3, 5), (2, 5)]), ("b", &[(5, 5), (6, 5), (7, 5)])],
        );
        assert_eq!(heads_closing(&config, &longer, &met), Some(10));
        // Touching heads have no cell to meet in.
        let touching = turn(
            &config,
            &[("a", &[(4, 5), (3, 5)]), ("b", &[(5, 5), (6, 5)])],
        );
        assert_eq!(heads_closing(&config, &far, &touching), None);
        // Side by side, both going up, two apart: no closer than before.
        let side = turn(
            &config,
            &[("a", &[(3, 5), (3, 6)]), ("b", &[(5, 5), (5, 6)])],
        );
        let still = turn(
            &config,
            &[("a", &[(3, 4), (3, 5)]), ("b", &[(5, 4), (5, 5)])],
        );
        assert_eq!(heads_closing(&config, &side, &still), None);
    }

    #[test]
    fn being_boxed_in_counts_once_not_every_turn() {
        let config = GameConfig::default();
        let free = turn(
            &config,
            &[("a", &[(1, 3), (1, 4), (1, 5)]), ("b", &[(8, 8), (9, 8)])],
        );
        // In the top-left corner, heading up the left wall, its own body
        // below: right is the only way out.
        let cornered = turn(
            &config,
            &[("a", &[(1, 1), (1, 2), (1, 3)]), ("b", &[(8, 8), (9, 8)])],
        );
        assert!(newly_cornered(&config, &free, &cornered));
        assert!(!newly_cornered(&config, &cornered, &cornered));
    }

    /// A real game (122 turns against the hard bot, where the old rules
    /// slowed nearly all of it): the highlights stay a highlight reel.
    #[test]
    fn a_real_game_is_mostly_fast() {
        let record: crate::GameRecord =
            postcard::from_bytes(include_bytes!("../tests/fixtures/game-122-turns.bin")).unwrap();
        let moments = tense_moments(&record.config, &record.turns);
        let last = record.turns.len() - 1;
        assert!(
            moments
                .iter()
                .any(|&(i, t)| i == last && t == Tension::Death)
        );
        assert!(
            moments.len() <= budget(record.turns.len()) + 2,
            "{moments:?}"
        );
        let timeline = plan(record.turns.len(), &moments);
        let slow = timeline.iter().filter(|b| b.ms > FAST_MS).count();
        assert!(
            slow * 3 < record.turns.len(),
            "{slow} of {} slowed",
            record.turns.len()
        );
    }

    #[test]
    fn the_plan_rewinds_races_and_slows_for_the_good_bits() {
        let plan = plan(100, &[(50, Tension::Close)]);
        // Rewind: from the end, downwards, half a second in all.
        assert_eq!(plan[0].turn, 99);
        let rewind: Vec<&Beat> = plan
            .iter()
            .take_while(|b| b.turn > 0 && b.ms < FAST_MS)
            .collect();
        let rewind_ms: f64 = plan.iter().take(30).map(|b| b.ms).sum();
        assert!((rewind_ms - REWIND_MS).abs() < 1.0, "{rewind_ms}");
        assert!(rewind.windows(2).all(|w| w[1].turn < w[0].turn));
        // Play: fast far from the moment, slow at it, easing either side.
        let racing = &plan[30..];
        let ms = |turn: usize| racing.iter().find(|b| b.turn == turn).unwrap().ms;
        assert!((ms(10) - FAST_MS).abs() < f64::EPSILON);
        assert!((ms(50) - SLOW_MS).abs() < f64::EPSILON);
        assert!((ms(49) - SLOW_MS).abs() < f64::EPSILON);
        assert!(ms(47) < ms(48) && ms(48) < ms(49), "easing in");
        assert!(ms(50) > ms(51) && ms(51) > ms(52), "easing out");
        assert!((ms(47) - FAST_MS).abs() < f64::EPSILON);
        assert!((ms(52) - FAST_MS).abs() < f64::EPSILON);
        // And a hold on the end.
        assert_eq!(
            *plan.last().unwrap(),
            Beat {
                turn: 99,
                ms: END_HOLD_MS
            }
        );
    }

    #[test]
    fn the_timeline_can_be_entered_anywhere_and_loops() {
        let timeline = plan(10, &[]);
        let start = start_of(&timeline, 4);
        assert_eq!(beat_at(&timeline, start).unwrap().turn, 4);
        assert_eq!(
            beat_at(&timeline, 0.0).unwrap().turn,
            9,
            "starts with the rewind"
        );
        let total: f64 = timeline.iter().map(|b| b.ms).sum();
        assert_eq!(
            beat_at(&timeline, total + 1.0).unwrap().turn,
            9,
            "and goes round again"
        );
        assert_eq!(plan(0, &[]), Vec::<Beat>::new());
        assert_eq!(
            plan(1, &[]),
            vec![Beat {
                turn: 0,
                ms: END_HOLD_MS
            }]
        );
    }
}
