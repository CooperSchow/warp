//! Reads Claude Code's on-disk session store into a browsable, resume-able
//! conversation list (Feature: "Warp Conversations").
//!
//! Claude Code persists every session as `~/.claude/projects/<slug>/<id>.jsonl`,
//! one JSON object per line, and can natively reopen one with
//! `claude --resume <id>`. This module turns that store into a
//! [`ClaudeSession`] list so the UI can show past conversations and relaunch
//! them in a new tab. It reads Claude's files directly (no new persistence),
//! so it also surfaces sessions started before this feature existed.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

/// One browsable Claude Code conversation, keyed by its native session id
/// (the value passed to `claude --resume <id>`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClaudeSession {
    /// Native Claude session id; the resume target and the `.jsonl` file stem.
    pub session_id: String,
    /// Working directory the session ran in — used to reopen the tab in place.
    pub cwd: Option<String>,
    /// Fallback project label derived from the on-disk directory name.
    pub project_dir: String,
    /// Claude's own generated title (`ai-title` line), when present.
    pub title: Option<String>,
    /// First real user prompt — the title fallback when there is no `ai-title`.
    pub first_prompt: Option<String>,
    /// Most recent activity timestamp (ISO-8601; sorts lexically).
    pub last_activity: Option<String>,
    /// Earliest activity timestamp (ISO-8601) — when the session began.
    pub first_activity: Option<String>,
    /// Count of the human's own (non-sidechain) user + assistant turns. Sidechain
    /// (subagent/Task) turns are excluded so a session's weight reflects the real
    /// conversation rather than the subagents it spawned.
    pub message_count: u32,
    /// Distinct models seen across the (non-sidechain) transcript.
    pub models: BTreeSet<String>,
}

impl ClaudeSession {
    /// The label shown in the conversation list.
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.first_prompt.clone())
            .unwrap_or_else(|| "(untitled session)".to_string())
    }

    /// The shell command that reopens this conversation in a new tab.
    pub fn resume_command(&self) -> String {
        format!("claude --resume {}", self.session_id)
    }

    /// Short project label for grouping/display (last path component of the cwd).
    pub fn project_label(&self) -> String {
        let raw = self.cwd.clone().unwrap_or_else(|| self.project_dir.clone());
        raw.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&raw)
            .to_string()
    }

    /// Compact "MM-DD HH:MM" of the most recent activity, for the list.
    pub fn last_activity_label(&self) -> Option<String> {
        short_stamp(self.last_activity.as_deref())
    }

    /// Compact "MM-DD HH:MM" of when the session began.
    pub fn first_activity_label(&self) -> Option<String> {
        short_stamp(self.first_activity.as_deref())
    }
}

/// Turn an ISO-8601 timestamp like `2026-07-16T02:43:…` into a compact
/// `07-16 02:43`. Returns `None` if the string is too short to slice.
fn short_stamp(iso: Option<&str>) -> Option<String> {
    iso.and_then(|s| s.get(5..16)).map(|d| d.replace('T', " "))
}

/// Extract plain text from a `message.content` field (string or content-block array).
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => String::new(),
    }
}

fn first_prompt_snippet(text: &str) -> Option<String> {
    let text = text.trim();
    // Skip empty and tool-result / system-injected (`<...>`) openers.
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(text.chars().take(200).collect::<String>().replace('\n', " "))
}

