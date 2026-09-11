//! Emoji tags, the fork's favorites. A tab wears up to three emoji before its
//! title, put on and taken off in the emoji picker that its row's hover
//! controls and its menu open (see `tab_emoji_picker`).
//!
//! With the "Float tagged tabs to the top" setting on, which it is by default,
//! a tagged tab floats. Tags ride on upstream's pinned tabs, so it sits in the
//! block at the top of the tab list and bulk closes leave it open (see
//! `starred_tabs`). It floats as its first emoji goes on and sinks as its last
//! comes off. A tab in a group stays with its group, whatever it wears. With
//! the setting off, emoji are labels and nothing moves. A floating tab with no
//! emoji of its own, as a tab starred before tags existed is, wears ⭐.

use warpui::elements::{
    Align, ConstrainedBox, Container, CrossAxisAlignment, Element, Flex, MainAxisSize,
    ParentElement, Shrinkable, Text,
};
use warpui::fonts::FamilyId;

use super::starred_tabs::starred_tabs_enabled;

/// The most emoji a tab wears.
pub(crate) const MAX_TAB_TAGS: usize = 3;

/// What a floating tab with no emoji of its own wears.
pub(crate) const STAR: &str = "⭐";

/// The emoji-presentation selector: ⭐ and ⭐️ are one emoji spelled two ways.
const EMOJI_PRESENTATION: char = '\u{FE0F}';

/// A tab's emoji, in the order they were put on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabTags(Vec<String>);

impl TabTags {
    /// ⭐ alone.
    pub(crate) fn star() -> Self {
        Self(vec![STAR.to_owned()])
    }

    /// The tags `stored` names, as a restore or a transfer brings them back:
    /// only single emoji, each once, the first three, in order.
    pub(crate) fn from_stored(stored: impl IntoIterator<Item = String>) -> Self {
        let mut tags = Self::default();
        for emoji in stored {
            if is_tag_emoji(&emoji) {
                tags.set(&emoji, true);
            }
        }
        tags
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the tab wears as many emoji as it can.
    pub(crate) fn is_full(&self) -> bool {
        self.0.len() >= MAX_TAB_TAGS
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub(crate) fn to_vec(&self) -> Vec<String> {
        self.0.clone()
    }

    /// Whether the tab wears `emoji`, however it's spelled.
    pub(crate) fn contains(&self, emoji: &str) -> bool {
        let key = tag_key(emoji);
        self.0.iter().any(|tag| tag_key(tag) == key)
    }

    /// Puts `emoji` on (`present`) or takes it off. A new emoji goes after the
    /// others, and nothing goes onto a full set; the rest keep their order when
    /// one comes off. Returns whether anything changed.
    pub(crate) fn set(&mut self, emoji: &str, present: bool) -> bool {
        let key = tag_key(emoji);
        let existing = self.0.iter().position(|tag| tag_key(tag) == key);
        match (existing, present) {
            (Some(index), false) => {
                self.0.remove(index);
                true
            }
            (None, true) if !key.is_empty() && !self.is_full() => {
                self.0.push(emoji.to_owned());
                true
            }
            _ => false,
        }
    }

    /// Takes every emoji off. Returns whether there were any.
    pub(crate) fn clear(&mut self) -> bool {
        let had_any = !self.0.is_empty();
        self.0.clear();
        had_any
    }
}

/// What two spellings of one emoji share: the emoji without its presentation
/// selector.
fn tag_key(emoji: &str) -> String {
    emoji.chars().filter(|c| *c != EMOJI_PRESENTATION).collect()
}

/// Whether `emoji` is one emoji, such as the picker offers.
pub(crate) fn is_tag_emoji(emoji: &str) -> bool {
    emojis::get(emoji).is_some()
}

/// What a tab wears: its emoji, or ⭐ when it floats with none of its own.
/// Nothing with tags off.
pub(crate) fn worn_tags(tags: &TabTags, pinned: bool) -> TabTags {
    if !starred_tabs_enabled() {
        TabTags::default()
    } else if tags.is_empty() && pinned {
        TabTags::star()
    } else {
        tags.clone()
    }
}

/// What a vertical tabs row wears for its tab: all of `worn` on the tab's
/// first row, unless the tab's Panes-layout header wears them, and nothing on
/// its other rows, so a split tab drawn one row per pane wears them once.
pub(super) fn row_tags(
    worn: &TabTags,
    is_first_row_of_tab: bool,
    header_wears_tags: bool,
) -> TabTags {
    if is_first_row_of_tab && !header_wears_tags {
        worn.clone()
    } else {
        TabTags::default()
    }
}

/// Whether a Panes-layout tab header wears its tab's emoji: the tab wears some
/// and the panel draws its header. The emoji belong to the tab, and the header
/// is the tab's own label, so they go there rather than on any one pane's row.
pub(super) fn header_shows_tags(worn: &TabTags, header_is_drawn: bool) -> bool {
    !worn.is_empty() && header_is_drawn
}

/// How big emoji are drawn: the font size, and the square each is centred in,
/// so a row's height never depends on the emoji font's line, which is taller
/// than the title's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TagSize {
    pub font_size: f32,
    pub box_size: f32,
}

/// Before a 12 px title.
pub(crate) const TITLE_TAG_SIZE: TagSize = TagSize {
    font_size: 11.,
    box_size: 15.,
};

/// Before a Panes-layout tab header's 10 px label.
pub(crate) const HEADER_TAG_SIZE: TagSize = TagSize {
    font_size: 9.,
    box_size: 12.,
};

/// Between two emoji of one tab.
const TAG_GAP: f32 = 1.;

/// Between a tab's emoji and the title they lead, the gap the unread dot keeps
/// from a title.
pub(crate) const TAGS_TITLE_GAP: f32 = 4.;

/// One emoji, centred in its box. It's only a mark, with no hover state and no
/// click of its own, so a click on a row's emoji lands on the row.
pub(crate) fn render_emoji(emoji: &str, size: TagSize, font_family: FamilyId) -> Box<dyn Element> {
    ConstrainedBox::new(
        Align::new(Text::new_inline(emoji.to_owned(), font_family, size.font_size).finish())
            .finish(),
    )
    .with_width(size.box_size)
    .with_height(size.box_size)
    .finish()
}

/// A tab's emoji, side by side in the order they were put on.
pub(crate) fn render_tags(
    tags: &TabTags,
    size: TagSize,
    font_family: FamilyId,
) -> Box<dyn Element> {
    let mut run = Flex::row()
        .with_main_axis_size(MainAxisSize::Min)
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_spacing(TAG_GAP);
    for emoji in tags.iter() {
        run.add_child(render_emoji(emoji, size, font_family));
    }
    run.finish()
}

/// `title` led by `tags`, the title keeping its own clipping in the width they
/// leave. Only the title's line: the lines under it start where they always
/// do, so every row's text lines up down the list however many emoji each
/// wears.
pub(crate) fn title_with_tags(
    title: Box<dyn Element>,
    tags: &TabTags,
    size: TagSize,
    font_family: FamilyId,
) -> Box<dyn Element> {
    if tags.is_empty() {
        return title;
    }
    Flex::row()
        .with_main_axis_size(MainAxisSize::Min)
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_child(
            Container::new(render_tags(tags, size, font_family))
                .with_margin_right(TAGS_TITLE_GAP)
                .finish(),
        )
        .with_child(Shrinkable::new(1., title).finish())
        .finish()
}

#[cfg(test)]
#[path = "tab_tags_tests.rs"]
mod tests;
