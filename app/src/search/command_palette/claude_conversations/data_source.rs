use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use warpui::{AppContext, Entity, ModelContext};

use crate::search::command_palette::claude_conversations::search_item::ClaudeSessionItem;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{DataSourceRunErrorWrapper, SyncDataSource};
use crate::terminal::cli_agent_sessions::history::{
    claude_projects_dir, list_transcript_paths, read_session_summary, transcript_mtime,
    ClaudeSession,
};

/// How long a loaded session snapshot is reused before re-reading
/// `~/.claude/projects` from disk. Short enough that a conversation you just
/// started or are actively in shows up the next time you open the palette,
/// long enough that a burst of keystrokes in one search reuses a single read.
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);

/// A transcript summarized at a known modification time. Reused as-is while the
/// file on disk is unchanged, so a refresh costs a directory walk plus one
/// `stat` per file instead of re-reading anything.
struct CachedSummary {
    mtime: Option<SystemTime>,
    session: ClaudeSession,
}

/// Datasource that lists past Claude Code sessions (from `~/.claude/projects`)
/// as command-palette entries. The session list is refreshed when the cached
/// snapshot is missing or older than [`SESSION_CACHE_TTL`], so sessions created
/// or updated after the app launched (including the one you're in right now)
/// appear without a restart. A refresh only re-reads transcripts whose mtime
/// changed.
pub struct DataSource {
    cache: Mutex<Option<(Instant, Arc<Vec<ClaudeSession>>)>>,
    /// Per-transcript summaries, keyed by path.
    summaries: Mutex<HashMap<PathBuf, CachedSummary>>,
}

impl DataSource {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            cache: Mutex::new(None),
            summaries: Mutex::new(HashMap::new()),
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
            *cache = Some((Instant::now(), Arc::new(self.load_sessions())));
        }
        Arc::clone(&cache.as_ref().expect("cache populated above").1)
    }

    /// Rebuilds the session list, re-summarizing only transcripts whose
    /// modification time changed since the last pass.
    ///
    /// This runs on the main thread (the command palette's sync data sources do
    /// not get their own thread), so it must stay cheap no matter how large the
    /// user's Claude history is.
    fn load_sessions(&self) -> Vec<ClaudeSession> {
        let Some(root) = claude_projects_dir().filter(|root| root.is_dir()) else {
            return Vec::new();
        };

        let paths: HashSet<PathBuf> = list_transcript_paths(&root).into_iter().collect();
        let mut summaries = self.summaries.lock().unwrap_or_else(|e| e.into_inner());
        let mut by_id: HashMap<String, ClaudeSession> = HashMap::with_capacity(paths.len());

        for path in &paths {
            let mtime = transcript_mtime(path);
            let reusable = summaries
                .get(path)
                .is_some_and(|cached| cached.mtime == mtime && mtime.is_some());
            if !reusable {
                match read_session_summary(path) {
                    Some(session) => {
                        summaries.insert(path.clone(), CachedSummary { mtime, session });
                    }
                    None => {
                        summaries.remove(path);
                        continue;
                    }
                }
            }
            if let Some(cached) = summaries.get(path) {
                if cached.session.first_prompt.is_none() {
                    continue;
                }
                // The same session id can appear in more than one project
                // directory; keep the most recently active copy.
                match by_id.get(&cached.session.session_id) {
                    Some(existing) if existing.last_activity >= cached.session.last_activity => {}
                    _ => {
                        by_id.insert(cached.session.session_id.clone(), cached.session.clone());
                    }
                }
            }
        }

        // Drop entries for transcripts that no longer exist.
        summaries.retain(|path, _| paths.contains(path));

        let mut sessions: Vec<ClaudeSession> = by_id.into_values().collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        sessions
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
