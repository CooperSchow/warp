//! Tracks the user's authoritative Claude subscription usage (the same 5-hour
//! and weekly limits Claude Code's `/usage` shows) and exposes it to the tab bar
//! as an always-visible pill. Gated behind `FeatureFlag::ClaudeUsage`.
//!
//! Data path (macOS): read Claude Code's OAuth token from the Keychain via
//! `security`, then `GET https://api.anthropic.com/api/oauth/usage`. Both run off
//! the main thread on a ~60s poll loop; on other platforms the Keychain lookup
//! fails and the pill simply stays hidden.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use warpui::r#async::Timer;
use warpui::{Entity, ModelContext, SingletonEntity};

use crate::channel::{Channel, ChannelState};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Steady poll cadence.
///
/// The endpoint is shared with Claude Code's own usage checks and rate-limits
/// the account as a whole (HTTP 429), so polling cheaply is a correctness
/// concern, not just politeness — a 429 carries no reading. Five minutes is
/// ample for an ambient indicator of a 300-minute window.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Slowest the poll loop gets after repeated failures.
///
/// Failures back *off* rather than retrying eagerly: against a rate-limited
/// endpoint an eager retry is what produces the next failure, and with the last
/// reading still on screen there is nothing to hurry for.
const MAX_POLL_BACKOFF: Duration = Duration::from_secs(20 * 60);

/// Age at which a reading stops being presented as current: it still shows, but
/// muted and without its severity color, since usage may have moved since.
const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Age at which a reading is dropped entirely and the pill falls back to its
/// placeholder. The pill itself stays put — only the figures go away.
const MAX_DISPLAY_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// Cache file holding the last reading, so a relaunch shows real figures
/// immediately instead of waiting on a poll that may be rate-limited.
const CACHE_FILE: &str = "claude_usage_cache.json";

/// How close to a limit we are, for coloring the pill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaudeUsage {
    pub five_hour_pct: f32,
    pub weekly_pct: f32,
    pub five_hour_resets_at: Option<String>,
    pub weekly_resets_at: Option<String>,
    pub severity: UsageSeverity,
}

/// Stands in for the figures until a reading lands. Same shape as a real label
/// so the pill keeps its size when data arrives instead of jumping.
pub const PLACEHOLDER_LABEL: &str = "5h — · wk —";

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
    /// Wall-clock time the reading was taken. Wall clock rather than `Instant`
    /// so a reading restored from disk can be aged across a restart.
    taken_at: Option<SystemTime>,
    /// True once the reading is older than [`STALE_AFTER`] — shown without its
    /// severity color, since usage may have moved since it was taken.
    ///
    /// Recomputed only when a poll completes, never at render time, so the
    /// pill's appearance can't change between two repaints of the same state.
    stale: bool,
    /// Failed polls since the last success, for the backoff.
    consecutive_failures: u32,
}

impl ClaudeUsageModel {
    /// Starts from the reading left by the previous launch, when there is one
    /// recent enough to still mean something. The endpoint rate-limits, so the
    /// first poll of a session often fails; without this the pill would sit on
    /// its placeholder for minutes after every restart.
    ///
    /// A restored reading counts as stale from the outset — it is shown, but
    /// its severity color isn't, until a live poll confirms it.
    pub fn new() -> Self {
        Self::restoring(load_cached_reading(), SystemTime::now())
    }

    /// Builds a model around a reading recovered from disk, keeping it only
    /// while it is recent enough to still mean something.
    ///
    /// Split from [`Self::new`] so the freshness rule can be tested without
    /// depending on what happens to be cached on the machine running the tests.
    fn restoring(cached: Option<(ClaudeUsage, SystemTime)>, now: SystemTime) -> Self {
        let restored = cached.filter(|(_, taken_at)| age(*taken_at, now) < MAX_DISPLAY_AGE);
        Self {
            stale: restored.is_some(),
            usage: restored.as_ref().map(|(usage, _)| usage.clone()),
            taken_at: restored.map(|(_, taken_at)| taken_at),
            consecutive_failures: 0,
        }
    }

    /// Read by the tab bar via `ClaudeUsageModel::as_ref(ctx).usage()`.
    ///
    /// Pure: the value only ever changes when a poll completes, so the pill
    /// can't change between two repaints of unchanged state.
    pub fn usage(&self) -> Option<&ClaudeUsage> {
        self.usage.as_ref()
    }

