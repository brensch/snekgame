//! Profile options — emoji lists and colour formula lifted from tac.

/// tac's full profile emoji list (shared/types/Emojis.ts).
pub const EMOJIS: &[&str] = &[
    "👍", "🚀", "🔥", "🍕", "🌈", "🦍", "🎲", "🦄", "💡", "🌋", "🎭", "🎷", "🧪", "🦕", "🧠",
    "📡", "🔮", "🦜", "🦥", "🌵", "🧳", "🗿", "🍝", "🍤", "🦷", "🧻", "🥩", "🍉", "🪱", "🧿",
    "🥒", "🐉", "🎢", "🤯", "🦔", "🫖", "🐓", "🫃", "🎡", "🥴", "🦨", "🧚", "🌭", "🧼", "🧇",
    "🛹", "🫛", "🍄", "🦞", "🪲", "🍔", "🦐", "🛷", "🦒", "🌙", "💾", "💿", "🦖", "🥷", "🛼",
    "🦚", "🍙", "🛁", "🍧", "🛢", "🎱", "🛶", "🎪", "🎬", "🥨", "🦉", "🦦", "🌯", "🧀", "🦧",
    "🥌", "🪐", "🧸", "💍", "🌶️", "🧊", "🧈", "🎃", "🦑", "🍖", "🦘", "🥛", "🦀", "🧦", "🍌",
    "🍯", "🥟", "🪂", "🥋", "🍿", "🎠", "🧫", "🍫", "🎐", "🪡", "🥚", "🍩", "🪕", "🎗️", "🎴",
    "🪑", "🛩", "🦋", "🍷", "🪃", "🥢", "🥄", "🧲", "🪨", "🍠", "🦗", "🦢", "🍡", "🦤", "🪒",
    "🧽", "🪞", "🦫", "🍟", "🎨", "🎓", "🍹", "🪦", "🥀", "🎏", "🧨", "🛣️", "🎮", "🧁", "🥬",
    "🧢", "🎧", "🍂", "🍮", "🎆", "🎎", "🍻", "🧭", "🧬", "🍱", "🦙", "🎹", "🍜", "🪜", "🎼",
    "🍋", "🎯", "🛑", "🥃", "🥓", "🍰", "🪁", "🦣", "🎤", "🛸", "🎛️", "🍘", "🧋", "🛍️", "🪝",
    "🍲", "🪄", "🎿", "🍳", "🪀", "🥔", "🪣", "🎣", "🧩", "🦭", "🥵", "🥶", "🛏️", "🪵", "🍈",
    "🛋", "🍥", "🧂", "🎳", "🪔", "🪗", "🥏", "🪚", "🥕", "🧆", "🧄", "🍭", "🧃", "🧯",
];

/// tac's sendEmojis — the actionish set that rotates on the play button.
pub const SEND_EMOJIS: &[&str] = &[
    "🚀", "🛫", "🏹", "💨", "📈", "💸", "🏄", "🌙", "➡️", "💹", "🐂", "🎬", "🚦", "👉", "🔛",
    "🆒", "🦍", "✅", "👍", "🍑", "🦶", "🤑", "⛷️", "🕺", "🏌️", "🏂", "🕹️", "🥇", "💡", "🚒",
    "🏎️", "✈️", "🛩️", "🚢", "🛥️", "⛵", "🚤", "🚨", "🆕", "▶️", "🔜", "☑️",
];

/// tac's generateColor: hsl(h, 85%, 60%).
pub fn colour(hue: u32) -> String {
    format!("hsl({}, 85%, 60%)", hue % 360)
}

pub fn swatches() -> Vec<String> {
    (0..16).map(|i| colour(i * 360 / 16)).collect()
}

/// A random spread of profile emojis for the picker.
/// Things a snake could plausibly want to eat. Only edible things belong
/// here — the food is the one emoji on the board that has to read as a
/// snack at a glance.
pub const FOODS: &[&str] = &[
    "🎃", "🍎", "🍕", "🍔", "🍩", "🍒", "🍓", "🌮", "🍗", "🥑", "🍇", "🍌", "🥕", "🧀", "🍰",
    "🍪", "🥨", "🌭", "🍤", "🍣", "🥩", "🍉", "🍍", "🥞", "🍦", "🍫", "🥐", "🌽", "🍄", "🥟",
    "🍭", "🧁", "🍿", "🥓", "🍑", "🥝", "🫐", "🍆", "🥦", "🦴", "🍋", "🥯", "🍠", "🧇",
];

/// A handful of foods to choose between, always including the one already
/// picked so a shuffle never yanks the current choice off the screen.
pub fn random_food_set(count: usize, keep: &str) -> Vec<String> {
    let mut pool: Vec<&str> = FOODS.iter().copied().filter(|f| *f != keep).collect();
    let mut out = vec![keep.to_string()];
    for _ in 1..count.min(pool.len() + 1) {
        let i = (js_sys::Math::random() * pool.len() as f64) as usize;
        out.push(pool.swap_remove(i.min(pool.len() - 1)).to_string());
    }
    out
}

pub fn random_emoji_set(count: usize) -> Vec<String> {
    let mut pool: Vec<&str> = EMOJIS.to_vec();
    let mut out = Vec::with_capacity(count);
    for _ in 0..count.min(pool.len()) {
        let i = (js_sys::Math::random() * pool.len() as f64) as usize;
        out.push(pool.swap_remove(i.min(pool.len() - 1)).to_string());
    }
    out
}
