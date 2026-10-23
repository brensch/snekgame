//! The board, drawn as SVG in cell units so everything scales crisply with
//! zero gaps. The perimeter walls exist in the rules but are not drawn — the
//! viewBox crops to the playable interior, so the screen edge IS the wall.
//! - checkerboard field, full-bleed and square to maximise size on mobile
//! - sneks as one continuous rounded path with the player's emoji as head,
//!   health on the neck, stacked count on the tail (tac logic)
//! - food is whatever the lobby picked; the dead become grey 💀 trails
use crate::num;
use leptos::prelude::*;
use snek_shared::{GameConfig, PlayerProfile, Pos, Turn};
use std::collections::HashMap;
use std::fmt::Write as _;
use wasm_bindgen::JsCast;

/// The board's two field colours, alternating cell by cell.
pub(crate) const CHECKER_A: &str = "#dcedc8";
/// See [`CHECKER_A`].
pub(crate) const CHECKER_B: &str = "#cfe6b8";

// Vertical centering uses dy="0.36em" instead of dominant-baseline, which
// WebKit ignores on SVG <text> — dy is honoured identically everywhere.
fn emoji_text(x: f64, y: f64, size: f64, glyph: &str) -> String {
    format!(
        r#"<text x="{x}" y="{y}" dy="0.36em" font-size="{size}" text-anchor="middle">{glyph}</text>"#
    )
}

/// A number drawn on a snek's body the way the board draws length and
/// health: small, bold, centred in its cell.
pub(crate) fn number_text(x: f64, y: f64, value: &str) -> String {
    format!(
        r##"<text x="{x}" y="{y}" dy="0.36em" font-size="0.44" font-weight="bold" font-family="snek mono, monospace" fill="#000" text-anchor="middle">{value}</text>"##
    )
}

/// The board's size in cells, walls included, and the arithmetic that turns
/// a cell index into somewhere to draw.
#[derive(Clone, Copy)]
struct Grid {
    w: u16,
    h: u16,
}

impl Grid {
    /// Column of a cell's top-left corner.
    const fn x(self, pos: Pos) -> u16 {
        pos % self.w
    }

    /// Row of a cell's top-left corner.
    const fn y(self, pos: Pos) -> u16 {
        pos / self.w
    }

    /// Centre of a cell, horizontally.
    fn cx(self, pos: Pos) -> f64 {
        f64::from(self.x(pos)) + 0.5
    }

    /// Centre of a cell, vertically.
    fn cy(self, pos: Pos) -> f64 {
        f64::from(self.y(pos)) + 0.5
    }

    /// The checkerboard interior.
    fn field(self) -> String {
        let mut field = String::new();
        for y in 1..self.h - 1 {
            for x in 1..self.w - 1 {
                let fill = if (x + y) % 2 == 0 {
                    CHECKER_A
                } else {
                    CHECKER_B
                };
                let _ = write!(
                    field,
                    r#"<rect x="{x}" y="{y}" width="1" height="1" fill="{fill}"/>"#
                );
            }
        }
        field
    }

    /// The cell under a click, if it landed on the board at all.
    fn cell_at(self, ev: &web_sys::MouseEvent) -> Option<Pos> {
        let el = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())?;
        let rect = el.get_bounding_client_rect();
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        let vis_w = f64::from(self.w - 2);
        let vis_h = f64::from(self.h - 2);
        // The svg keeps the board square inside whatever box it is given,
        // centring it and leaving a margin on one axis. Taps have to be
        // measured against the board, not the box, or every column is off by
        // half the margin — and a tap in the margin is not a tap on the board
        // at all.
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
            return None; // in the margin beside the board
        }
        let x = 1 + num::to_u32(fx * vis_w);
        let y = 1 + num::to_u32(fy * vis_h);
        let (w, h) = (u32::from(self.w), u32::from(self.h));
        if x < w - 1 && y < h - 1 {
            Pos::try_from(y * w + x).ok()
        } else {
            None
        }
    }
}

