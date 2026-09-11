use super::*;
use crate::features::FeatureFlag;

#[test]
fn emoji_go_on_in_order_and_three_at_most() {
    let mut tags = TabTags::default();
    assert!(tags.set("🔥", true));
    assert!(tags.set("⭐", true));
    assert!(tags.set("✅", true));
    assert!(tags.is_full());
    assert!(!tags.set("🧪", true), "a full tab takes no fourth emoji");
    assert_eq!(tags.to_vec(), ["🔥", "⭐", "✅"]);

    assert!(tags.set("⭐", false));
    assert_eq!(tags.to_vec(), ["🔥", "✅"]);
    assert!(
        !tags.set("⭐", false),
        "taking off an emoji the tab doesn't wear changes nothing"
    );
    assert!(tags.set("🧪", true));
    assert_eq!(tags.to_vec(), ["🔥", "✅", "🧪"]);

    assert!(tags.clear());
    assert!(tags.is_empty());
    assert!(!tags.clear());
}

#[test]
fn two_spellings_of_one_emoji_are_one_tag() {
    let mut tags = TabTags::default();
    assert!(tags.set("⭐", true));
    assert!(tags.contains("⭐\u{FE0F}"));
    assert!(!tags.set("⭐\u{FE0F}", true));
    assert!(tags.set("⭐\u{FE0F}", false));
    assert!(tags.is_empty());
}

#[test]
fn stored_tags_come_back_as_single_emoji_each_once_three_at_most() {
    let stored = ["🔥", "not an emoji", "", "⭐🔥", "🔥", "✅", "❤️", "🧪"].map(String::from);
    assert_eq!(TabTags::from_stored(stored).to_vec(), ["🔥", "✅", "❤️"]);
}

/// A tab wears its own emoji; a floating tab with none of its own wears ⭐; a
/// tab that doesn't float and has none wears nothing; and with tags off no tab
/// wears anything. Over every flag state.
#[test]
fn a_floating_tab_without_emoji_wears_a_star_and_nothing_shows_with_tags_off() {
    for pins in [false, true] {
        for stars in [false, true] {
            let _pins = FeatureFlag::PinnedTabs.override_enabled(pins);
            let _stars = FeatureFlag::StarredTabs.override_enabled(stars);
            let on = pins && stars;
            let none = TabTags::default();
            let fire = TabTags::from_stored(["🔥".to_owned()]);
            let expect = |tags: &TabTags| if on { tags.clone() } else { none.clone() };
            assert_eq!(worn_tags(&fire, true), expect(&fire));
            assert_eq!(worn_tags(&fire, false), expect(&fire));
            assert_eq!(worn_tags(&none, true), expect(&TabTags::star()));
            assert_eq!(worn_tags(&none, false), none);
        }
    }
}

/// A tab wears its emoji once: on its Panes-layout header when the header
/// wears them, and otherwise on its first row only.
#[test]
fn a_tab_wears_its_emoji_once_on_its_first_row_or_its_header() {
    let fire = TabTags::from_stored(["🔥".to_owned(), "✅".to_owned()]);
    let none = TabTags::default();
    for header_is_drawn in [false, true] {
        for worn in [&fire, &none] {
            let header_wears = header_shows_tags(worn, header_is_drawn);
            assert_eq!(header_wears, header_is_drawn && !worn.is_empty());
            let rows: Vec<TabTags> = (0..3)
                .map(|row| row_tags(worn, row == 0, header_wears))
                .collect();
            let wearing =
                rows.iter().filter(|tags| !tags.is_empty()).count() + usize::from(header_wears);
            assert_eq!(wearing, usize::from(!worn.is_empty()));
            if !header_wears {
                assert_eq!(&rows[0], worn);
            }
        }
    }
}

/// Any sequence of putting on, taking off and clearing leaves at most three
/// emoji, each once, in the order they went on, and says whether it changed
/// anything exactly when it did. Stated as "always", so it sweeps sequences
/// rather than sampling a few.
#[test]
fn any_sequence_keeps_at_most_three_distinct_emoji_in_order() {
    const ALPHABET: [&str; 6] = ["⭐", "⭐\u{FE0F}", "🔥", "✅", "🧪", "💤"];
    let key = |emoji: &str| -> String { emoji.chars().filter(|c| *c != '\u{FE0F}').collect() };
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    for _ in 0..300 {
        let mut tags = TabTags::default();
        let mut model: Vec<String> = Vec::new();
        for _ in 0..30 {
            let emoji = ALPHABET[next(ALPHABET.len())];
            match next(6) {
                0 => {
                    assert_eq!(tags.clear(), !model.is_empty());
                    model.clear();
                }
                1 | 2 => {
                    let had = model.contains(&key(emoji));
                    assert_eq!(tags.set(emoji, false), had);
                    model.retain(|existing| *existing != key(emoji));
                }
                _ => {
                    let goes_on = !model.contains(&key(emoji)) && model.len() < MAX_TAB_TAGS;
                    assert_eq!(tags.set(emoji, true), goes_on);
                    if goes_on {
                        model.push(key(emoji));
                    }
                }
            }
            let keys: Vec<String> = tags.iter().map(key).collect();
            assert_eq!(keys, model);
            assert!(tags.len() <= MAX_TAB_TAGS);
        }
    }
}
