//! The rules popup: steps to page through (see `demo`), each one a picture
//! and a line, with arrows either side and "got it" to be done.

use crate::demo::{STEPS, show, stage};
use crate::store::Store;
use leptos::prelude::*;
use snek_shared::PlayerProfile;

/// How to play, in a popup, at step `step`.
pub(crate) fn rules_popup(food: &str, step: usize) -> impl IntoView + use<> {
    let store = Store::use_store();
    let close = move |_| store.popup.set(None);
    let step = step.min(STEPS.len() - 1);
    // The snek you watch is drawn as the viewer: it's you.
    let profile = store.profile.get_untracked();
    let me = PlayerProfile {
        id: String::new(),
        name: profile.name,
        emoji: profile.emoji,
        colour: profile.colour,
    };
    let current = &STEPS[step];
    view! {
        <div class="modal-back" on:click=close>
            <div class="modal rules" on:click=move |ev| ev.stop_propagation()>
                <h3 style="margin-top: 0;">"rules"</h3>
                {stage(current, &me, food)}
                <p class="demo-caption">{current.says}</p>
                <div class="stepper">
                    <button
                        class="step-arrow"
                        aria-label="back"
                        disabled=step == 0
                        on:click=move |_| show(store, step.saturating_sub(1))
                    >
                        "‹"
                    </button>
                    <span class="step-count">{format!("{} / {}", step + 1, STEPS.len())}</span>
                    <button
                        class="step-arrow"
                        aria-label="next"
                        disabled=step + 1 == STEPS.len()
                        on:click=move |_| show(store, step + 1)
                    >
                        "›"
                    </button>
                </div>
                <button class="big" style="margin-top: 12px;" on:click=close>
                    "got it"
                </button>
            </div>
        </div>
    }
}
