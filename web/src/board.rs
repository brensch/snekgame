//! The board, drawn as SVG in cell units (viewBox = board size), so
//! everything scales crisply with zero gaps:
//! - checkerboard field
//! - brick-drawn perimeter walls
//! - sneks as one continuous rounded path through cell centres, with a
//!   proper head (eyes look where you're going); your own head gets a ring
//! - apples for food, a starburst where someone died
use leptos::prelude::*;
use snek_shared::{GameConfig, PlayerId, PlayerProfile, Pos, Turn};
use std::collections::HashMap;
use wasm_bindgen::JsCast;

const CHECKER_A: &str = "#dcedc8";
const CHECKER_B: &str = "#cfe6b8";
const BRICK: &str = "#a8503c";
const MORTAR: &str = "#7c3a2b";

fn emoji_text(x: f64, y: f64, size: f64, glyph: &str) -> String {
    format!(
        r##"<text x="{x}" y="{ty}" font-size="{size}" text-anchor="middle" dominant-baseline="central">{glyph}</text>"##,
        ty = y + 0.04,
    )
}

fn number_text(x: f64, y: f64, value: &str) -> String {
    format!(
        r##"<text x="{x}" y="{ty}" font-size="0.44" font-weight="bold" font-family="Courier New, monospace" fill="#000" text-anchor="middle" dominant-baseline="central">{value}</text>"##,
        ty = y + 0.03,
    )
}