/// One snek: a rounded body through the cell centres, the head square with
/// the player's emoji, and its numbers.
fn snek_svg(grid: Grid, snake: &[Pos], hp: u8, colour: &str, emoji: &str) -> String {
    let mut out = String::new();
    let (Some(&head), Some(&tail_cell)) = (snake.first(), snake.last()) else {
        return out;
    };

    // Collapse consecutive duplicates (stacked start / fresh growth).
    let mut path_cells: Vec<Pos> = Vec::with_capacity(snake.len());
    for &p in snake {
        if path_cells.last() != Some(&p) {
            path_cells.push(p);
        }
    }

    // Body: one rounded polyline through the centres, drawn through the
    // head cell too so it blends seamlessly into the head square.
    if let [_, .., t] = path_cells.as_slice() {
        let points: String = path_cells
            .iter()
            .map(|&p| format!("{},{}", grid.cx(p), grid.cy(p)))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = write!(
            out,
            r#"<polyline points="{points}" fill="none" stroke="{colour}" stroke-width="0.72" stroke-linecap="round" stroke-linejoin="round"/>"#
        );
        // Tail taper: a slightly smaller round cap on the last cell.
        let _ = write!(
            out,
            r#"<circle cx="{}" cy="{}" r="0.30" fill="{colour}"/>"#,
            grid.cx(*t),
            grid.cy(*t)
        );
    }

    // The head: a full body-coloured square with the player's emoji.
    let _ = write!(
        out,
        r#"<rect x="{x}" y="{y}" width="1" height="1" fill="{colour}"/>"#,
        x = grid.x(head),
        y = grid.y(head),
    );
    out.push_str(&emoji_text(grid.cx(head), grid.cy(head), 0.8, emoji));

    // Numbers as the snek unfurls: length on the first body tile,
    // health on the one after.
    if let Some(&second) = path_cells.get(1) {
        out.push_str(&number_text(
            grid.cx(second),
            grid.cy(second),
            &snake.len().to_string(),
        ));
    }
    if let Some(&third) = path_cells.get(2) {
        out.push_str(&number_text(
            grid.cx(third),
            grid.cy(third),
            &hp.to_string(),
        ));
    }
    // Stacked-segment count on the butt (growth queued up).
    let stacked = snake.iter().filter(|&&p| p == tail_cell).count();
    if stacked > 1
        && tail_cell != head
        && path_cells.get(1) != Some(&tail_cell)
        && path_cells.get(2) != Some(&tail_cell)
    {
        out.push_str(&number_text(
            grid.cx(tail_cell),
            grid.cy(tail_cell),
            &stacked.to_string(),
        ));
    }
    out
}

/// The dead: a grey square and a skull on every cell they left behind.
/// The head is gone with them (it went into whatever killed them), so
/// there's nothing to say whose they were but the space they took up.
fn clashes_svg(grid: Grid, turn: &Turn) -> String {
    let mut clashes = String::new();
    for clash in &turn.clashes {
        let _ = write!(
            clashes,
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#d3d3d3" opacity="0.8"/>"##,
            x = grid.x(clash.pos),
            y = grid.y(clash.pos),
        );
        clashes.push_str(&emoji_text(
            grid.cx(clash.pos),
            grid.cy(clash.pos),
            0.8,
            "💀",
        ));
    }
    clashes
}

/// The spotlight, as its own layer (`.spot`, shown only while the board
/// is `.spotlit`): a white veil over everything drawn so far, my snek
/// (`mine`) on top of it, a red box around my head and "you!" over it
/// (under it if the head is near the top and there's no room above).
/// The view stops at the walls, so on an edge cell the box is pulled in
/// on that side and the label slides along to stay whole.
fn spotlight_svg(grid: Grid, head: Pos, mine: &str) -> String {
    // The box stands this far out from the cell, its stroke this thick.
    const OUT: f64 = 0.18;
    const STROKE: f64 = 0.14;
    // Half the label's width (four letters of the mono font at 0.8).
    const HALF_LABEL: f64 = 1.1;
    let (x, y) = (f64::from(grid.x(head)), f64::from(grid.y(head)));
    // The view: from 1 to the far wall, either way.
    let (right, bottom) = (f64::from(grid.w - 1), f64::from(grid.h - 1));
    let inside = 1.0 + STROKE / 2.0;
    let (bx, by) = ((x - OUT).max(inside), (y - OUT).max(inside));
    let bw = (x + 1.0 + OUT).min(right - STROKE / 2.0) - bx;
    let bh = (y + 1.0 + OUT).min(bottom - STROKE / 2.0) - by;
    let label_y = if y <= 2.0 { y + 1.75 } else { y - 0.55 };
    let tx = (x + 0.5).clamp(1.0 + HALF_LABEL, right - HALF_LABEL);
    format!(
        r##"<g class="spot"><rect x="0" y="0" width="{w}" height="{h}" fill="#fff" opacity="0.72"/>{mine}<rect x="{bx}" y="{by}" width="{bw}" height="{bh}" fill="none" stroke="#e00" stroke-width="{STROKE}"/><text x="{tx}" y="{label_y}" font-size="0.8" font-weight="bold" font-family="snek mono, monospace" fill="#e00" stroke="#fff" stroke-width="0.12" paint-order="stroke" text-anchor="middle">you!</text></g>"##,
        w = grid.w,
        h = grid.h,
    )
}

