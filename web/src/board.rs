//! The board, drawn as SVG in cell units so everything scales crisply with
//! zero gaps. The perimeter walls exist in the rules but are not drawn — the
//! viewBox crops to the playable interior, so the screen edge IS the wall.
//! - checkerboard field, full-bleed and square to maximise size on mobile
//! - sneks as one continuous rounded path with the player's emoji as head,
//!   health on the neck, stacked count on the tail (tac logic)
//! - food is whatever the lobby picked; deaths leave 💀 trails but keep the
//!   dead player's emoji head
use leptos::prelude::*;
use snek_shared::{GameConfig, PlayerId, PlayerProfile, Pos, Turn};
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;

const CHECKER_A: &str = "#dcedc8";
const CHECKER_B: &str = "#cfe6b8";

// Vertical centering uses dy="0.36em" instead of dominant-baseline, which
// WebKit ignores on SVG <text> — dy is honoured identically everywhere.
fn emoji_text(x: f64, y: f64, size: f64, glyph: &str) -> String {
    format!(
        r##"<text x="{x}" y="{y}" dy="0.36em" font-size="{size}" text-anchor="middle">{glyph}</text>"##
    )
}

fn number_text(x: f64, y: f64, value: &str) -> String {
    format!(
        r##"<text x="{x}" y="{y}" dy="0.36em" font-size="0.44" font-weight="bold" font-family="snek mono, monospace" fill="#000" text-anchor="middle">{value}</text>"##
    )
}

