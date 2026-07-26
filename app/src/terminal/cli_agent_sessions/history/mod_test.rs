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

mod first_user_prompt_in_file {
    use std::io::Write as _;

    use super::super::first_user_prompt_in_file;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file
    }

    #[test]
    fn returns_full_untruncated_prompt() {
        let long_tail = "keyword-at-the-very-end".to_string();
        let prompt = format!("{} {}", "x".repeat(500), long_tail);
        let file = write_transcript(&[
            r#"{"type":"last-prompt","sessionId":"s"}"#,
            &format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{prompt}"}}}}"#
            ),
        ]);

        let read = first_user_prompt_in_file(file.path()).expect("prompt");
        assert!(read.len() > 200, "must not be truncated to the 200-char snippet");
        assert!(read.ends_with(&long_tail));
    }

    #[test]
    fn skips_meta_sidechain_and_system_injected_turns() {
        let file = write_transcript(&[
            r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>Caveat</local-command-caveat>"}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"subagent prompt"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/model</command-name>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"the real prompt"}]}}"#,
        ]);

        assert_eq!(
            first_user_prompt_in_file(file.path()).as_deref(),
            Some("the real prompt")
        );
    }

    #[test]
    fn returns_none_when_no_real_prompt_yet() {
        let file = write_transcript(&[
            r#"{"type":"mode","mode":"normal"}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"<caveat>x</caveat>"}}"#,
        ]);

        assert_eq!(first_user_prompt_in_file(file.path()), None);
    }

    #[test]
    fn returns_none_for_missing_file() {
        assert_eq!(
            first_user_prompt_in_file(std::path::Path::new("/nonexistent/x.jsonl")),
            None
        );
    }
}

mod bounded_reads {
    use std::io::Write as _;

    use super::super::{read_session_summary, HEAD_SCAN_BYTES};

    /// A transcript far larger than the scan window must still be summarized,
    /// and must not be read in full — this is what keeps the command palette
    /// off a multi-second main-thread stall on a large Claude history.
    #[test]
    fn summarizes_a_huge_transcript_from_its_ends() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("huge.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");

        writeln!(
            file,
            r#"{{"type":"user","cwd":"/tmp/proj","timestamp":"2026-07-01T00:00:00Z","message":{{"role":"user","content":"the first prompt"}}}}"#
        )
        .expect("write head");

        // Bulk filler between the head and tail windows.
        let filler = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(4096)
        );
        let mut written = 0u64;
        while written < HEAD_SCAN_BYTES * 4 {
            writeln!(file, "{filler}").expect("write filler");
            written += filler.len() as u64 + 1;
        }

        writeln!(
            file,
            r#"{{"type":"ai-title","aiTitle":"Generated Title","sessionId":"huge"}}"#
        )
        .expect("write tail");
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2026-07-09T12:00:00Z","message":{{"role":"user","content":"latest turn"}}}}"#
        )
        .expect("write tail turn");
        drop(file);

        let session = read_session_summary(&path).expect("summary");
        assert_eq!(session.session_id, "huge");
        assert_eq!(session.first_prompt.as_deref(), Some("the first prompt"));
        assert_eq!(session.cwd.as_deref(), Some("/tmp/proj"));
        // Title comes from the tail window.
        assert_eq!(session.title.as_deref(), Some("Generated Title"));
        assert_eq!(session.display_title(), "Generated Title");
        // Timestamps come from both ends.
        assert_eq!(
            session.first_activity.as_deref(),
            Some("2026-07-01T00:00:00Z")
        );
        assert_eq!(
            session.last_activity.as_deref(),
            Some("2026-07-09T12:00:00Z")
        );
    }

    #[test]
    fn small_transcripts_are_read_whole() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("small.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-07-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-07-01T00:00:05Z","message":{"model":"claude-opus-5","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
            ),
        )
        .expect("write");

        let session = read_session_summary(&path).expect("summary");
        assert_eq!(session.first_prompt.as_deref(), Some("hello"));
        assert_eq!(session.message_count, 2);
        assert!(session.models.contains("claude-opus-5"));
    }

    #[test]
    fn subagent_transcripts_are_skipped() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("agent-sub.jsonl");
        std::fs::write(&path, "{}\n").expect("write");
        assert!(read_session_summary(&path).is_none());
    }
}

mod real_store_bench {
    use std::io::BufRead as _;
    use std::time::Instant;

    use super::super::{
        claude_projects_dir, list_transcript_paths, parse_session, read_session_summary,
    };

    /// Compares the old "parse every transcript in full" behaviour against the
    /// bounded head/tail summary, using whatever is in the developer's real
    /// `~/.claude/projects`. Ignored by default because it depends on local
    /// data; run it explicitly when tuning the scan windows.
    #[test]
    #[ignore = "benchmark against the local ~/.claude store"]
    fn compare_full_parse_vs_bounded_summary() {
        let Some(root) = claude_projects_dir().filter(|r| r.is_dir()) else {
            println!("no ~/.claude/projects; skipping");
            return;
        };
        let paths = list_transcript_paths(&root);
        let total_bytes: u64 = paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        println!(
            "store: {} transcripts, {:.1} MB",
            paths.len(),
            total_bytes as f64 / 1_048_576.0
        );

        let started = Instant::now();
        for path in &paths {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            let lines = std::io::BufReader::new(file).lines().map_while(Result::ok);
            let _ = parse_session("id".to_string(), "dir".to_string(), lines);
        }
        let full = started.elapsed();

        let started = Instant::now();
        for path in &paths {
            let _ = read_session_summary(path);
        }
        let bounded = started.elapsed();

        println!("full parse (old):      {full:?}");
        println!("bounded summary (new): {bounded:?}");
        println!(
            "speedup: {:.1}x",
            full.as_secs_f64() / bounded.as_secs_f64().max(f64::EPSILON)
        );
    }
}