/// Fold one transcript line into the accumulating session.
fn ingest_line(session: &mut ClaudeSession, line: &str) {
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or_default();

    if session.cwd.is_none() {
        if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
            session.cwd = Some(cwd.to_string());
        }
    }
    if let Some(ts) = obj.get("timestamp").and_then(Value::as_str) {
        if session.last_activity.as_deref().is_none_or(|cur| ts > cur) {
            session.last_activity = Some(ts.to_string());
        }
        if session.first_activity.as_deref().is_none_or(|cur| ts < cur) {
            session.first_activity = Some(ts.to_string());
        }
    }
    if ty == "ai-title" {
        if let Some(title) = obj.get("aiTitle").and_then(Value::as_str) {
            session.title = Some(title.to_string());
        }
    }

    if ty == "user" || ty == "assistant" {
        // Subagent (Task) turns are marked `isSidechain: true` and are
        // interleaved into their parent session — exclude them from the human
        // turn count and never let one supply the title.
        let is_sidechain = obj
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_sidechain {
            return;
        }
        session.message_count += 1;
        let message = obj.get("message");
        if let Some(model) = message.and_then(|m| m.get("model")).and_then(Value::as_str) {
            session.models.insert(model.to_string());
        }
        if ty == "user" && session.first_prompt.is_none() {
            if let Some(content) = message.and_then(|m| m.get("content")) {
                if let Some(snippet) = first_prompt_snippet(&text_of(content)) {
                    session.first_prompt = Some(snippet);
                }
            }
        }
    }
}

/// Parse a single `.jsonl` file's contents into a [`ClaudeSession`].
fn parse_session(session_id: String, project_dir: String, lines: impl Iterator<Item = String>) -> ClaudeSession {
    let mut session = ClaudeSession {
        session_id,
        project_dir,
        ..Default::default()
    };
    for line in lines {
        ingest_line(&mut session, &line);
    }
    session
}

/// Resolve the Claude projects directory (`$CLAUDE_CONFIG_DIR/projects` or
/// `~/.claude/projects`). Mirrors the CLI's own config-dir resolution.
pub fn claude_projects_dir() -> Option<PathBuf> {
    let base = match env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        Ok(_) | Err(_) => dirs::home_dir()?.join(".claude"),
    };
    Some(base.join("projects"))
}

/// Read every resume-able Claude Code session under `projects_root`, newest first.
///
/// Skips `agent-*` files (subagent Task transcripts, not top-level resumable
/// sessions) and sessions with fewer than two human turns (empty/metadata-only).
pub fn read_claude_sessions(projects_root: &Path) -> Vec<ClaudeSession> {
    let mut sessions: HashMap<String, ClaudeSession> = HashMap::new();

    for entry in WalkDir::new(projects_root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        // `agent-*` files are subagent/Task transcripts, not top-level resumable
        // sessions — skip them outright.
        if session_id.starts_with("agent-") {
            continue;
        }
        let project_dir = path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let Ok(file) = File::open(path) else {
            continue;
        };
        let lines = BufReader::new(file)
            .lines()
            .map_while(Result::ok);
        let parsed = parse_session(session_id.clone(), project_dir, lines);

        // A session can span multiple files (rare); merge by id, keeping the
        // richest fields.
        match sessions.get_mut(&session_id) {
            Some(existing) => merge_session(existing, parsed),
            None => {
                sessions.insert(session_id, parsed);
            }
        }
    }

    let mut resumable: Vec<ClaudeSession> = sessions
        .into_values()
        .filter(|s| s.message_count >= 2)
        .collect();
    // Newest activity first; None sorts last.
    resumable.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    resumable
}

fn merge_session(into: &mut ClaudeSession, other: ClaudeSession) {
    into.message_count += other.message_count;
    into.models.extend(other.models);
    if into.cwd.is_none() {
        into.cwd = other.cwd;
    }
    if into.title.is_none() {
        into.title = other.title;
    }
    if into.first_prompt.is_none() {
        into.first_prompt = other.first_prompt;
    }
    if other.last_activity > into.last_activity {
        into.last_activity = other.last_activity;
    }
    match (&into.first_activity, &other.first_activity) {
        (None, _) => into.first_activity = other.first_activity,
        (Some(cur), Some(o)) if o < cur => into.first_activity = other.first_activity,
        _ => {}
    }
}

/// Convenience: resolve the projects dir and read all sessions. Empty if the
/// Claude store can't be located or doesn't exist.
pub fn load_claude_sessions() -> Vec<ClaudeSession> {
    match claude_projects_dir() {
        Some(root) if root.is_dir() => read_claude_sessions(&root),
        Some(_) | None => Vec::new(),
    }
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
