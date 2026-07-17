use ordered_float::OrderedFloat;
use warpui::elements::{Flex, ParentElement, Text};
use warpui::fonts::{Properties, Weight};
use warpui::{AppContext, Element, SingletonEntity};

use crate::appearance::Appearance;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::command_palette::render_util::render_search_item_icon;
use crate::search::result_renderer::ItemHighlightState;
use crate::terminal::cli_agent_sessions::history::ClaudeSession;
use crate::ui_components::icons::Icon;

/// A single past Claude Code session rendered in the command palette.
#[derive(Debug)]
pub struct ClaudeSessionItem {
    session: ClaudeSession,
    /// Higher sorts earlier; we bake recency in as `-index`.
    score: f64,
}

impl ClaudeSessionItem {
    pub fn new(session: ClaudeSession, score: f64) -> Self {
        Self { session, score }
    }
}

impl crate::search::item::SearchItem for ClaudeSessionItem {
    type Action = CommandPaletteItemAction;

    fn is_multiline(&self) -> bool {
        true
    }

    fn render_icon(
        &self,
        highlight_state: ItemHighlightState,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        let color = appearance.theme().foreground().into_solid();
        render_search_item_icon(appearance, Icon::ClockRewind, color, highlight_state)
    }

    fn render_item(
        &self,
        highlight_state: ItemHighlightState,
        app: &AppContext,
    ) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);

        let title = Text::new_inline(
            self.session.display_title(),
            appearance.ui_font_family(),
            appearance.monospace_font_size(),
        )
        .with_color(highlight_state.sub_text_fill(appearance).into_solid())
        .with_style(Properties::default().weight(Weight::Bold));

        let location = self
            .session
            .cwd
            .clone()
            .unwrap_or_else(|| self.session.project_label());
        // Prefix the working directory with the last-activity date, e.g.
        // "07-16 02:43 · /Users/me/project".
        let subtitle_text = match self.session.last_activity_label() {
            Some(when) => format!("{when} · {location}"),
            None => location,
        };
        let subtitle = Text::new_inline(
            subtitle_text,
            appearance.ui_font_family(),
            appearance.monospace_font_size() - 2.,
        )
        .with_color(highlight_state.sub_text_fill(appearance).into_solid());

        Flex::column()
            .with_child(title.finish())
            .with_child(subtitle.finish())
            .with_spacing(4.)
            .finish()
    }

    fn score(&self) -> OrderedFloat<f64> {
        OrderedFloat::from(self.score)
    }

    fn accept_result(&self) -> Self::Action {
        CommandPaletteItemAction::OpenClaudeSession {
            cwd: self.session.cwd.clone().unwrap_or_default(),
            resume_command: self.session.resume_command(),
        }
    }

    fn execute_result(&self) -> Self::Action {
        self.accept_result()
    }

    fn accessibility_label(&self) -> String {
        format!("Claude conversation: {}", self.session.display_title())
    }

    fn accessibility_help_message(&self) -> Option<String> {
        Some("Press enter to reopen this Claude session in a new tab.".into())
    }
}