/// What a board shows on top of the field: the "you!" spotlight, drawn in
/// but only shown while `spot_on` says so, and anything else laid over the
/// board's middle (the countdown).
pub(crate) struct Extras<V> {
    /// Draw the spotlight layer at all (before the first tick).
    pub spotlight: bool,
    /// Whether it's showing right now.
    pub spot_on: Signal<bool>,
    /// Whether the game is paused (the board greys out).
    pub held: Signal<bool>,
    /// A game just starting: how far into the board's fade-in we are, in
    /// ms. Given rather than started fresh, so a board redrawn partway
    /// through carries on rather than fading in again.
    pub entering_ms: Option<f64>,
    /// Laid over the board, centred on it.
    pub overlay: V,
}

impl Extras<()> {
    /// A plain board: no spotlight, nothing over it.
    pub(crate) fn none() -> Self {
        Self {
            spotlight: false,
            spot_on: Signal::stored(false),
            held: Signal::stored(false),
            entering_ms: None,
            overlay: (),
        }
    }
}

/// The board as SVG, exactly as the game draws it: the field, food, every
/// snek with its numbers, the dead, and optionally the "you!" spotlight
/// layer and a red ring round one cell.
pub(crate) fn board_svg(
    config: &GameConfig,
    players: &[PlayerProfile],
    turn: &Turn,
    my_id: &str,
    spotlight: bool,
    ring: Option<Pos>,
) -> String {
    let grid = Grid {
        w: u16::from(config.board_width),
        h: u16::from(config.board_height),
    };
    let colour_of: HashMap<&str, &str> = players
        .iter()
        .map(|p| (p.id.as_str(), p.colour.as_str()))
        .collect();
    let emoji_of: HashMap<&str, &str> = players
        .iter()
        .map(|p| (p.id.as_str(), p.emoji.as_str()))
        .collect();

    let field = grid.field();

    // --- food stays emoji, like tac --------------------------------------
    let food: String = turn
        .food
        .iter()
        .map(|&pos| emoji_text(grid.cx(pos), grid.cy(pos), 0.8, &config.food_emoji))
        .collect();

    // --- sneks ------------------------------------------------------------
    // With a spotlight, my snek is also drawn a second time, in the
    // spotlight's layer over the veil.
    let my_head = turn
        .snakes
        .get(my_id)
        .filter(|_| spotlight)
        .and_then(|s| s.first().copied());
    let mut sneks = String::new();
    let mut mine = String::new();
    for (player_id, snake) in &turn.snakes {
        let drawn = snek_svg(
            grid,
            snake,
            turn.health.get(player_id).copied().unwrap_or(0),
            colour_of.get(player_id.as_str()).copied().unwrap_or("#999"),
            emoji_of.get(player_id.as_str()).copied().unwrap_or("🐍"),
        );
        if my_head.is_some() && player_id == my_id {
            mine.clone_from(&drawn);
        }
        sneks.push_str(&drawn);
    }

    let clashes = clashes_svg(grid, turn);

    // A red ring round one cell, for pointing something out (the rules).
    let ring = ring.map_or_else(String::new, |pos| {
        format!(
            r##"<rect x="{x}" y="{y}" width="1" height="1" fill="none" stroke="#e00" stroke-width="0.1" rx="0.15"/>"##,
            x = grid.x(pos),
            y = grid.y(pos),
        )
    });

    let spot = my_head.map_or_else(String::new, |head| spotlight_svg(grid, head, &mine));

    // Crop the view to the playable interior — the walls sit just off-screen.
    let svg = format!(
        r#"<svg viewBox="1 1 {vw} {vh}" xmlns="http://www.w3.org/2000/svg" style="display:block;width:100%;height:100%;">
            {field}{food}{sneks}{clashes}{ring}{spot}
        </svg>"#,
        vw = grid.w - 2,
        vh = grid.h - 2,
    );
    svg
}

pub(crate) fn board_view<F: Fn(Pos) + Copy + 'static, V: IntoView + 'static>(
    config: &GameConfig,
    players: &[PlayerProfile],
    turn: &Turn,
    my_id: &str,
    // Before the first tick: grey out everything but my snek and put a red
    // box and a "you!" over it, so nobody spends the start looking for
    // themselves.
    extras: Extras<V>,
    on_cell: F,
) -> impl IntoView + use<F, V> {
    let Extras {
        spotlight,
        spot_on,
        held,
        entering_ms,
        overlay,
    } = extras;
    let grid = Grid {
        w: u16::from(config.board_width),
        h: u16::from(config.board_height),
    };
    let svg = board_svg(config, players, turn, my_id, spotlight, None);

    view! {
        <div
            class="board-wrap"
            class:spotlit=move || spot_on.get()
            class:held=move || held.get()
            class:entering=entering_ms.is_some()
            style=entering_ms.map(|ms| format!("animation-delay: -{ms:.0}ms;"))
            on:click=move |ev: web_sys::MouseEvent| {
                if let Some(pos) = grid.cell_at(&ev) {
                    on_cell(pos);
                }
            }
        >
            <div class="board-svg" inner_html=svg></div>
            {overlay}
        </div>
    }
}
