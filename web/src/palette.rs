//! Profile options — emoji list and colour formula lifted from tac.

pub const EMOJIS: &[&str] = &[
    "👍", "🚀", "🔥", "🍕", "🌈", "🦍", "🎲", "🦄", "💡", "🌋", "🎭", "🎷", "🧪", "🦕", "🧠",
    "📡", "🔮", "🦜", "🦥", "🌵", "🧳", "🗿", "🍝", "🍤", "🦷", "🧻", "🥩", "🍉", "🪱", "🐍",
    "🕺", "🎃",
];

/// tac's generateColor: hsl(h, 85%, 60%).
pub fn colour(hue: u32) -> String {
    format!("hsl({}, 85%, 60%)", hue % 360)
}

pub fn swatches() -> Vec<String> {
    (0..16).map(|i| colour(i * 360 / 16)).collect()
}
