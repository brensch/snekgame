//! Highlights, played: once a game is over, the end-of-game board plays it
//! back on its own (see `snek_shared::highlights` for the timeline: a quick
//! rewind, then racing through the dull bits and slowing for the tense
//! ones, holding on the end, and round again). The slider follows it.
//! Touching the slider takes over; the play button picks up from wherever
//! it was left.

use crate::net::now;
use crate::store::Store;
use gloo_timers::callback::Interval;
use leptos::prelude::*;
use snek_shared::highlights::{Beat, beat_at, plan, start_of, tense_moments};
use std::cell::RefCell;

/// A frame a little quicker than the fastest turn, so none is skipped.
const FRAME_MS: u32 = 16;

/// The highlights playing: the timeline, when (wall clock) it began, and
/// the timer moving it on (held only to keep it running).
struct Playing {
    plan: Vec<Beat>,
    began: f64,
    _timer: Interval,
}

thread_local! {
    static PLAYING: RefCell<Option<Playing>> = const { RefCell::new(None) };
}

/// Play the highlights of the game on screen, if it's over: from the
/// top (the rewind) or, given a turn, from that turn on.
pub(crate) fn start(store: Store, from: Option<usize>) {
    let game = store.game.with_untracked(|g| {
        g.as_ref()
            .filter(|g| g.turns.last().is_some_and(snek_shared::Turn::game_over))
            .map(|g| (g.config.clone(), g.turns.clone()))
    });
    let Some((config, turns)) = game else {
        return;
    };
    let plan = plan(turns.len(), &tense_moments(&config, &turns));
    if plan.is_empty() {
        return;
    }
    let into = from.map_or(0.0, |turn| start_of(&plan, turn));
    let timer = Interval::new(FRAME_MS, move || tick(store));
    PLAYING.with(|p| {
        *p.borrow_mut() = Some(Playing {
            plan,
            began: now() - into,
            _timer: timer,
        });
    });
    store.autoplay.set(true);
    tick(store);
}

/// Stop playing; whatever turn is showing stays.
pub(crate) fn stop(store: Store) {
    PLAYING.with(|p| p.borrow_mut().take());
    if store.autoplay.get_untracked() {
        store.autoplay.set(false);
    }
}

/// Show whichever turn the timeline is on. A game that's gone (back to the
/// lobby, a new one started) stops it.
fn tick(store: Store) {
    let over = store.game.with_untracked(|g| {
        g.as_ref()
            .is_some_and(|g| g.turns.last().is_some_and(snek_shared::Turn::game_over))
    });
    if !over {
        stop(store);
        return;
    }
    let turn = PLAYING.with(|p| {
        p.borrow()
            .as_ref()
            .and_then(|p| beat_at(&p.plan, now() - p.began))
            .map(|b| b.turn)
    });
    if let Some(turn) = turn
        && store.view_turn.get_untracked() != Some(turn)
    {
        store.view_turn.set(Some(turn));
    }
}
