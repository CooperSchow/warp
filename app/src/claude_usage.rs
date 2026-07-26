//! Tracks the user's authoritative Claude subscription usage (the same 5-hour
//! and weekly limits Claude Code's `/usage` shows) and exposes it to the tab bar
//! as an always-visible pill. Gated behind `FeatureFlag::ClaudeUsage`.
//!
//! Data path (macOS): read Claude Code's OAuth token from the Keychain via
//! `security`, then `GET https://api.anthropic.com/api/oauth/usage`. Both run off
//! the main thread on a ~60s poll loop; on other platforms the Keychain lookup
//! fails and the pill simply stays hidden.

use std::time::{Duration, Instant};

use serde_json::Value;
use warpui::r#async::Timer;
use warpui::{Entity, ModelContext, SingletonEntity};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long the last good reading keeps showing while refreshes are failing.
///
/// Usage moves slowly — the 5-hour window is 300 minutes — so a few stale
/// minutes are harmless, and holding the number steady through a network blip
/// is far better than blinking the pill out of existence. Past this age the
/// figure could be misleading, so the pill hides rather than lie.
const MAX_DISPLAY_AGE: Duration = Duration::from_secs(30 * 60);

/// Slowest the poll loop gets after repeated failures.
///
/// Failures back *off* rather than retrying eagerly: the usage endpoint answers
/// HTTP 429 when it's asked too often, and a 429 body carries no reading — so
/// hammering it after a failure is what *causes* the next failure. Now that a
/// failed poll leaves the last reading on screen, there is nothing to gain by
/// hurrying. Capped low enough that the pill still appears within a few minutes
/// of Claude Code being logged in on a machine that had no credentials.
const MAX_POLL_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// How close to a limit we are, for coloring the pill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageSeverity {
    #[default]
    Normal,
    Warning,
    Critical,
}

impl UsageSeverity {
    fn from_api(s: &str) -> UsageSeverity {
        match s {
            "warning" => UsageSeverity::Warning,
            "critical" | "blocked" | "exceeded" => UsageSeverity::Critical,
            _ => UsageSeverity::Normal,
        }
    }

    /// Fallback when the API doesn't tag a severity: derive from the percentage.
    fn from_pct(pct: f32) -> UsageSeverity {
        if pct >= 90.0 {
            UsageSeverity::Critical
        } else if pct >= 70.0 {
            UsageSeverity::Warning
        } else {
            UsageSeverity::Normal
        }
    }

    fn combine(self, other: UsageSeverity) -> UsageSeverity {
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }
}

/// A single usage reading.
#[derive(Clone, Debug)]
pub struct ClaudeUsage {
    pub five_hour_pct: f32,
    pub weekly_pct: f32,
    pub five_hour_resets_at: Option<String>,
    pub weekly_resets_at: Option<String>,
    pub severity: UsageSeverity,
}

impl ClaudeUsage {
    /// Compact always-visible label, e.g. `5h 7% · wk 9%`.
    pub fn pill_label(&self) -> String {
        format!(
            "5h {:.0}% · wk {:.0}%",
            self.five_hour_pct, self.weekly_pct
        )
    }

    /// Multi-line detail shown on hover, including when each window resets.
    pub fn tooltip_text(&self) -> String {
        format!(
            "Claude usage\n5-hour limit: {:.0}%{}\nWeekly limit: {:.0}%{}",
            self.five_hour_pct,
            fmt_reset(&self.five_hour_resets_at),
            self.weekly_pct,
            fmt_reset(&self.weekly_resets_at),
        )
    }
}

/// Turn an ISO-8601 `resets_at` into a compact ` · resets 07-17 01:59 UTC` hint.
fn fmt_reset(iso: &Option<String>) -> String {
    match iso.as_deref().and_then(|s| s.get(5..16)) {
        Some(stamp) => format!(" · resets {} UTC", stamp.replace('T', " ")),
        None => String::new(),
    }
}

pub enum ClaudeUsageEvent {
    Updated,
}

/// Singleton model that polls the usage endpoint and holds the latest reading.
///
/// The reading is *sticky*: a poll that fails leaves the previous value in
/// place instead of clearing it. Polling a network endpoint every minute fails
/// occasionally for reasons the user neither causes nor cares about (a dropped
/// connection, a 5xx, Claude Code rotating its OAuth token mid-request), and
/// letting each of those blank the pill until the next success made it flicker
/// in and out on its own.
pub struct ClaudeUsageModel {
    usage: Option<ClaudeUsage>,
    /// When `usage` was last refreshed, for the [`MAX_DISPLAY_AGE`] cutoff.
    updated_at: Option<Instant>,
    /// Failed polls since the last success, for the backoff.
    consecutive_failures: u32,
}

impl ClaudeUsageModel {
    pub fn new() -> Self {
        Self {
            usage: None,
            updated_at: None,
            consecutive_failures: 0,
        }
    }

    /// Read by the tab bar via `ClaudeUsageModel::as_ref(ctx).usage()`.
    ///
    /// Pure: visibility only ever changes when a poll completes, so the pill
    /// can't appear or vanish between two repaints of unchanged state.
    pub fn usage(&self) -> Option<&ClaudeUsage> {
        self.usage.as_ref()
    }

    /// Kick off the poll loop; call once from the singleton constructor.
    pub fn start_polling(&mut self, ctx: &mut ModelContext<Self>) {
        self.refresh(ctx);
    }

