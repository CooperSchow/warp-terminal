use ordered_float::OrderedFloat;
use warpui::elements::{Flex, ParentElement, Text};
use warpui::fonts::{Properties, Weight};
use warpui::{AppContext, Element, SingletonEntity};

use crate::appearance::Appearance;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::command_palette::render_util::render_search_item_icon;
use crate::search::result_renderer::ItemHighlightState;
use crate::terminal::cli_agent_sessions::history::codex::CodexSession;
use crate::ui_components::icons::Icon;

/// A single past Codex CLI session rendered in the command palette.
#[derive(Debug)]
pub struct CodexSessionItem {
    session: CodexSession,
    /// Higher sorts earlier; we bake recency in as `-index`.
    score: f64,
}

impl CodexSessionItem {
    pub fn new(session: CodexSession, score: f64) -> Self {
        Self { session, score }
    }
}

impl crate::search::item::SearchItem for CodexSessionItem {
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
        // The provider's own logo rather than the Claude list's clock, so the
        // two session sources are told apart at a glance when both are showing.
        render_search_item_icon(appearance, Icon::OpenAILogo, color, highlight_state)
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
        // Prefix the working directory with when the session started, e.g.
        // "09-11 21:21 · /Users/me/project".
        let subtitle_text = match self.session.started_at_label() {
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
        CommandPaletteItemAction::OpenAgentSession {
            cwd: self.session.cwd.clone().unwrap_or_default(),
            resume_command: self.session.resume_command(),
        }
    }

    fn execute_result(&self) -> Self::Action {
        self.accept_result()
    }

    fn accessibility_label(&self) -> String {
        format!("Codex conversation: {}", self.session.display_title())
    }

    fn accessibility_help_message(&self) -> Option<String> {
        Some("Press enter to reopen this Codex session in a new tab.".into())
    }
}
