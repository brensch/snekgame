//! Game names: three words picked at random.
//!
//! Two adjectives and a noun, from lists with a tired, sardonic streak
//! ("lukewarm-mandatory-potluck"). The name is also the game's link, so it
//! is lowercase, dash-joined, and short enough to say out loud.

/// The mood of the thing.
pub const ADJECTIVES: &[&str] = &[
    "lukewarm",
    "beige",
    "damp",
    "overdue",
    "mandatory",
    "unpaid",
    "adequate",
    "mediocre",
    "regrettable",
    "tepid",
    "bureaucratic",
    "moist",
    "unremarkable",
    "overcooked",
    "lacklustre",
    "soggy",
    "existential",
    "optional",
    "decaffeinated",
    "discount",
    "expired",
    "brooding",
    "smug",
    "sulking",
    "wistful",
    "dubious",
    "unsolicited",
    "passive",
    "aggressive",
    "polite",
    "awkward",
    "beleaguered",
    "lethargic",
    "apologetic",
    "resigned",
    "sheepish",
    "petty",
    "earnest",
    "pointless",
    "inevitable",
    "accidental",
    "reluctant",
    "generic",
    "corporate",
    "laminated",
    "underwhelming",
    "recycled",
    "disgruntled",
    "salaried",
    "crumbling",
];

/// The thing itself.
pub const NOUNS: &[&str] = &[
    "casserole",
    "spreadsheet",
    "meeting",
    "timesheet",
    "voicemail",
    "cardigan",
    "sandwich",
    "committee",
    "memo",
    "receipt",
    "invoice",
    "deadline",
    "potluck",
    "intern",
    "printer",
    "lanyard",
    "dentist",
    "tuesday",
    "crouton",
    "carpool",
    "mortgage",
    "flatpack",
    "roundabout",
    "tupperware",
    "toaster",
    "excuse",
    "shrug",
    "compromise",
    "sigh",
    "cubicle",
    "raincoat",
    "lasagne",
    "minivan",
    "cabbage",
    "survey",
    "inbox",
    "fax",
    "stapler",
    "beanbag",
    "cutlery",
    "slideshow",
    "puddle",
    "teabag",
    "handshake",
    "commute",
    "audit",
    "wheelbarrow",
    "biscuit",
];

/// A fresh game name: two different adjectives and a noun, dash-joined.
/// `pick` is given how many there are to choose from and returns an index
/// below that (out of range is clamped).
pub fn game_name(mut pick: impl FnMut(usize) -> usize) -> String {
    let mut choose = |from: &[&'static str]| from[pick(from.len()).min(from.len() - 1)];
    let first = choose(ADJECTIVES);
    let mut second = choose(ADJECTIVES);
    if second == first {
        // The neighbour instead: still random, never "damp-damp".
        let i = ADJECTIVES.iter().position(|a| *a == first).unwrap_or(0);
        second = ADJECTIVES[(i + 1) % ADJECTIVES.len()];
    }
    format!("{first}-{second}-{}", choose(NOUNS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are three distinct-enough words that fit a link.
    #[test]
    fn names_are_two_different_adjectives_and_a_noun() {
        let mut seed = 3usize;
        for _ in 0..500 {
            let name = game_name(|n| {
                seed = seed.wrapping_mul(2_654_435_761).wrapping_add(7);
                seed % n
            });
            let words: Vec<&str> = name.split('-').collect();
            assert_eq!(words.len(), 3, "{name}");
            assert!(
                ADJECTIVES.contains(&words[0]) && ADJECTIVES.contains(&words[1]),
                "{name}"
            );
            assert_ne!(words[0], words[1], "{name}");
            assert!(NOUNS.contains(&words[2]), "{name}");
            assert!(name.len() <= 48, "{name}");
        }
    }

    /// Picking the same adjective twice takes the next one along instead,
    /// and an out-of-range pick is clamped rather than panicking.
    #[test]
    fn a_repeated_or_wild_pick_still_makes_a_name() {
        assert_eq!(
            game_name(|_| 0),
            format!("{}-{}-{}", ADJECTIVES[0], ADJECTIVES[1], NOUNS[0])
        );
        let last = ADJECTIVES.len() - 1;
        assert_eq!(
            game_name(|n| n + 9),
            format!(
                "{}-{}-{}",
                ADJECTIVES[last],
                ADJECTIVES[0],
                NOUNS[NOUNS.len() - 1]
            )
        );
    }

    /// Every word is lowercase letters only, so a name is a clean link.
    #[test]
    fn every_word_is_plain_lowercase() {
        for w in ADJECTIVES.iter().chain(NOUNS) {
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        }
    }
}
