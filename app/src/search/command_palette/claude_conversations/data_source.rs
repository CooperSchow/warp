use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use warpui::{AppContext, Entity, ModelContext};

use crate::search::command_palette::claude_conversations::search_item::ClaudeSessionItem;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{DataSourceRunErrorWrapper, SyncDataSource};
use crate::terminal::cli_agent_sessions::history::{load_claude_sessions, ClaudeSession};

/// How long a loaded session snapshot is reused before re-reading
/// `~/.claude/projects` from disk. Short enough that a conversation you just
/// started or are actively in shows up the next time you open the palette,
/// long enough that a burst of keystrokes in one search reuses a single read.
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);

/// Datasource that lists past Claude Code sessions (from `~/.claude/projects`)
/// as command-palette entries. The session list is re-read from disk when the
/// cached snapshot is missing or older than [`SESSION_CACHE_TTL`], so sessions
/// created or updated after the app launched (including the one you're in right
/// now) appear without a restart.
pub struct DataSource {
    cache: Mutex<Option<(Instant, Arc<Vec<ClaudeSession>>)>>,
}

impl DataSource {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }

    /// Returns the session list, re-reading from disk if the cached snapshot is
    /// missing or has expired.
    fn sessions(&self) -> Arc<Vec<ClaudeSession>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let is_fresh = cache
            .as_ref()
            .is_some_and(|(loaded_at, _)| loaded_at.elapsed() < SESSION_CACHE_TTL);
        if !is_fresh {
            *cache = Some((Instant::now(), Arc::new(load_claude_sessions())));
        }
        Arc::clone(&cache.as_ref().expect("cache populated above").1)
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
        let sessions = self.sessions();

        // Sessions are already newest-first; score by position so the mixer
        // preserves recency order (newest = highest score).
        let results = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                needle.is_empty()
                    || session.display_title().to_lowercase().contains(&needle)
                    // Also match the first user prompt so content keywords
                    // (client names, etc.) find a session even when its title
                    // is generic.
                    || session
                        .first_prompt
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
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
