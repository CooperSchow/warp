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
