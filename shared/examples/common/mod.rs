//! Positions shared by the fingerprint and profiling examples.

use snek_shared::bot::think;
use snek_shared::{Engine, GameConfig, PlayerId, Turn};

/// Positions from real playthroughs: 2-, 3- and 4-snake games on three
/// board sizes, sampled every few turns.
#[must_use]
pub fn fingerprint_positions() -> Vec<(GameConfig, Turn, PlayerId)> {
    let mut out = Vec::new();
    for (size, players, seed) in [(11u8, 2usize, 1u64), (13, 3, 2), (17, 4, 3), (13, 2, 4)] {
        let cfg = GameConfig {
            board_width: size,
            board_height: size,
            ..GameConfig::default()
        };
        let ids: Vec<PlayerId> = (0..players).map(|i| format!("p{i}")).collect();
        let mut engine = Engine::new(cfg.clone(), ids, seed);
        let mut turn = engine.first_turn();
        for t in 0..60 {
            if turn.game_over() {
                break;
            }
            if t % 5 == 0 {
                out.push((cfg.clone(), turn.clone(), turn.alive[0].clone()));
            }
            let moves = turn
                .alive
                .iter()
                .map(|p| (p.clone(), think(&cfg, &turn, p, 400, 1).0))
                .collect();
            turn = engine.next_turn(&turn, &moves);
        }
    }
    out
}
