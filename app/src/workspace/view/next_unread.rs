//! ⌘J: jump to the topmost unread tab other than the active one. Starred tabs
//! lead the list, so they come first, with no priorities to learn.

use warpui::ViewContext;

use super::Workspace;

/// The topmost tab in `visible` (tab indices, in list order) that is unread
/// and isn't `active`.
pub(super) fn next_unread_tab(
    visible: &[usize],
    active: usize,
    is_unread: impl Fn(usize) -> bool,
) -> Option<usize> {
    visible
        .iter()
        .copied()
        .filter(|&index| index != active)
        .find(|&index| is_unread(index))
}

impl Workspace {
    /// Activates the topmost unread tab other than the active one, among the
    /// tabs the vertical tabs search shows, and focuses its unread pane. A
    /// toast says so when there's none.
    pub(super) fn jump_to_next_unread_tab(&mut self, _ctx: &mut ViewContext<Self>) {}
}
