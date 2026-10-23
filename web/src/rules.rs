//! The rules popup: one rule a line, each with a picture. The ways to die
//! are drawn as tiny boards in the game's own look, so a rule reads the way
//! the moment will look in play.

use crate::board::{CHECKER_A, CHECKER_B, number_text};
use crate::store::Store;
use leptos::prelude::*;
use std::fmt::Write as _;

/// The sneks in the pictures: whoever dies is red, everyone else green
/// (and wearing the viewer's emoji).
const DEAD: &str = "hsl(0, 80%, 62%)";
const ALIVE: &str = "hsl(120, 55%, 55%)";

/// A snek in a picture: its colour, its cells head first, its head's emoji,
/// and any numbers on its body, as (index into `cells`, text).
struct Snek<'a> {
    colour: &'a str,
    cells: &'a [(u8, u8)],
    head: &'a str,
    numbers: &'a [(usize, &'a str)],
}

/// A tiny board, `w` by `h` cells: the checkerboard, optionally a wall
/// down the right edge, the sneks, then any loose glyphs (food, skulls) on
/// top.
fn picture(w: u8, h: u8, wall: bool, sneks: &[Snek<'_>], glyphs: &[(u8, u8, &str)]) -> String {
    let mut svg = String::new();
    for y in 0..h {
        for x in 0..w {
            let fill = if (x + y) % 2 == 0 {
                CHECKER_A
            } else {
                CHECKER_B
            };
            let _ = write!(
                svg,
                r#"<rect x="{x}" y="{y}" width="1" height="1" fill="{fill}"/>"#
            );
        }
    }
    if wall {
        let _ = write!(
            svg,
            r##"<rect x="{}" y="0" width="0.22" height="{h}" fill="#000"/>"##,
            f32::from(w) - 0.22
        );
    }
    for snek in sneks {
        let points = snek
            .cells
            .iter()
            .map(|&(x, y)| format!("{x}.5,{y}.5"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = write!(
            svg,
            r#"<polyline points="{points}" fill="none" stroke="{}" stroke-width="0.72" stroke-linecap="round" stroke-linejoin="round"/>"#,
            snek.colour
        );
        if let Some(&(x, y)) = snek.cells.first() {
            let _ = write!(
                svg,
                r#"<rect x="{x}" y="{y}" width="1" height="1" fill="{}"/>{}"#,
                snek.colour,
                glyph(x, y, snek.head)
            );
        }
        for &(i, text) in snek.numbers {
            if let Some(&(x, y)) = snek.cells.get(i) {
                svg.push_str(&number_text(f64::from(x) + 0.5, f64::from(y) + 0.5, text));
            }
        }
    }
    for &(x, y, g) in glyphs {
        svg.push_str(&glyph(x, y, g));
    }
    format!(r#"<svg viewBox="0 0 {w} {h}" xmlns="http://www.w3.org/2000/svg">{svg}</svg>"#)
}

/// An emoji centred in a cell (dy rather than dominant-baseline, which
/// `WebKit` ignores on SVG text).
fn glyph(x: u8, y: u8, g: &str) -> String {
    format!(
        r#"<text x="{x}.5" y="{y}.5" dy="0.36em" font-size="0.8" text-anchor="middle">{g}</text>"#
    )
}

/// One rule: a picture (an SVG, or a plain emoji) and a sentence.
fn rule(picture: Result<String, &'static str>, text: String) -> impl IntoView {
    view! {
        <li class="rule">
            {match picture {
                Ok(svg) => view! { <span class="rule-pic" inner_html=svg></span> }.into_any(),
                Err(emoji) => view! { <span class="rule-pic rule-emoji">{emoji}</span> }.into_any(),
            }}
            <span>{text}</span>
        </li>
    }
}

/// You, heading right at your food.
fn eat_picture(me: &str, food: &str) -> String {
    picture(
        4,
        2,
        false,
        &[Snek {
            colour: ALIVE,
            cells: &[(2, 1), (1, 1), (0, 1), (0, 0)],
            head: me,
            numbers: &[],
        }],
        &[(3, 1, food)],
    )
}

/// The numbers on a snek: its length first, then its health.
fn numbers_picture(me: &str) -> String {
    picture(
        4,
        2,
        false,
        &[Snek {
            colour: ALIVE,
            cells: &[(3, 0), (2, 0), (1, 0), (0, 0), (0, 1)],
            head: me,
            numbers: &[(1, "5"), (2, "87")],
        }],
        &[],
    )
}

/// You, into the wall.
fn wall_picture() -> String {
    picture(
        4,
        2,
        true,
        &[Snek {
            colour: DEAD,
            cells: &[(3, 1), (2, 1), (1, 1), (1, 0)],
            head: "💀",
            numbers: &[],
        }],
        &[],
    )
}

/// Boxed in: the green snek wraps right round the red one's head, so
/// every way it can turn is into green's body.
fn body_picture(me: &str) -> String {
    picture(
        5,
        3,
        false,
        &[
            Snek {
                colour: ALIVE,
                cells: &[(2, 0), (3, 0), (4, 0), (4, 1), (4, 2), (3, 2), (2, 2)],
                head: me,
                numbers: &[],
            },
            Snek {
                colour: DEAD,
                cells: &[(3, 1), (2, 1), (1, 1), (0, 1)],
                head: "💀",
                numbers: &[],
            },
        ],
        &[],
    )
}

/// Head to head: the longer snek lives.
fn heads_picture(me: &str) -> String {
    picture(
        5,
        2,
        false,
        &[
            Snek {
                colour: ALIVE,
                cells: &[(2, 1), (1, 1), (0, 1), (0, 0), (1, 0)],
                head: me,
                numbers: &[],
            },
            Snek {
                colour: DEAD,
                cells: &[(3, 1), (4, 1)],
                head: "💀",
                numbers: &[],
            },
        ],
        &[],
    )
}

/// How to play, in a popup.
pub(crate) fn rules_popup(food: &str) -> impl IntoView + use<> {
    let store = Store::use_store();
    let close = move |_| store.popup.set(None);
    let food = food.to_owned();
    // The sneks that live are drawn with the viewer's own emoji: the
    // pictures are about you.
    let me = store.profile.get_untracked().emoji;
    let eat = eat_picture(&me, &food);
    let numbers = numbers_picture(&me);
    let wall = wall_picture();
    let body = body_picture(&me);
    let heads = heads_picture(&me);
    view! {
        <div class="modal-back" on:click=close>
            <div class="modal rules" on:click=move |ev| ev.stop_propagation()>
                <h3 style="margin-top: 0;">"rules"</h3>
                <ul class="rule-list">
                    {rule(Err("👆"), "Steer with swipes, taps, arrows or WASD.".to_owned())}
                    {rule(Ok(eat), format!("Eat {food} to grow and refill your health."))}
                    {rule(Err("💔"), "You lose 1 health every move.".to_owned())}
                    {rule(
                        Ok(numbers),
                        "Numbers behind your head: your length, then your health.".to_owned(),
                    )}
                    {rule(Ok(wall), "Hit a wall and you die.".to_owned())}
                    {rule(Ok(body), "Hit a snek's body, yours included, and you die.".to_owned())}
                    {rule(Ok(heads), "Head to head, the bigger snek wins.".to_owned())}
                    {rule(Err("🥳"), "Whoever has the most fun wins.".to_owned())}
                </ul>
                <button class="big" style="margin-top: 18px;" on:click=close>
                    "got it"
                </button>
            </div>
        </div>
    }
}
