use super::*;
use crate::workspace::view::tab_tags::is_tag_emoji;

#[test]
fn a_search_finds_an_emoji_by_its_name_or_its_shortcode_first() {
    for query in ["fire", "Fire", " fire ", ":fire:"] {
        assert_eq!(
            search(query).first().map(|emoji| emoji.as_str()),
            Some("🔥"),
            "query {query:?}"
        );
    }
    assert_eq!(
        search("thumbs up").first().map(|emoji| emoji.as_str()),
        Some("👍")
    );
    assert_eq!(
        search("white_check_mark")
            .first()
            .map(|emoji| emoji.as_str()),
        Some("✅")
    );
}

#[test]
fn a_blank_query_matches_nothing_and_one_that_matches_nothing_leaves_no_rows() {
    assert!(search("").is_empty());
    assert!(search("  :: ").is_empty());
    assert!(build_rows("zzqxj", &suggestions(&[])).is_empty());
}

#[test]
fn with_no_query_the_grid_shows_the_suggestions_then_every_group() {
    let rows = build_rows("", &suggestions(&[]));
    let titles: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            PickerRow::Title(title) => Some(*title),
            PickerRow::Emoji(_) => None,
        })
        .collect();
    assert_eq!(
        titles,
        [
            "Suggested",
            "Smileys & Emotion",
            "People & Body",
            "Animals & Nature",
            "Food & Drink",
            "Travel & Places",
            "Activities",
            "Objects",
            "Symbols",
            "Flags",
        ]
    );
    let offered: usize = rows
        .iter()
        .map(|row| match row {
            PickerRow::Emoji(emoji) => emoji.len(),
            PickerRow::Title(_) => 0,
        })
        .sum();
    assert_eq!(offered, 2 * COLUMNS + catalog().len());
}

#[test]
fn every_row_fits_the_grid_and_a_search_has_no_titles() {
    for rows in [
        build_rows("", &suggestions(&[])),
        build_rows("face", &suggestions(&[])),
    ] {
        for row in &rows {
            if let PickerRow::Emoji(emoji) = row {
                assert!(!emoji.is_empty() && emoji.len() <= COLUMNS);
            }
        }
    }
    assert!(build_rows("face", &[])
        .iter()
        .all(|row| matches!(row, PickerRow::Emoji(_))));
}

#[test]
fn the_suggestions_lead_with_the_emoji_other_tabs_wear_each_once() {
    let in_use = ["🧪", "⭐", "not an emoji", "🧪"].map(String::from);
    let suggested = suggestions(&in_use);
    let suggested: Vec<&str> = suggested.iter().map(|emoji| emoji.as_str()).collect();
    assert_eq!(&suggested[..3], ["🧪", "⭐", "🔥"]);
    assert_eq!(suggested.len(), 2 * COLUMNS);
    let distinct: HashSet<&str> = suggested.iter().copied().collect();
    assert_eq!(distinct.len(), suggested.len());
}

/// Every emoji the picker offers is one a tab can wear and one macOS draws,
/// and every suggestion is in the catalog.
#[test]
fn every_emoji_the_picker_offers_is_one_a_tab_can_wear() {
    assert!(catalog().len() > 1_800);
    for emoji in catalog() {
        assert!(is_tag_emoji(emoji.as_str()), "{emoji:?}");
        assert!(emoji.emoji_version() <= NEWEST_EMOJI, "{emoji:?}");
    }
    for emoji in SUGGESTED {
        assert!(
            catalog().iter().any(|offered| offered.as_str() == emoji),
            "{emoji}"
        );
    }
}
