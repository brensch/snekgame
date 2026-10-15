//! Profile options — emoji lists and colour formula lifted from tac.

/// tac's full profile emoji list (shared/types/Emojis.ts).
pub const EMOJIS: &[&str] = &[
    "👍", "🚀", "🔥", "🍕", "🌈", "🦍", "🎲", "🦄", "💡", "🌋", "🎭", "🎷", "🧪", "🦕", "🧠", "📡",
    "🔮", "🦜", "🦥", "🌵", "🧳", "🗿", "🍝", "🍤", "🦷", "🧻", "🥩", "🍉", "🪱", "🧿", "🥒", "🐉",
    "🎢", "🤯", "🦔", "🫖", "🐓", "🫃", "🎡", "🥴", "🦨", "🧚", "🌭", "🧼", "🧇", "🛹", "🫛", "🍄",
    "🦞", "🪲", "🍔", "🦐", "🛷", "🦒", "🌙", "💾", "💿", "🦖", "🥷", "🛼", "🦚", "🍙", "🛁", "🍧",
    "🛢", "🎱", "🛶", "🎪", "🎬", "🥨", "🦉", "🦦", "🌯", "🧀", "🦧", "🥌", "🪐", "🧸", "💍", "🌶️",
    "🧊", "🧈", "🎃", "🦑", "🍖", "🦘", "🥛", "🦀", "🧦", "🍌", "🍯", "🥟", "🪂", "🥋", "🍿", "🎠",
    "🧫", "🍫", "🎐", "🪡", "🥚", "🍩", "🪕", "🎗️", "🎴", "🪑", "🛩", "🦋", "🍷", "🪃", "🥢", "🥄",
    "🧲", "🪨", "🍠", "🦗", "🦢", "🍡", "🦤", "🪒", "🧽", "🪞", "🦫", "🍟", "🎨", "🎓", "🍹", "🪦",
    "🥀", "🎏", "🧨", "🛣️", "🎮", "🧁", "🥬", "🧢", "🎧", "🍂", "🍮", "🎆", "🎎", "🍻", "🧭", "🧬",
    "🍱", "🦙", "🎹", "🍜", "🪜", "🎼", "🍋", "🎯", "🛑", "🥃", "🥓", "🍰", "🪁", "🦣", "🎤", "🛸",
    "🎛️", "🍘", "🧋", "🛍️", "🪝", "🍲", "🪄", "🎿", "🍳", "🪀", "🥔", "🪣", "🎣", "🧩", "🦭", "🥵",
    "🥶", "🛏️", "🪵", "🍈", "🛋", "🍥", "🧂", "🎳", "🪔", "🪗", "🥏", "🪚", "🥕", "🧆", "🧄", "🍭",
    "🧃", "🧯",
];

/// tac's sendEmojis — the actionish set that rotates on the play button.
pub const SEND_EMOJIS: &[&str] = &[
    "🚀", "🛫", "🏹", "💨", "📈", "💸", "🏄", "🌙", "➡️", "💹", "🐂", "🎬", "🚦", "👉", "🔛", "🆒",
    "🦍", "✅", "👍", "🍑", "🦶", "🤑", "⛷️", "🕺", "🏌️", "🏂", "🕹️", "🥇", "💡", "🚒", "🏎️", "✈️",
    "🛩️", "🚢", "🛥️", "⛵", "🚤", "🚨", "🆕", "▶️", "🔜", "☑️",
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
    "🎃", "🍎", "🍕", "🍔", "🍩", "🍒", "🍓", "🌮", "🍗", "🥑", "🍇", "🍌", "🥕", "🧀", "🍰", "🍪",
    "🥨", "🌭", "🍤", "🍣", "🥩", "🍉", "🍍", "🥞", "🍦", "🍫", "🥐", "🌽", "🍄", "🥟", "🍭", "🧁",
    "🍿", "🥓", "🍑", "🥝", "🫐", "🍆", "🥦", "🦴", "🍋", "🥯", "🍠", "🧇",
];

/// A fresh hand from `pool`, with `keep` at the front. Whatever is
/// currently chosen always survives a shuffle and always sits in the same
/// place, so the choice you just made can't be shuffled out from under you.
pub fn shuffled_with(pool: &[&str], count: usize, keep: &str) -> Vec<String> {
    let mut rest: Vec<&str> = pool.iter().copied().filter(|e| *e != keep).collect();
    let mut out = Vec::with_capacity(count);
    out.push(keep.to_string());
    while out.len() < count && !rest.is_empty() {
        let i = (js_sys::Math::random() * rest.len() as f64) as usize;
        out.push(rest.swap_remove(i.min(rest.len() - 1)).to_string());
    }
    out
}

/// The hand as it should be drawn. A hand only changes when someone asks for
/// a shuffle — picking from it must leave it exactly as it is, or the thing
/// you were aiming at moves as you tap. The one exception is a choice that
/// isn't in the hand at all (another player changed the food), which is
/// shown at the front so the selection is always visible somewhere.
pub fn hand_showing(hand: Vec<String>, selected: &str, count: usize) -> Vec<String> {
    if hand.iter().any(|e| e == selected) {
        return hand;
    }
    let mut out = vec![selected.to_string()];
    out.extend(hand.into_iter().take(count.saturating_sub(1)));
    out
}

/// Cells in the lobby's food hand, and in the profile's emoji grid.
pub const FOOD_HAND: usize = 8;
pub const EMOJI_HAND: usize = 24;

pub fn random_emoji_set(count: usize, keep: &str) -> Vec<String> {
    shuffled_with(EMOJIS, count, keep)
}
