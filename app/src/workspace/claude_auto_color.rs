//! Keyword matching for automatically coloring tabs from the content of the
//! Claude Code conversation running in them.
//!
//! Pure logic only — the orchestration (watching CLI agent sessions, locating
//! transcripts, applying the color) lives in `workspace::view`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::tab_settings::ClaudeAutoColorRule;
use crate::terminal::cli_agent_sessions::history::{
    first_user_prompt_in_file, session_file_path, transcripts_for_cwd_created_after,
    CLAUDE_CONTINUE_SENTINEL,
};

/// How much of a first prompt we consider for keyword matching. Prompts can be
/// arbitrarily large (pasted logs etc.); keywords that identify a client or
/// project appear early in practice.
pub(crate) const MATCH_TEXT_MAX_BYTES: usize = 16 * 1024;

/// Returns the best-matching rule for `text`, or `None` when no rule has at
/// least one keyword hit.
///
/// Scoring: number of distinct matched keywords per rule; the highest score
/// wins and ties go to the earliest rule in the list. Matching is
/// case-insensitive. Keywords made of a single alphanumeric word must match on
/// word boundaries (so "IRE" doesn't hit "hired"); anything else (phrases,
/// hyphenated terms) matches as a plain substring.
pub(crate) fn best_rule_match<'a>(
    rules: &'a [ClaudeAutoColorRule],
    text: &str,
) -> Option<&'a ClaudeAutoColorRule> {
    let haystack = truncated_lowercase(text);

    let mut best: Option<(&ClaudeAutoColorRule, usize)> = None;
    for rule in rules {
        let score = rule_score(rule, &haystack);
        if score == 0 {
            continue;
        }
        match best {
            Some((_, best_score)) if best_score >= score => {}
            _ => best = Some((rule, score)),
        }
    }
    best.map(|(rule, _)| rule)
}

/// Number of distinct keywords of `rule` found in `haystack` (which must
/// already be lowercase).
fn rule_score(rule: &ClaudeAutoColorRule, haystack: &str) -> usize {
    rule.keywords
        .split(',')
        .map(normalized_keyword)
        .filter(|k| !k.is_empty())
        .filter(|keyword| keyword_matches(keyword, haystack))
        .count()
}

/// Trims one comma-separated keyword down to what should actually be searched
/// for: surrounding whitespace and quotes removed, lowercased.
///
/// Quoting is the natural way to write a multi-word keyword — `"Inside Real
/// Estate", "IRE"` — so the quote characters must not end up as part of the
/// needle. Without this, such a rule silently never matches anything.
fn normalized_keyword(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '\u{201c}' | '\u{201d}' | '\u{2018}' | '\u{2019}'))
        .trim()
        .to_lowercase()
}

/// True if `needle` (lowercase) occurs in `haystack` (lowercase), respecting
/// word boundaries for purely-alphanumeric needles.
fn keyword_matches(needle: &str, haystack: &str) -> bool {
    let word_bounded = needle.chars().all(|c| c.is_alphanumeric());
    let mut search_start = 0;
    while let Some(found) = haystack[search_start..].find(needle) {
        let start = search_start + found;
        let end = start + needle.len();
        if !word_bounded {
            return true;
        }
        let boundary_before = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let boundary_after = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if boundary_before && boundary_after {
            return true;
        }
        // Advance by at least one char to make progress on overlapping hits.
        search_start = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
        if search_start >= haystack.len() {
            break;
        }
    }
    false
}

/// Lowercases `text`, considering at most the first [`MATCH_TEXT_MAX_BYTES`]
/// bytes (cut at a char boundary).
fn truncated_lowercase(text: &str) -> String {
    let mut end = text.len().min(MATCH_TEXT_MAX_BYTES);
    while end < text.len() && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_lowercase()
}

/// Per-terminal-view retry state while we wait for a Claude conversation's
/// first prompt to appear on disk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClaudeAutoColorPending {
    /// When we first saw the Claude session in this pane — used to scope the
    /// cwd-based transcript search to files written by *this* conversation.
    pub started_at: SystemTime,
    /// Completed (unsuccessful) evaluation attempts so far.
    pub attempts: u32,
    /// True while a background transcript read is running, so event-driven
    /// triggers don't stack up concurrent reads.
    pub in_flight: bool,
}

impl ClaudeAutoColorPending {
    pub fn new(started_at: SystemTime) -> Self {
        Self {
            started_at,
            attempts: 0,
            in_flight: false,
        }
    }
}

