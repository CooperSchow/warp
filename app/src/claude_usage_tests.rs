use super::{parse_usage, UsageSeverity};

#[test]
fn parses_windows_and_resets() {
    let body = br#"{
        "five_hour": {"utilization": 7.0, "resets_at": "2026-07-16T04:50:00Z"},
        "seven_day": {"utilization": 9.0, "resets_at": "2026-07-20T05:00:00Z"},
        "limits": [{"kind":"session","percent":7,"severity":"normal","is_active":false}]
    }"#;
    let u = parse_usage(body).expect("valid payload");
    assert_eq!(u.five_hour_pct, 7.0);
    assert_eq!(u.weekly_pct, 9.0);
    assert_eq!(
        u.five_hour_resets_at.as_deref(),
        Some("2026-07-16T04:50:00Z")
    );
    assert_eq!(u.weekly_resets_at.as_deref(), Some("2026-07-20T05:00:00Z"));
    assert_eq!(u.pill_label(), "5h 7% · wk 9%");
}

#[test]
fn takes_worst_active_severity_from_limits() {
    let body = br#"{
        "five_hour": {"utilization": 20.0},
        "seven_day": {"utilization": 5.0},
        "limits": [
            {"severity":"normal","is_active":true},
            {"severity":"critical","is_active":true},
            {"severity":"warning","is_active":false}
        ]
    }"#;
    let u = parse_usage(body).unwrap();
    // critical is active; the inactive... entry is ignored.
    assert_eq!(u.severity, UsageSeverity::Critical);
}

#[test]
fn falls_back_to_percentage_severity_when_untagged() {
    let body = br#"{"five_hour": {"utilization": 92.0}, "seven_day": {"utilization": 3.0}}"#;
    let u = parse_usage(body).unwrap();
    assert_eq!(u.severity, UsageSeverity::Critical); // 92% -> critical

    let body2 = br#"{"five_hour": {"utilization": 75.0}, "seven_day": {"utilization": 3.0}}"#;
    assert_eq!(parse_usage(body2).unwrap().severity, UsageSeverity::Warning);

    let body3 = br#"{"five_hour": {"utilization": 10.0}, "seven_day": {"utilization": 3.0}}"#;
    assert_eq!(parse_usage(body3).unwrap().severity, UsageSeverity::Normal);
}

