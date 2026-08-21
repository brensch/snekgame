//! The board, drawn as SVG in cell units so everything scales crisply with
//! zero gaps. The perimeter walls exist in the rules but are not drawn — the
//! viewBox crops to the playable interior, so the screen edge IS the wall.
//! - checkerboard field, full-bleed and square to maximise size on mobile
//! - sneks as one continuous rounded path with the player's emoji as head,
//!   health on the neck, stacked count on the tail (tac logic)
//! - 🎃 food; deaths leave 💀 trails but keep the dead player's emoji head
use leptos::prelude::*;
use snek_shared::{GameConfig, PlayerId, PlayerProfile, Pos, Turn};
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;

const CHECKER_A: &str = "#dcedc8";
const CHECKER_B: &str = "#cfe6b8";

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

    // --- clashes stay emoji, like tac. The dead snek's body becomes a 💀
    // trail, but its head cell keeps the player's emoji so you can see who
    // died. The engine puts the dead player first in `players` and their
    // head first in their clash trail.
    let mut clashes = String::new();
    let mut seen_dead_heads: HashSet<&str> = HashSet::new();
    for clash in &turn.clashes {
        let owner = clash.players.first().map(String::as_str).unwrap_or("");
        let is_head = seen_dead_heads.insert(owner);
        clashes.push_str(&format!(
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#d3d3d3" opacity="0.8"/>"##,
            x = clash.pos as usize % w,
            y = clash.pos as usize / w,
        ));
        let glyph = if is_head {
            emoji_of.get(owner).copied().unwrap_or("💀")
        } else {
            "💀"
        };
        clashes.push_str(&emoji_text(cx(clash.pos), cy(clash.pos), 0.8, glyph));
    }

    // Crop the view to the playable interior — the walls sit just off-screen.
    let svg = format!(
        r##"<svg viewBox="1 1 {vw} {vh}" xmlns="http://www.w3.org/2000/svg" style="display:block;width:100%;background:{CHECKER_A};">
            {field}{food}{sneks}{clashes}
        </svg>"##,
        vw = w - 2,
        vh = h - 2,
    );

    let vis_w = (w - 2) as f64;
    let vis_h = (h - 2) as f64;
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
                let x = 1 + ((ev.client_x() as f64 - rect.left()) / rect.width() * vis_w) as i32;
                let y = 1 + ((ev.client_y() as f64 - rect.top()) / rect.height() * vis_h) as i32;
                if x >= 1 && y >= 1 && (x as usize) < w - 1 && (y as usize) < h - 1 {
                    on_cell((y as usize * w + x as usize) as Pos);
                }
            }
            inner_html=svg
        ></div>
    }
}