    /// True when the current reading is old enough that its severity color
    /// should not be trusted. Meaningless when [`Self::usage`] is `None`.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Kick off the poll loop; call once from the singleton constructor.
    ///
    /// Never in integration tests: they run the real app against a throwaway
    /// `HOME`, but a poll reads Claude Code's credential from the login
    /// keychain and spends the account's rate limit on the live endpoint.
    pub fn start_polling(&mut self, ctx: &mut ModelContext<Self>) {
        if ChannelState::channel() == Channel::Integration {
            return;
        }
        self.refresh(ctx);
    }

    /// Folds one completed poll into the model state and returns how long to
    /// wait before the next one.
    ///
    /// Separated from the async plumbing so the retain/expire policy is
    /// testable without a network or an app context.
    fn record_poll(&mut self, fetched: Option<ClaudeUsage>, now: SystemTime) -> Duration {
        match fetched {
            Some(usage) => {
                self.usage = Some(usage);
                self.taken_at = Some(now);
                self.stale = false;
                self.consecutive_failures = 0;
            }
            None => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                // Keep showing the last reading — but stop vouching for it once
                // it ages, and drop it entirely once it would misinform.
                let age = self.taken_at.map(|at| age(at, now));
                self.stale = age.is_some_and(|age| age >= STALE_AFTER);
                if age.is_some_and(|age| age >= MAX_DISPLAY_AGE) {
                    self.usage = None;
                    self.taken_at = None;
                    self.stale = false;
                }
            }
        }
        self.next_delay()
    }

    /// Steady [`POLL_INTERVAL`] cadence when healthy, doubling up to
    /// [`MAX_POLL_BACKOFF`] while polls keep failing.
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
        ctx.spawn(
            async move {
                let fetched = fetch_usage().await;
                // Persisted here, on the background executor, so the main
                // thread never waits on a file write.
                if let Some(usage) = &fetched {
                    save_cached_reading(usage, SystemTime::now());
                }
                fetched
            },
            |me, usage, ctx| {
                let delay = me.record_poll(usage, SystemTime::now());
                ctx.emit(ClaudeUsageEvent::Updated);
                ctx.notify();
                // Re-arm the timer.
                ctx.spawn(
                    async move {
                        Timer::after(delay).await;
                    },
                    |me, _, ctx| me.refresh(ctx),
                );
            },
        );
    }
}

/// Elapsed wall-clock time between two instants, saturating at zero so a clock
/// adjustment can't make a reading look like it came from the future.
fn age(taken_at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(taken_at).unwrap_or(Duration::ZERO)
}

/// The last reading, as persisted between launches.
#[derive(Serialize, Deserialize)]
struct CachedReading {
    usage: ClaudeUsage,
    /// Unix seconds; portable across restarts, unlike a monotonic instant.
    taken_at_unix: u64,
}

fn cache_path() -> PathBuf {
    warp_core::paths::data_dir().join(CACHE_FILE)
}

/// Reads the reading left behind by a previous launch, if it parses.
///
/// One small file read during singleton construction; deliberately not spawned,
/// so the pill can render its real figures on the very first frame.
fn load_cached_reading() -> Option<(ClaudeUsage, SystemTime)> {
    load_cached_reading_from(&cache_path())
}

/// Records a reading for the next launch. Best-effort: a cache that can't be
/// written costs a placeholder on next start and nothing else.
fn save_cached_reading(usage: &ClaudeUsage, taken_at: SystemTime) {
    save_cached_reading_to(&cache_path(), usage, taken_at);
}

fn load_cached_reading_from(path: &std::path::Path) -> Option<(ClaudeUsage, SystemTime)> {
    let body = std::fs::read(path).ok()?;
    let cached: CachedReading = serde_json::from_slice(&body).ok()?;
    let taken_at = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(cached.taken_at_unix))?;
    Some((cached.usage, taken_at))
}

fn save_cached_reading_to(path: &std::path::Path, usage: &ClaudeUsage, taken_at: SystemTime) {
    let Ok(since_epoch) = taken_at.duration_since(SystemTime::UNIX_EPOCH) else {
        return;
    };
    let cached = CachedReading {
        usage: usage.clone(),
        taken_at_unix: since_epoch.as_secs(),
    };
    let Ok(body) = serde_json::to_vec(&cached) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, body) {
        log::debug!("[claude_usage] could not cache reading: {e}");
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