pub fn board_view(
    config: GameConfig,
    players: Vec<PlayerProfile>,
    turn: &Turn,
    my_id: PlayerId,
    // Before the first tick: grey out everything but my snek and put a red
    // box and a "you!" over it, so nobody spends the start looking for
    // themselves.
    spotlight: bool,
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
            let fill = if (x + y) % 2 == 0 {
                CHECKER_A
            } else {
                CHECKER_B
            };
            field.push_str(&format!(
                r##"<rect x="{x}" y="{y}" width="1" height="1" fill="{fill}"/>"##
            ));
        }
    }

    // --- food stays emoji, like tac --------------------------------------
    let mut food = String::new();
    for &pos in &turn.food {
        food.push_str(&emoji_text(cx(pos), cy(pos), 0.8, &config.food_emoji));
    }

    // --- sneks ------------------------------------------------------------
    let mut sneks = String::new();
    let mut mine = String::new();
    for (player_id, snake) in &turn.snakes {
        let spotlit = spotlight && *player_id == my_id;
        let sneks = if spotlit { &mut mine } else { &mut sneks };
        let colour = colour_of.get(player_id.as_str()).copied().unwrap_or("#999");

        // Collapse consecutive duplicates (stacked start / fresh growth).
        let mut path_cells: Vec<Pos> = Vec::with_capacity(snake.len());
        for &p in snake.iter() {
            if path_cells.last() != Some(&p) {
                path_cells.push(p);
            }
        }

        // Body: one rounded polyline through the centres, drawn through the
        // head cell too so it blends seamlessly into the head square.
        let head = snake[0];
        if path_cells.len() > 1 {
            let points: String = path_cells
                .iter()
                .map(|&p| format!("{},{}", cx(p), cy(p)))
                .collect::<Vec<_>>()
                .join(" ");
            sneks.push_str(&format!(
                r##"<polyline points="{points}" fill="none" stroke="{colour}" stroke-width="0.72" stroke-linecap="round" stroke-linejoin="round"/>"##
            ));
            // Tail taper: a slightly smaller round cap on the last cell.
            let t = *path_cells.last().unwrap();
            sneks.push_str(&format!(
                r##"<circle cx="{}" cy="{}" r="0.30" fill="{colour}"/>"##,
                cx(t),
                cy(t)
            ));
        }

        // The head: a full body-coloured square with the player's emoji.
        sneks.push_str(&format!(
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="{colour}"/>"##,
            x = head as usize % w,
            y = head as usize / w,
        ));
        let emoji = emoji_of.get(player_id.as_str()).copied().unwrap_or("🐍");
        sneks.push_str(&emoji_text(cx(head), cy(head), 0.8, emoji));

        // Numbers as the snek unfurls: length on the first body tile,
        // health on the one after.
        if let Some(&second) = path_cells.get(1) {
            sneks.push_str(&number_text(
                cx(second),
                cy(second),
                &snake.len().to_string(),
            ));
        }
        if let Some(&third) = path_cells.get(2) {
            let hp = turn.health.get(player_id).copied().unwrap_or(0);
            sneks.push_str(&number_text(cx(third), cy(third), &hp.to_string()));
        }
        // Stacked-segment count on the butt (growth queued up).
        let tail_cell = *snake.last().unwrap();
        let stacked = snake.iter().filter(|&&p| p == tail_cell).count();
        if stacked > 1
            && tail_cell != head
            && path_cells.get(1) != Some(&tail_cell)
            && path_cells.get(2) != Some(&tail_cell)
        {
            sneks.push_str(&number_text(
                cx(tail_cell),
                cy(tail_cell),
                &stacked.to_string(),
            ));
        }
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

    // The spotlight: a white veil over everything drawn so far, my snek on
    // top of it, a red box around my head and "you!" over it (under it if
    // the head is on the top row and there's no room above).
    let mut spot = String::new();
    if let Some(head) = turn.snakes.get(&my_id).filter(|_| spotlight).map(|s| s[0]) {
        let (x, y) = ((head as usize % w) as f64, (head as usize / w) as f64);
        let label_y = if y <= 1.0 { y + 1.75 } else { y - 0.55 };
        spot = format!(
            r##"<rect x="0" y="0" width="{w}" height="{h}" fill="#fff" opacity="0.72"/>{mine}<rect x="{bx}" y="{by}" width="1.36" height="1.36" fill="none" stroke="#e00" stroke-width="0.14"/><text x="{tx}" y="{label_y}" font-size="0.8" font-weight="bold" font-family="snek mono, monospace" fill="#e00" stroke="#fff" stroke-width="0.12" paint-order="stroke" text-anchor="middle">you!</text>"##,
            bx = x - 0.18,
            by = y - 0.18,
            tx = x + 0.5,
        );
    } else {
        sneks.push_str(&mine);
    }

    // Crop the view to the playable interior — the walls sit just off-screen.
    let svg = format!(
        r##"<svg viewBox="1 1 {vw} {vh}" xmlns="http://www.w3.org/2000/svg" style="display:block;width:100%;height:100%;">
            {field}{food}{sneks}{clashes}{spot}
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
                // The svg keeps the board square inside whatever box it is
                // given, centring it and leaving a margin on one axis. Taps
                // have to be measured against the board, not the box, or
                // every column is off by half the margin — and a tap in the
                // margin is not a tap on the board at all.
                let board_aspect = vis_w / vis_h;
                let (bw, bh) = if rect.width() / rect.height() > board_aspect {
                    (rect.height() * board_aspect, rect.height())
                } else {
                    (rect.width(), rect.width() / board_aspect)
                };
                let left = rect.left() + (rect.width() - bw) / 2.0;
                let top = rect.top() + (rect.height() - bh) / 2.0;
                let fx = (ev.client_x() - left) / bw;
                let fy = (ev.client_y() - top) / bh;
                if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
                    return; // in the margin beside the board
                }
                let x = 1 + (fx * vis_w) as i32;
                let y = 1 + (fy * vis_h) as i32;
                if x >= 1 && y >= 1 && (x as usize) < w - 1 && (y as usize) < h - 1 {
                    on_cell((y as usize * w + x as usize) as Pos);
                }
            }
            inner_html=svg
        ></div>
    }
}
