//! Profile options — emoji lists and colour formula lifted from tac.

/// tac's full profile emoji list (shared/types/Emojis.ts).
pub(crate) const EMOJIS: &[&str] = &[
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
pub(crate) const SEND_EMOJIS: &[&str] = &[
    "🚀", "🛫", "🏹", "💨", "📈", "💸", "🏄", "🌙", "➡️", "💹", "🐂", "🎬", "🚦", "👉", "🔛", "🆒",
    "🦍", "✅", "👍", "🍑", "🦶", "🤑", "⛷️", "🕺", "🏌️", "🏂", "🕹️", "🥇", "💡", "🚒", "🏎️", "✈️",
    "🛩️", "🚢", "🛥️", "⛵", "🚤", "🚨", "🆕", "▶️", "🔜", "☑️",
];

/// tac's generateColor: hsl(h, 85%, 60%).
pub(crate) fn colour(hue: u32) -> String {
    format!("hsl({}, 85%, 60%)", hue % 360)
}

pub(crate) fn swatches() -> Vec<String> {
    (0..16).map(|i| colour(i * 360 / 16)).collect()
}

/// Things a snake could plausibly want to eat. Only edible things belong
/// here — the food is the one emoji on the board that has to read as a
/// snack at a glance.
pub(crate) const FOODS: &[&str] = &[
    "🎃", "🍎", "🍕", "🍔", "🍩", "🍒", "🍓", "🌮", "🍗", "🥑", "🍇", "🍌", "🥕", "🧀", "🍰", "🍪",
    "🥨", "🌭", "🍤", "🍣", "🥩", "🍉", "🍍", "🥞", "🍦", "🍫", "🥐", "🌽", "🍄", "🥟", "🍭", "🧁",
    "🍿", "🥓", "🍑", "🥝", "🫐", "🍆", "🥦", "🦴", "🍋", "🥯", "🍠", "🧇",
];

/// How many foods the lobby offers at once.
pub(crate) const FOOD_HAND: usize = 8;

/// This phone's hand of foods in the lobby. The hand is ours alone: it is
/// dealt here, shuffled here, and nobody else ever sees it. The pick is the
/// lobby's, shared by everyone, and the hand has one slot that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FoodHand {
    foods: Vec<String>,
    /// Where the lobby's pick sits in `foods`.
    picked_slot: usize,
}

impl FoodHand {
    /// A fresh random hand with `picked` in the first slot.
    pub(crate) fn dealt(picked: &str) -> Self {
        Self {
            foods: shuffled_with(FOODS, FOOD_HAND, picked),
            picked_slot: 0,
        }
    }

    /// The lobby's pick is now `picked`. If it is already in the hand, the
    /// pick moves to it and nothing else does: whatever was tapped stays
    /// exactly where it was. If it isn't (someone else picked from their
    /// own hand), it takes over the slot the old pick was in, and the rest
    /// of the hand is left alone.
    pub(crate) fn show_pick(&mut self, picked: &str) {
        if let Some(slot) = self.foods.iter().position(|f| f == picked) {
            self.picked_slot = slot;
        } else if let Some(slot) = self.foods.get_mut(self.picked_slot) {
            picked.clone_into(slot);
        }
    }

    /// The foods, in their slots.
    pub(crate) fn foods(&self) -> &[String] {
        &self.foods
    }
}

/// A fresh hand from `pool`, with `keep` at the front. Whatever is
/// currently chosen always survives a shuffle and always sits in the same
/// place, so the choice you just made can't be shuffled out from under you.
pub(crate) fn shuffled_with(pool: &[&str], count: usize, keep: &str) -> Vec<String> {
    let mut rest: Vec<&str> = pool.iter().copied().filter(|e| *e != keep).collect();
    let mut out = Vec::with_capacity(count);
    out.push(keep.to_owned());
    while out.len() < count && !rest.is_empty() {
        let i = crate::num::random_index(rest.len());
        out.push(rest.swap_remove(i).to_owned());
    }
    out
}

/// The hand as it should be drawn. A hand only changes when someone asks for
/// a shuffle — picking from it must leave it exactly as it is, or the thing
/// you were aiming at moves as you tap. The one exception is a choice that
/// isn't in the hand at all (another player changed the food), which is
/// shown at the front so the selection is always visible somewhere.
pub(crate) fn hand_showing(hand: Vec<String>, selected: &str, count: usize) -> Vec<String> {
    if hand.iter().any(|e| e == selected) {
        return hand;
    }
    let mut out = vec![selected.to_owned()];
    out.extend(hand.into_iter().take(count.saturating_sub(1)));
    out
}

/// Cells in the profile's emoji grid.
pub(crate) const EMOJI_HAND: usize = 24;

/// A random spread of profile emojis for the picker.
pub(crate) fn random_emoji_set(count: usize, keep: &str) -> Vec<String> {
    shuffled_with(EMOJIS, count, keep)
}
