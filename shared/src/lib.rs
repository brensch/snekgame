//! The game itself, shared by the server and the browser: the types every
//! message is made of, the deterministic rules engine, the house bot's
//! search and the wire protocol. Both ends compile this same crate, so they
//! can never disagree about the rules or the bytes.

pub mod bot;
pub mod engine;
pub mod names;
pub mod protocol;
pub mod rng;
pub mod types;

pub use engine::{Engine, clockwise_start_dir};
pub use types::*;