pub fn board_view(
    config: GameConfig,
    players: Vec<PlayerProfile>,
    turn: &Turn,
    _my_id: PlayerId,
    on_cell: impl Fn(Pos) + Copy + 'static,
) -> impl IntoView {
    let w = config.board_width as usize;
    let h = config.board_height as usize;
    let colour_of: HashMap<&str, &str> = players
        .iter()
        .map(|p| (p.id.as_str(), p.colour.as_str()))
        .collect();
    let emoji_of: HashMap<&str, &str> = players
        .iter()
        .map(|p| (p.id.as_str(), p.emoji.as_str()))
        .collect();

    let cx = |pos: Pos| (pos as usize % w) as f64 + 0.5;
    let cy = |pos: Pos| (pos as usize / w) as f64 + 0.5;

    // --- checkerboard interior -------------------------------------------
    let mut field = String::new();
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let fill = if (x + y) % 2 == 0 { CHECKER_A } else { CHECKER_B };
            field.push_str(&format!(
                r##"<rect x="{x}" y="{y}" width="1" height="1" fill="{fill}"/>"##
            ));
        }
    }

    // --- brick walls ------------------------------------------------------
    let mut walls = String::new();
    for pos in config.walls() {
        let (x, y) = (pos as usize % w, pos as usize / w);
        walls.push_str(&format!(
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="{BRICK}"/>"##
        ));
        // mortar: two horizontal courses + offset headers per course
        let head_off = if (x + y) % 2 == 0 { 0.5 } else { 0.25 };
        walls.push_str(&format!(
            r##"<path d="M {x} {y1} h 1 M {x} {y2} h 1 M {hx} {y} v 0.34 M {hx2} {gy} v 0.32 M {hx3} {gy2} v 0.34" stroke="{MORTAR}" stroke-width="0.045" fill="none"/>"##,
            y1 = y as f64 + 0.34,
            y2 = y as f64 + 0.66,
            hx = x as f64 + head_off,
            hx2 = x as f64 + (1.0 - head_off),
            gy = y as f64 + 0.34,
            hx3 = x as f64 + head_off,
            gy2 = y as f64 + 0.66,
        ));
    }

    // --- food stays emoji, like tac --------------------------------------
    let mut food = String::new();
    for &pos in &turn.food {
        food.push_str(&emoji_text(cx(pos), cy(pos), 0.8, "🎃"));
    }

    // --- sneks ------------------------------------------------------------
    let mut sneks = String::new();
    for (player_id, snake) in &turn.snakes {
        let colour = colour_of.get(player_id.as_str()).copied().unwrap_or("#999");

        // Collapse consecutive duplicates (stacked start / fresh growth).
        let mut path_cells: Vec<Pos> = Vec::with_capacity(snake.len());
        for &p in snake.iter() {
            if path_cells.last() != Some(&p) {
                path_cells.push(p);
            }
        }

        // Body: one rounded polyline through the centres, starting at the
        // neck — the head cell holds nothing but the player's emoji.
        let head = snake[0];
        if path_cells.len() > 1 {
            let body = &path_cells[1..];
            if body.len() == 1 {
                sneks.push_str(&format!(
                    r##"<circle cx="{}" cy="{}" r="0.36" fill="{colour}"/>"##,
                    cx(body[0]),
                    cy(body[0])
                ));
            } else {
                let points: String = body
                    .iter()
                    .map(|&p| format!("{},{}", cx(p), cy(p)))
                    .collect::<Vec<_>>()
                    .join(" ");
                sneks.push_str(&format!(
                    r##"<polyline points="{points}" fill="none" stroke="{colour}" stroke-width="0.72" stroke-linecap="round" stroke-linejoin="round"/>"##
                ));
                // Tail taper: a slightly smaller round cap on the last cell.
                let t = *body.last().unwrap();
                sneks.push_str(&format!(
                    r##"<circle cx="{}" cy="{}" r="0.30" fill="{colour}"/>"##,
                    cx(t),
                    cy(t)
                ));
            }
        }

        // tac-style numbers: health on the second tile…
        if let Some(&second) = path_cells.get(1) {
            let hp = turn.health.get(player_id).copied().unwrap_or(0);
            sneks.push_str(&number_text(cx(second), cy(second), &hp.to_string()));
        }
        // …and the count of stacked segments on the butt (growth queued up).
        let tail_cell = *snake.last().unwrap();
        let stacked = snake.iter().filter(|&&p| p == tail_cell).count();
        if stacked > 1
            && tail_cell != head
            && path_cells.get(1) != Some(&tail_cell)
        {
            sneks.push_str(&number_text(
                cx(tail_cell),
                cy(tail_cell),
                &stacked.to_string(),
            ));
        }

        // The head: just the player's emoji.
        let emoji = emoji_of.get(player_id.as_str()).copied().unwrap_or("🐍");
        sneks.push_str(&emoji_text(cx(head), cy(head), 0.8, emoji));
    }

    // --- clashes stay emoji, like tac -------------------------------------
    let mut clashes = String::new();
    for clash in &turn.clashes {
        clashes.push_str(&format!(
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#d3d3d3" opacity="0.8"/>"##,
            x = clash.pos as usize % w,
            y = clash.pos as usize / w,
        ));
        clashes.push_str(&emoji_text(cx(clash.pos), cy(clash.pos), 0.8, "💀"));
    }

    let svg = format!(
        r##"<svg viewBox="0 0 {w} {h}" xmlns="http://www.w3.org/2000/svg" style="display:block;width:100%;background:{CHECKER_A};border:3px solid #000;">
            {field}{walls}{food}{sneks}{clashes}
        </svg>"##
    );

    let width = w as f64;
    let height = h as f64;
    view! {
        <div
            class="board-wrap"
            on:click=move |ev: web_sys::MouseEvent| {
                let Some(el) = ev
                    .current_target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else {
                    return;
                };
                let rect = el.get_bounding_client_rect();
                if rect.width() <= 0.0 || rect.height() <= 0.0 {
                    return;
                }
                let x = ((ev.client_x() as f64 - rect.left()) / rect.width() * width) as i32;
                let y = ((ev.client_y() as f64 - rect.top()) / rect.height() * height) as i32;
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    on_cell((y as usize * w + x as usize) as Pos);
                }
            }
            inner_html=svg
        ></div>
    }
}
