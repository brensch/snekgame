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

pub fn board_view(
    config: GameConfig,
    players: Vec<PlayerProfile>,
    turn: &Turn,
    my_id: PlayerId,
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

    // --- food: little apples ---------------------------------------------
    let mut food = String::new();
    for &pos in &turn.food {
        let (x, y) = (cx(pos), cy(pos));
        food.push_str(&format!(
            r##"<circle cx="{x}" cy="{cyb}" r="0.30" fill="#d7263d"/>
               <circle cx="{hx}" cy="{hy}" r="0.09" fill="#ffffff" opacity="0.55"/>
               <path d="M {x} {sy} q 0.05 -0.16 0.16 -0.2" stroke="#5d4037" stroke-width="0.06" fill="none" stroke-linecap="round"/>
               <ellipse cx="{lx}" cy="{ly}" rx="0.11" ry="0.055" fill="#66bb6a" transform="rotate(-30 {lx} {ly})"/>"##,
            cyb = y + 0.04,
            hx = x - 0.10,
            hy = y - 0.06,
            sy = y - 0.24,
            lx = x + 0.14,
            ly = y - 0.30,
        ));
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

        // Body: one rounded polyline through the centres.
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

        // Head with eyes looking along the heading.
        let head = snake[0];
        let (hx, hy) = (cx(head), cy(head));
        let (dx, dy) = match path_cells.get(1) {
            Some(&neck) => {
                let ddx = cx(head) - cx(neck);
                let ddy = cy(head) - cy(neck);
                let len = (ddx * ddx + ddy * ddy).sqrt().max(0.001);
                (ddx / len, ddy / len)
            }
            None => (0.0, -1.0),
        };
        let is_me = *player_id == my_id;
        if is_me {
            sneks.push_str(&format!(
                r##"<circle cx="{hx}" cy="{hy}" r="0.52" fill="none" stroke="#ffffff" stroke-width="0.10"/>
                   <circle cx="{hx}" cy="{hy}" r="0.58" fill="none" stroke="#000000" stroke-width="0.045"/>"##
            ));
        }
        sneks.push_str(&format!(
            r##"<circle cx="{hx}" cy="{hy}" r="0.44" fill="{colour}"/>"##
        ));
        // Tongue, because snek.
        sneks.push_str(&format!(
            r##"<path d="M {sx} {sy} l {fx} {fy}" stroke="#e53935" stroke-width="0.07" stroke-linecap="round"/>"##,
            sx = hx + dx * 0.40,
            sy = hy + dy * 0.40,
            fx = dx * 0.22,
            fy = dy * 0.22,
        ));
        // The player's emoji is the face.
        let emoji = emoji_of.get(player_id.as_str()).copied().unwrap_or("🐍");
        sneks.push_str(&format!(
            r##"<text x="{hx}" y="{ty}" font-size="0.62" text-anchor="middle" dominant-baseline="central">{emoji}</text>"##,
            ty = hy + 0.03,
        ));
    }

    // --- clash starbursts -------------------------------------------------
    let mut clashes = String::new();
    for clash in &turn.clashes {
        let (x, y) = (cx(clash.pos), cy(clash.pos));
        let mut pts = String::new();
        for i in 0..16 {
            let ang = std::f64::consts::PI * 2.0 * i as f64 / 16.0;
            let r = if i % 2 == 0 { 0.46 } else { 0.20 };
            pts.push_str(&format!("{},{} ", x + ang.cos() * r, y + ang.sin() * r));
        }
        clashes.push_str(&format!(
            r##"<polygon points="{pts}" fill="#b71c1c" opacity="0.85"/>
               <circle cx="{x}" cy="{y}" r="0.10" fill="#fff176"/>"##
        ));
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
