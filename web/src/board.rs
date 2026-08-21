//! The grid — a straight port of tac's board look: coloured snek cells with
//! white gaps, emoji heads, length on the neck, 🎃 food, 🧱 walls, 💀 clashes.

use leptos::prelude::*;
use snek_shared::{GameConfig, PlayerProfile, Pos, Turn};
use std::collections::HashMap;

#[derive(Clone, Default)]
struct CellView {
    background: Option<String>,
    content: Option<String>,
    is_wall: bool,
    is_clash: bool,
    /// Scale factor for the content (emoji vs number).
    small: bool,
}

pub fn board_view(
    config: GameConfig,
    players: Vec<PlayerProfile>,
    turn: &Turn,
    on_cell: impl Fn(Pos) + Copy + 'static,
) -> impl IntoView {
    let w = config.board_width as usize;
    let h = config.board_height as usize;
    let cells = w * h;

    let profile: HashMap<&str, &PlayerProfile> =
        players.iter().map(|p| (p.id.as_str(), p)).collect();

    let mut views: Vec<CellView> = vec![CellView::default(); cells];

    // Walls
    for pos in config.walls() {
        let v = &mut views[pos as usize];
        v.is_wall = true;
        v.content = Some("🧱".to_string());
    }

    // Sneks
    for (player_id, snake) in &turn.snakes {
        let p = profile.get(player_id.as_str());
        let colour = p.map(|p| p.colour.clone()).unwrap_or_else(|| "#999".into());
        for (i, &pos) in snake.iter().enumerate().rev() {
            let v = &mut views[pos as usize];
            v.background = Some(colour.clone());
            if i == 0 {
                v.content = Some(p.map(|p| p.emoji.clone()).unwrap_or_else(|| "⭕".into()));
                v.small = false;
            } else if i == 1 && snake[1] != snake[0] {
                v.content = Some(snake.len().to_string());
                v.small = true;
            }
        }
    }

    // Food
    for &pos in &turn.food {
        let v = &mut views[pos as usize];
        if v.content.is_none() {
            v.content = Some("🎃".to_string());
        }
    }

    // Clashes
    for clash in &turn.clashes {
        let v = &mut views[clash.pos as usize];
        v.is_clash = true;
        v.background = None;
        v.content = Some("💀".to_string());
        v.small = false;
    }

    let board_style = format!(
        "grid-template-columns: repeat({w}, 1fr); \
         font-size: calc(min(92vw, 560px) / {w} * 0.55);"
    );

    let cells_view = views
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            let style = match (&v.background, v.small) {
                (Some(bg), false) => format!("background: {bg};"),
                (Some(bg), true) => format!("background: {bg}; font-size: 0.8em;"),
                (None, _) => String::new(),
            };
            view! {
                <div
                    class="cell"
                    class:wall=v.is_wall
                    class:clash=v.is_clash
                    style=style
                    on:click=move |_| on_cell(i as Pos)
                >
                    {v.content}
                </div>
            }
        })
        .collect_view();

    view! {
        <div class="board" style=board_style>
            {cells_view}
        </div>
    }
}
