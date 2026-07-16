use std::sync::OnceLock;

use warpui::{AppContext, Entity, ModelContext};

use crate::search::command_palette::claude_conversations::search_item::ClaudeSessionItem;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{DataSourceRunErrorWrapper, SyncDataSource};
use crate::terminal::cli_agent_sessions::history::{load_claude_sessions, ClaudeSession};

/// Datasource that lists past Claude Code sessions (from `~/.claude/projects`)
/// as command-palette entries. The session list is read lazily on first query
/// and cached for the lifetime of the process (a snapshot; new sessions started
/// after the palette first opens won't appear until restart — acceptable for a
/// "reopen an old conversation" flow).
pub struct DataSource {
    sessions: OnceLock<Vec<ClaudeSession>>,
}

impl DataSource {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            sessions: OnceLock::new(),
        }
    }
}

impl SyncDataSource for DataSource {
    type Action = CommandPaletteItemAction;

    fn run_query(
        &self,
        query: &Query,
        _app: &AppContext,
    ) -> Result<Vec<QueryResult<Self::Action>>, DataSourceRunErrorWrapper> {
        let needle = query.text.trim().to_lowercase();
        let sessions = self.sessions.get_or_init(load_claude_sessions);

        // Sessions are already newest-first; score by position so the mixer
        // preserves recency order (newest = highest score).
        let results = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                needle.is_empty()
                    || session.display_title().to_lowercase().contains(&needle)
                    || session
                        .cwd
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
            })
            .map(|(index, session)| {
                QueryResult::from(ClaudeSessionItem::new(session.clone(), -(index as f64)))
            })
            .collect();
        Ok(results)
    }
}

impl Entity for DataSource {
    type Event = ();
}