/// Delay before retry number `attempts + 1`, or `None` to give up. Quick
/// checks while the user is likely typing their first prompt, then a slow
/// tail, capped so an idle claude prompt doesn't get polled forever.
pub(crate) fn next_retry_delay(attempts: u32) -> Option<Duration> {
    match attempts {
        // First minute: every 3s (the common case: prompt sent shortly after launch).
        0..=19 => Some(Duration::from_secs(3)),
        // Next ~10 minutes: every 15s.
        20..=59 => Some(Duration::from_secs(15)),
        // Next ~10 minutes: every 30s, then stop.
        60..=79 => Some(Duration::from_secs(30)),
        _ => None,
    }
}

/// Grace subtracted from the session start when scoping the cwd transcript
/// search, to absorb clock/timestamp granularity and the transcript being
/// created moments before we noticed the session.
///
/// Deliberately small: it is what keeps two Claude sessions started
/// back-to-back in the same directory from being confused for one another. A
/// wide grace would pull the older pane's transcript into the newer pane's
/// candidate set.
const SESSION_START_GRACE: Duration = Duration::from_secs(10);

/// Locates the transcript for a just-started Claude conversation and returns
/// the full text of its first real user prompt, if it exists yet.
///
/// Resolution order mirrors how confident we are in each signal:
/// 1. `transcript_path` reported by the Warp Claude plugin (exact),
/// 2. `resume_id` (transcript stem / plugin session id, exact when present),
/// 3. the first transcript created in the pane's cwd project dir since the
///    session started (fallback for the common plugin-absent case).
///
/// Runs blocking file IO — call from a background task, not the main thread.
pub(crate) fn locate_first_prompt(
    transcript_path: Option<String>,
    resume_id: Option<String>,
    cwd: Option<String>,
    session_started_at: SystemTime,
) -> Option<String> {
    if let Some(path) = transcript_path {
        if let Some(prompt) = first_user_prompt_in_file(Path::new(&path)) {
            return Some(prompt);
        }
    }

    if let Some(id) = resume_id.filter(|id| id != CLAUDE_CONTINUE_SENTINEL) {
        if let Some(path) = session_file_path(&id) {
            if let Some(prompt) = first_user_prompt_in_file(&path) {
                return Some(prompt);
            }
        }
    }

    if let Some(cwd) = cwd {
        let since = session_started_at
            .checked_sub(SESSION_START_GRACE)
            .unwrap_or(session_started_at);
        for path in transcripts_for_cwd_created_after(&cwd, since) {
            if let Some(prompt) = first_user_prompt_in_file(&path) {
                return Some(prompt);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, keywords: &str) -> ClaudeAutoColorRule {
        ClaudeAutoColorRule {
            name: name.to_string(),
            keywords: keywords.to_string(),
            color: "#502fef".to_string(),
        }
    }

    #[test]
    fn no_rules_no_match() {
        assert!(best_rule_match(&[], "anything").is_none());
    }

    #[test]
    fn simple_keyword_matches_case_insensitively() {
        let rules = [rule("IRE", "ire, inside real estate")];
        assert_eq!(
            best_rule_match(&rules, "Weekly KPI report for IRE expired leads")
                .map(|r| r.name.as_str()),
            Some("IRE")
        );
    }

    #[test]
    fn word_boundaries_prevent_substring_false_positives() {
        let rules = [rule("IRE", "ire")];
        assert!(best_rule_match(&rules, "we hired a contractor").is_none());
        assert!(best_rule_match(&rules, "the IRE campaign").is_some());
        assert!(best_rule_match(&rules, "ire!").is_some());
        assert!(best_rule_match(&rules, "(ire)").is_some());
    }

    #[test]
    fn phrases_match_as_substrings() {
        let rules = [rule("NWC", "member care,nwc")];
        assert!(best_rule_match(&rules, "transfer to Member Care queue").is_some());
    }

    #[test]
    fn most_keyword_hits_wins() {
        let rules = [
            rule("Generic", "portal, supabase"),
            rule("SIA", "sia, portal, supabase"),
        ];
        assert_eq!(
            best_rule_match(&rules, "the SIA PD portal on supabase")
                .map(|r| r.name.as_str()),
            Some("SIA")
        );
    }

    #[test]
    fn ties_go_to_the_earliest_rule() {
        let rules = [rule("First", "warp"), rule("Second", "warp")];
        assert_eq!(
            best_rule_match(&rules, "warp fork work").map(|r| r.name.as_str()),
            Some("First")
        );
    }

    #[test]
    fn empty_and_whitespace_keywords_are_ignored() {
        let rules = [rule("Empty", " , ,  ")];
        assert!(best_rule_match(&rules, "anything at all").is_none());
    }

    #[test]
    fn duplicate_keyword_hits_count_once() {
        // "warp warp warp" should score 1 for a single-keyword rule, so a
        // two-distinct-hit rule beats it even when listed later.
        let rules = [rule("Spam", "warp"), rule("Precise", "warp, terminal")];
        assert_eq!(
            best_rule_match(&rules, "warp warp warp terminal").map(|r| r.name.as_str()),
            Some("Precise")
        );
    }

    #[test]
    fn multibyte_text_is_handled() {
        let rules = [rule("Emoji", "déploiement")];
        assert!(best_rule_match(&rules, "🚀 le déploiement est prêt 🚀").is_some());
    }

    #[test]
    fn retry_schedule_terminates_and_covers_a_reasonable_window() {
        let mut total = Duration::ZERO;
        let mut attempts = 0;
        while let Some(delay) = next_retry_delay(attempts) {
            total += delay;
            attempts += 1;
            assert!(attempts < 1000, "schedule must terminate");
        }
        // Long enough to catch a prompt sent a few minutes after launch,
        // bounded so idle prompts aren't polled forever.
        assert!(total >= Duration::from_secs(10 * 60));
        assert!(total <= Duration::from_secs(45 * 60));
    }

    #[test]
    fn quoted_keywords_match_without_their_quotes() {
        // Quoting is a natural way to write a keyword list, especially one with
        // multi-word entries. Regression: the quotes used to be searched for
        // literally, so a rule written this way never matched anything.
        let rules = [rule("NWC", r#""Nutrition Wellness Center", "NWC""#)];
        assert!(best_rule_match(&rules, "NWC").is_some());
        assert!(best_rule_match(&rules, "checking the Nutrition Wellness Center flow").is_some());
        assert!(best_rule_match(&rules, "nothing relevant here").is_none());
    }

    #[test]
    fn quote_stripping_keeps_word_boundary_matching() {
        // Stripping quotes must leave a bare word bare, so it still matches on
        // word boundaries rather than degrading into a substring match.
        let rules = [rule("IRE", r#""IRE""#)];
        assert!(best_rule_match(&rules, "we hired a contractor").is_none());
        assert!(best_rule_match(&rules, "the IRE campaign").is_some());
    }

    #[test]
    fn stray_and_smart_quotes_are_tolerated() {
        let rules = [rule("Odd", "\u{201c}nwc\u{201d}, \"ire")];
        assert!(best_rule_match(&rules, "an NWC question").is_some());
        assert!(best_rule_match(&rules, "an IRE question").is_some());
    }

    /// Runs the real resolution path against the local `~/.claude` store: takes
    /// the most recently created transcript, pretends a pane's Claude session
    /// started a moment before it, and checks that we come back with *that*
    /// conversation's first prompt rather than whichever file in the directory
    /// happens to have been written to most recently.
    ///
    /// Ignored by default (it depends on the machine's own Claude history); run
    /// with `cargo test -p warp --lib --features gui -- --ignored
    /// resolves_the_pane_own_conversation_in_the_real_store --nocapture`.
    #[test]
    #[ignore = "reads the local ~/.claude store"]
    fn resolves_the_pane_own_conversation_in_the_real_store() {
        use crate::terminal::cli_agent_sessions::history::{
            claude_projects_dir, first_user_prompt_in_file,
        };

        let Some(root) = claude_projects_dir().filter(|r| r.is_dir()) else {
            println!("no ~/.claude/projects; skipping");
            return;
        };

        // Newest-created transcript anywhere in the store, and the cwd it ran in.
        let newest = std::fs::read_dir(&root)
            .expect("read projects root")
            .flatten()
            .flat_map(|project| std::fs::read_dir(project.path()).into_iter().flatten())
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .filter_map(|entry| {
                let created = entry.metadata().ok()?.created().ok()?;
                Some((created, entry.path()))
            })
            .max_by_key(|(created, _)| *created);
        let Some((created, path)) = newest else {
            println!("store is empty; skipping");
            return;
        };

        let expected = first_user_prompt_in_file(&path);
        let cwd = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| {
                body.lines().find_map(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()?
                        .get("cwd")?
                        .as_str()
                        .map(str::to_string)
                })
            })
            .expect("newest transcript records a cwd");

        let started_at = created - Duration::from_secs(1);
        let found = locate_first_prompt(None, None, Some(cwd.clone()), started_at);

        println!("cwd:      {cwd}");
        println!("expected: {:?}", expected.as_deref().map(|p| &p[..p.len().min(60)]));
        println!("found:    {:?}", found.as_deref().map(|p| &p[..p.len().min(60)]));
        assert_eq!(
            found, expected,
            "must resolve the pane's own conversation, not the busiest one in the directory"
        );
    }

    #[test]
    fn keywords_beyond_truncation_do_not_match() {
        let rules = [rule("Tail", "zzyzx")];
        let text = format!("{}zzyzx", "a".repeat(MATCH_TEXT_MAX_BYTES));
        assert!(best_rule_match(&rules, &text).is_none());
    }
}