#[test]
fn rejects_non_usage_payloads() {
    assert!(parse_usage(br#"{"error": "unauthorized"}"#).is_none());
    assert!(parse_usage(br#"not json"#).is_none());
    assert!(parse_usage(br#"{"seven_day": {"utilization": 5.0}}"#).is_none()); // no five_hour
}

/// The retain/expire policy behind the pill's figures. The pill itself is
/// always on screen once enabled — these rules govern what it shows.
mod poll_policy {
    use std::time::{Duration, SystemTime};

    use super::super::{
        ClaudeUsageModel, MAX_DISPLAY_AGE, MAX_POLL_BACKOFF, POLL_INTERVAL, STALE_AFTER,
    };
    use crate::claude_usage::{ClaudeUsage, UsageSeverity};

    fn reading(pct: f32) -> ClaudeUsage {
        ClaudeUsage {
            five_hour_pct: pct,
            weekly_pct: pct,
            five_hour_resets_at: None,
            weekly_resets_at: None,
            severity: UsageSeverity::Normal,
        }
    }

    /// A model with no cache restored, so tests start from a known state.
    fn model_with(reading: Option<ClaudeUsage>, now: SystemTime) -> ClaudeUsageModel {
        let mut model = ClaudeUsageModel::restoring(None, now);
        if let Some(reading) = reading {
            model.record_poll(Some(reading), now);
        }
        model
    }

    #[test]
    fn a_failed_poll_keeps_the_last_reading_on_screen() {
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);

        model.record_poll(None, now + POLL_INTERVAL);

        assert_eq!(
            model.usage().map(|u| u.five_hour_pct),
            Some(7.0),
            "a transient failure must not blank figures the user is looking at"
        );
        assert!(!model.is_stale(), "one missed refresh is not yet stale");
    }

    #[test]
    fn a_reading_goes_stale_before_it_is_dropped() {
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);

        model.record_poll(None, now + STALE_AFTER);
        assert!(model.usage().is_some(), "still shown");
        assert!(
            model.is_stale(),
            "shown without a severity color once it may have moved"
        );

        model.record_poll(None, now + MAX_DISPLAY_AGE);
        assert!(
            model.usage().is_none(),
            "past the cutoff the number could misinform, so it gives way to the placeholder"
        );
        assert!(!model.is_stale(), "nothing left to be stale about");
    }

    #[test]
    fn a_later_success_restores_the_figures_and_the_steady_cadence() {
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);
        model.record_poll(None, now + STALE_AFTER);
        assert!(model.is_stale());

        let delay = model.record_poll(Some(reading(11.0)), now + STALE_AFTER * 2);

        assert_eq!(model.usage().map(|u| u.five_hour_pct), Some(11.0));
        assert!(!model.is_stale(), "a fresh reading is trusted again");
        assert_eq!(delay, POLL_INTERVAL, "healthy again: back to the slow poll");
    }

    #[test]
    fn failures_back_off_instead_of_hammering_a_rate_limited_endpoint() {
        // The endpoint answers 429 when polled too often, and a 429 carries no
        // reading — so retrying faster after a failure produces more failures.
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);

        let mut delays = Vec::new();
        for i in 1..=6 {
            delays.push(model.record_poll(None, now + Duration::from_secs(i)));
        }

        assert_eq!(
            delays,
            vec![
                POLL_INTERVAL,
                POLL_INTERVAL * 2,
                POLL_INTERVAL * 4,
                MAX_POLL_BACKOFF,
                MAX_POLL_BACKOFF,
                MAX_POLL_BACKOFF,
            ],
            "each failure waits longer, up to the cap"
        );
    }

    #[test]
    fn the_figures_can_still_recover_before_they_expire() {
        // The backoff must not grow so fast that a stale reading times out
        // before the loop has had a fair number of chances to refresh it.
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);

        let mut elapsed = Duration::ZERO;
        let mut attempts = 0;
        loop {
            let delay = model.record_poll(None, now + elapsed);
            if model.usage().is_none() {
                break;
            }
            elapsed += delay;
            attempts += 1;
            assert!(attempts < 1000, "must terminate");
        }

        assert!(
            attempts >= 5,
            "only {attempts} refresh attempts before the figures expired"
        );
    }

    #[test]
    fn a_machine_without_credentials_is_not_polled_aggressively() {
        // No success has ever happened (no Claude Code login here). Polling
        // must not tighten into a subprocess-spawning loop.
        let now = SystemTime::now();
        let mut model = model_with(None, now);

        for i in 1..=5 {
            let delay = model.record_poll(None, now + Duration::from_secs(i));
            assert!(
                delay >= POLL_INTERVAL,
                "never faster than the steady cadence"
            );
        }
        assert!(model.usage().is_none());
    }

    #[test]
    fn the_first_successful_poll_supplies_the_figures() {
        let now = SystemTime::now();
        let mut model = model_with(None, now);
        assert!(
            model.usage().is_none(),
            "placeholder until there is something to show"
        );

        let delay = model.record_poll(Some(reading(3.0)), now);

        assert_eq!(model.usage().map(|u| u.five_hour_pct), Some(3.0));
        assert!(!model.is_stale());
        assert_eq!(delay, POLL_INTERVAL);
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_expire_a_reading() {
        let now = SystemTime::now();
        let mut model = model_with(Some(reading(7.0)), now);

        model.record_poll(None, now - Duration::from_secs(3600));

        assert!(
            model.usage().is_some(),
            "a reading from the future is odd, not expired"
        );
    }
}

/// The reading that survives a relaunch: real file I/O, and the freshness rule
/// that decides whether it is still worth showing.
mod cache {
    use std::time::{Duration, SystemTime};

    use super::super::{
        load_cached_reading_from, save_cached_reading_to, ClaudeUsageModel, MAX_DISPLAY_AGE,
    };
    use crate::claude_usage::{ClaudeUsage, UsageSeverity};

    fn reading() -> ClaudeUsage {
        ClaudeUsage {
            five_hour_pct: 42.0,
            weekly_pct: 13.5,
            five_hour_resets_at: Some("2026-07-26T23:20:00Z".to_string()),
            weekly_resets_at: None,
            severity: UsageSeverity::Warning,
        }
    }

    #[test]
    fn a_reading_round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("claude_usage_cache.json");
        let taken_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_000_000);

        save_cached_reading_to(&path, &reading(), taken_at);
        let (read, read_taken_at) = load_cached_reading_from(&path).expect("cached reading");

        assert_eq!(read.five_hour_pct, 42.0);
        assert_eq!(read.weekly_pct, 13.5);
        assert_eq!(read.severity, UsageSeverity::Warning);
        assert_eq!(
            read.five_hour_resets_at.as_deref(),
            Some("2026-07-26T23:20:00Z")
        );
        assert_eq!(
            read_taken_at, taken_at,
            "second-resolution timestamp must survive the round trip"
        );
    }

    #[test]
    fn the_cache_directory_is_created_if_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("not-created-yet").join("cache.json");

        save_cached_reading_to(&path, &reading(), SystemTime::now());

        assert!(path.is_file(), "a fresh profile has no data dir yet");
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent.json");
        assert!(load_cached_reading_from(&missing).is_none());

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"not json at all").expect("write");
        assert!(load_cached_reading_from(&corrupt).is_none());

        let partial = dir.path().join("partial.json");
        std::fs::write(&partial, b"{}").expect("write");
        assert!(load_cached_reading_from(&partial).is_none());
    }

    #[test]
    fn a_recent_cached_reading_is_shown_but_not_trusted() {
        let now = SystemTime::now();
        let taken_at = now - Duration::from_secs(60);

        let model = ClaudeUsageModel::restoring(Some((reading(), taken_at)), now);

        assert_eq!(
            model.usage().map(|u| u.five_hour_pct),
            Some(42.0),
            "a relaunch should show real figures, not the placeholder"
        );
        assert!(
            model.is_stale(),
            "restored figures wear no severity color until a live poll confirms them"
        );
    }

    #[test]
    fn a_cached_reading_too_old_to_mean_anything_is_discarded() {
        let now = SystemTime::now();
        let taken_at = now - MAX_DISPLAY_AGE;

        let model = ClaudeUsageModel::restoring(Some((reading(), taken_at)), now);

        assert!(
            model.usage().is_none(),
            "yesterday's usage would misinform; fall back to the placeholder"
        );
    }
}
