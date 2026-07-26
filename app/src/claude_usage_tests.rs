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
    assert_eq!(u.five_hour_resets_at.as_deref(), Some("2026-07-16T04:50:00Z"));
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

/// The retain/expire policy behind the pill's visibility. These are the rules
/// that stop it from blinking in and out on its own.
mod poll_policy {
    use std::time::{Duration, Instant};

    use super::super::{ClaudeUsageModel, MAX_DISPLAY_AGE, MAX_POLL_BACKOFF, POLL_INTERVAL};
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

    #[test]
    fn a_failed_poll_keeps_the_last_reading_on_screen() {
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();
        model.record_poll(Some(reading(7.0)), now);

        model.record_poll(None, now + Duration::from_secs(60));

        assert_eq!(
            model.usage().map(|u| u.five_hour_pct),
            Some(7.0),
            "a transient failure must not blank a pill the user is looking at"
        );
    }

    #[test]
    fn repeated_failures_keep_it_until_the_reading_would_mislead() {
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();
        model.record_poll(Some(reading(7.0)), now);

        // Well past the poll interval, but still fresh enough to show.
        model.record_poll(None, now + MAX_DISPLAY_AGE - Duration::from_secs(1));
        assert!(model.usage().is_some(), "still within the display window");

        model.record_poll(None, now + MAX_DISPLAY_AGE);
        assert!(
            model.usage().is_none(),
            "past the cutoff the number could misinform, so the pill hides"
        );
    }

    #[test]
    fn a_later_success_restores_the_pill_and_the_steady_cadence() {
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();
        model.record_poll(Some(reading(7.0)), now);
        model.record_poll(None, now + Duration::from_secs(60));

        let delay = model.record_poll(Some(reading(11.0)), now + Duration::from_secs(120));

        assert_eq!(model.usage().map(|u| u.five_hour_pct), Some(11.0));
        assert_eq!(delay, POLL_INTERVAL, "healthy again: back to the slow poll");
    }

    #[test]
    fn failures_back_off_instead_of_hammering_a_rate_limited_endpoint() {
        // The endpoint answers 429 when polled too often, and a 429 carries no
        // reading — so retrying faster after a failure produces more failures.
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();
        model.record_poll(Some(reading(7.0)), now);

        let mut delays = Vec::new();
        for i in 1..=6 {
            delays.push(model.record_poll(None, now + Duration::from_secs(i)));
        }

        assert_eq!(
            delays,
            vec![
                POLL_INTERVAL,
                Duration::from_secs(120),
                Duration::from_secs(240),
                MAX_POLL_BACKOFF,
                MAX_POLL_BACKOFF,
                MAX_POLL_BACKOFF,
            ],
            "each failure waits longer, up to the cap"
        );
    }

    #[test]
    fn the_pill_can_still_recover_before_the_reading_expires() {
        // The backoff must not grow so fast that a stale reading times out
        // before the loop has had a fair number of chances to refresh it.
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();
        model.record_poll(Some(reading(7.0)), now);

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
            "only {attempts} refresh attempts before the pill expired"
        );
    }

    #[test]
    fn a_machine_without_credentials_is_not_polled_aggressively() {
        // No success has ever happened (no Claude Code login here). Polling
        // must not tighten into a subprocess-spawning loop for a pill that
        // cannot appear.
        let now = Instant::now();
        let mut model = ClaudeUsageModel::new();

        for i in 1..=5 {
            let delay = model.record_poll(None, now + Duration::from_secs(i));
            assert!(delay >= POLL_INTERVAL, "never faster than the steady cadence");
        }
        assert!(model.usage().is_none());
    }

    #[test]
    fn the_first_successful_poll_shows_the_pill() {
        let mut model = ClaudeUsageModel::new();
        assert!(model.usage().is_none(), "hidden until there is something to show");

        let delay = model.record_poll(Some(reading(3.0)), Instant::now());

        assert_eq!(model.usage().map(|u| u.five_hour_pct), Some(3.0));
        assert_eq!(delay, POLL_INTERVAL);
    }
}
