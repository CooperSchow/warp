use super::{parse_session, ClaudeSession};

fn parse(lines: &[&str]) -> ClaudeSession {
    parse_session(
        "sess-1".to_string(),
        "my-project".to_string(),
        lines.iter().map(|l| l.to_string()),
    )
}

#[test]
fn captures_title_cwd_models_and_counts_human_turns() {
    let session = parse(&[
        r#"{"type":"user","cwd":"/home/me/proj","timestamp":"2026-07-15T01:00:00Z","message":{"role":"user","content":"Hello there"}}"#,
        r#"{"type":"assistant","timestamp":"2026-07-15T01:00:05Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"Hi"}]}}"#,
        r#"{"type":"ai-title","aiTitle":"My Title","sessionId":"sess-1"}"#,
    ]);

    assert_eq!(session.title.as_deref(), Some("My Title"));
    assert_eq!(session.display_title(), "My Title");
    assert_eq!(session.message_count, 2);
    assert_eq!(session.cwd.as_deref(), Some("/home/me/proj"));
    assert_eq!(session.last_activity.as_deref(), Some("2026-07-15T01:00:05Z"));
    assert_eq!(session.first_activity.as_deref(), Some("2026-07-15T01:00:00Z"));
    assert_eq!(session.last_activity_label(), Some("07-15 01:00".to_string()));
    assert_eq!(session.first_activity_label(), Some("07-15 01:00".to_string()));
    assert!(session.models.contains("claude-opus-4-8"));
    assert_eq!(session.project_label(), "proj");
    assert_eq!(session.resume_command(), "claude --resume sess-1");
}

#[test]
fn excludes_sidechain_turns_and_falls_back_to_first_prompt() {
    let session = parse(&[
        r#"{"type":"user","isSidechain":true,"message":{"content":"You are a subagent prompt"}}"#,
        r#"{"type":"user","message":{"content":"Real first prompt"}}"#,
        r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent reply"}]}}"#,
        r#"{"type":"assistant","message":{"model":"claude-fable-5","content":[{"type":"text","text":"ok"}]}}"#,
    ]);

    // No ai-title -> title falls back to the first non-sidechain user prompt.
    assert_eq!(session.title, None);
    assert_eq!(session.first_prompt.as_deref(), Some("Real first prompt"));
    assert_eq!(session.display_title(), "Real first prompt");
    // Only the two non-sidechain turns are counted.
    assert_eq!(session.message_count, 2);
    // The sidechain prompt never leaks into the title.
    assert!(session.models.contains("claude-fable-5"));
}

#[test]
fn skips_tool_result_openers_when_choosing_first_prompt() {
    let session = parse(&[
        r#"{"type":"user","message":{"content":"<tool_use_result>stuff</tool_use_result>"}}"#,
        r#"{"type":"user","message":{"content":"Actual question"}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"answer"}]}}"#,
    ]);

    assert_eq!(session.first_prompt.as_deref(), Some("Actual question"));
}

#[test]
fn untitled_session_has_placeholder_title() {
    let session = parse(&[
        r#"{"type":"user","message":{"content":"<tool_use_result>only tool output</tool_use_result>"}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
    ]);

    assert_eq!(session.title, None);
    assert_eq!(session.first_prompt, None);
    assert_eq!(session.display_title(), "(untitled session)");
}

#[test]
fn unparseable_lines_are_skipped() {
    let session = parse(&[
        "not json at all",
        r#"{"type":"user","message":{"content":"kept"}}"#,
        "",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"kept2"}]}}"#,
    ]);

    assert_eq!(session.message_count, 2);
    assert_eq!(session.first_prompt.as_deref(), Some("kept"));
}
