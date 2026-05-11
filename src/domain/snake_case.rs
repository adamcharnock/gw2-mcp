//! Cheap `snake_case` → Title Case transformation for GW2 ids that ship
//! without a human-readable name (raid encounters, dungeon paths).
//!
//! The transformation is deterministic and reversible from the
//! canonical id, so we don't need a hand-curated lookup table that has
//! to be updated every time `ArenaNet` ships a new wing or path.
//! Dungeon path ids of the form `<dungeon>_path_<n>` get a slightly
//! prettier split-form (`Foo Path 1` rather than `Foo Path 1` —
//! same in this case but documented for future variations).

/// Title-case a `snake_case` id. `"vale_guardian"` → `"Vale Guardian"`,
/// `"qadim_the_peerless"` → `"Qadim The Peerless"`. Empty input yields
/// empty output.
#[must_use]
pub fn title_case(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .split('_')
        .map(capitalise_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalise_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut out = first.to_uppercase().collect::<String>();
            out.push_str(chars.as_str());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_two_word() {
        assert_eq!(title_case("vale_guardian"), "Vale Guardian");
    }

    #[test]
    fn multi_word_with_article() {
        assert_eq!(title_case("qadim_the_peerless"), "Qadim The Peerless");
    }

    #[test]
    fn dungeon_path_id() {
        assert_eq!(
            title_case("ascalon_catacombs_path_1"),
            "Ascalon Catacombs Path 1"
        );
    }

    #[test]
    fn single_word() {
        assert_eq!(title_case("slothasor"), "Slothasor");
    }

    #[test]
    fn empty_input_yields_empty() {
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("   "), "");
    }

    #[test]
    fn preserves_already_titlecased_letters() {
        // GW2 ids are always `snake_case` lowercased, but be defensive:
        // if someone passes "Vale_Guardian" (which would be a bug
        // upstream), don't downcase mid-word.
        assert_eq!(title_case("vale_GUARDIAN"), "Vale GUARDIAN");
    }
}