    /// Folds one completed poll into the model state and returns how long to
    /// wait before the next one.
    ///
    /// Separated from the async plumbing so the retain/expire policy is
    /// testable without a network or an app context.
    fn record_poll(&mut self, fetched: Option<ClaudeUsage>, now: Instant) -> Duration {
        match fetched {
            Some(usage) => {
                self.usage = Some(usage);
                self.updated_at = Some(now);
                self.consecutive_failures = 0;
            }
            None => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                // Keep showing the last reading — unless it is old enough that
                // it would misinform rather than inform.
                let expired = self
                    .updated_at
                    .is_some_and(|at| now.saturating_duration_since(at) >= MAX_DISPLAY_AGE);
                if expired {
                    self.usage = None;
                    self.updated_at = None;
                }
            }
        }
        self.next_delay()
    }

    /// Steady 60s cadence when healthy, doubling up to [`MAX_POLL_BACKOFF`]
    /// while polls keep failing.
    fn next_delay(&self) -> Duration {
        if self.consecutive_failures == 0 {
            return POLL_INTERVAL;
        }
        POLL_INTERVAL
            .checked_mul(1u32 << (self.consecutive_failures - 1).min(31))
            .unwrap_or(MAX_POLL_BACKOFF)
            .min(MAX_POLL_BACKOFF)
    }

    fn refresh(&mut self, ctx: &mut ModelContext<Self>) {
        ctx.spawn(async move { fetch_usage().await }, |me, usage, ctx| {
            let delay = me.record_poll(usage, Instant::now());
            ctx.emit(ClaudeUsageEvent::Updated);
            ctx.notify();
            // Re-arm the timer.
            ctx.spawn(
                async move {
                    Timer::after(delay).await;
                },
                |me, _, ctx| me.refresh(ctx),
            );
        });
    }
}

impl Default for ClaudeUsageModel {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for ClaudeUsageModel {
    type Event = ClaudeUsageEvent;
}

impl SingletonEntity for ClaudeUsageModel {}

/// Read Claude Code's OAuth access token from the macOS Keychain.
async fn read_token() -> Option<String> {
    let out = match command::r#async::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            log::warn!("[claude_usage] security spawn failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        log::warn!(
            "[claude_usage] security exit={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    let json: Value = match serde_json::from_slice(&out.stdout) {
        Ok(json) => json,
        Err(e) => {
            log::warn!("[claude_usage] keychain json parse failed: {e}");
            return None;
        }
    };
    let token = json
        .get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    log::debug!(
        "[claude_usage] token read: {}",
        if token.is_some() { "OK" } else { "missing accessToken" }
    );
    token
}

async fn fetch_usage() -> Option<ClaudeUsage> {
    let token = read_token().await?;
    let auth = format!("Authorization: Bearer {token}");
    let out = match command::r#async::Command::new("/usr/bin/curl")
        .args([
            "-s",
            "-H",
            auth.as_str(),
            "-H",
            "anthropic-beta: oauth-2025-04-20",
            "-H",
            "anthropic-version: 2023-06-01",
            "-H",
            "Accept: application/json",
            USAGE_URL,
        ])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            log::warn!("[claude_usage] curl spawn failed: {e}");
            return None;
        }
    };
    log::debug!(
        "[claude_usage] curl exit={:?} body_len={}",
        out.status.code(),
        out.stdout.len()
    );
    let parsed = parse_usage(&out.stdout);
    match parsed {
        Some(_) => log::debug!("[claude_usage] parse: OK"),
        None => log::warn!(
            "[claude_usage] no reading in response ({}); keeping the previous one",
            api_error_kind(&out.stdout).unwrap_or_else(|| "unrecognized body".to_string())
        ),
    }
    parsed
}

/// The API's own error tag (e.g. `rate_limit_error`) from an error body, so a
/// run of failures can be told apart from a misconfiguration in the logs.
fn api_error_kind(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("error")?
        .get("type")?
        .as_str()
        .map(str::to_owned)
}

fn parse_usage(body: &[u8]) -> Option<ClaudeUsage> {
    let v: Value = serde_json::from_slice(body).ok()?;

    // A valid usage payload has a `five_hour.utilization`; anything else
    // (error page, `{"error": ...}`) yields no pill rather than a bogus 0%.
    let five_hour_pct = v
        .get("five_hour")
        .and_then(|w| w.get("utilization"))
        .and_then(Value::as_f64)? as f32;
    let weekly_pct = v
        .get("seven_day")
        .and_then(|w| w.get("utilization"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let five_hour_resets_at = v["five_hour"]["resets_at"].as_str().map(str::to_owned);
    let weekly_resets_at = v["seven_day"]["resets_at"].as_str().map(str::to_owned);

    // Prefer the structured severity from the active `limits[]`; else derive it.
    let mut severity = UsageSeverity::Normal;
    let mut saw_tagged = false;
    if let Some(limits) = v.get("limits").and_then(Value::as_array) {
        for limit in limits {
            if limit.get("is_active").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if let Some(sev) = limit.get("severity").and_then(Value::as_str) {
                severity = severity.combine(UsageSeverity::from_api(sev));
                saw_tagged = true;
            }
        }
    }
    if !saw_tagged {
        severity = UsageSeverity::from_pct(five_hour_pct.max(weekly_pct));
    }

    Some(ClaudeUsage {
        five_hour_pct,
        weekly_pct,
        five_hour_resets_at,
        weekly_resets_at,
        severity,
    })
}

#[cfg(test)]
#[path = "claude_usage_tests.rs"]
mod tests;
