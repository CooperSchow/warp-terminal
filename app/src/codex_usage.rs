//! Tracks the user's OpenAI Codex subscription usage — the same rolling limits
//! the Codex CLI's own status view shows — and exposes it to the tab bar as a
//! second always-visible pill beside the Claude one. Gated behind
//! `FeatureFlag::CodexUsage`.
//!
//! Data path: run `codex app-server`, the JSON-RPC service bundled with the
//! Codex CLI, and ask it `account/rateLimits/read`. That service reads the
//! CLI's own credential file and refreshes its own tokens, so unlike the Claude
//! pill this never handles a token and there is no keychain lookup to fail.
//! Where the Codex CLI isn't installed the binary simply doesn't resolve and
//! the pill sits on its placeholder.
//!
//! Two things about the payload shape drive the parsing below. Limits arrive as
//! a map of metered buckets (`codex`, and one per specialised model), any of
//! which can be the binding constraint — so the worst bucket is what gets
//! reported. And a bucket's two windows are *not* labelled 5-hour and weekly;
//! each carries a `windowDurationMins`, and which slot it belongs in has to be
//! read off that. On a Pro-lite account the `codex` bucket's `primary` window
//! is the weekly one, so assuming primary means short would report the wrong
//! number under a confident-looking label.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use command::r#async::Command;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use warpui::r#async::{FutureExt, Timer};
use warpui::{Entity, ModelContext, SingletonEntity};

use crate::agent_usage::{age, UsageSeverity};
use crate::channel::{Channel, ChannelState};

/// Steady poll cadence.
///
/// Each poll spawns a short-lived `codex app-server` process, which is cheap
/// (measured at about a third of a second, including the spawn) but not free,
/// and the windows being reported are hours and days long. Five minutes keeps
/// the pill honest without putting a process launch on a tight loop, and
/// matches the Claude pill so the two readings stay roughly contemporaneous.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Slowest the poll loop gets after repeated failures. Failures here usually
/// mean the CLI is missing, logged out, or mid-upgrade — none of which a faster
/// retry fixes.
const MAX_POLL_BACKOFF: Duration = Duration::from_secs(20 * 60);

/// Age at which a reading stops being presented as current: it still shows, but
/// muted and without its severity colour.
const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Age at which a reading is dropped entirely and the pill falls back to its
/// placeholder. The pill itself stays put — only the figures go away.
const MAX_DISPLAY_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// How long the whole app-server exchange gets before it's abandoned. Generous,
/// because it covers a process launch plus a round trip to OpenAI; the poll runs
/// on a background executor, so a slow one costs nothing on screen.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Cache file holding the last reading, so a relaunch shows real figures
/// immediately instead of waiting on the first poll.
const CACHE_FILE: &str = "codex_usage_cache.json";

/// Longest `windowDurationMins` still treated as the short rolling window.
///
/// Codex reports 300 minutes for the short window and 10080 for the weekly one.
/// The threshold sits well clear of both, so a provider-side change from 300 to
/// 360 or 480 lands in the same slot instead of being relabelled as weekly.
const SHORT_WINDOW_MAX_MINS: i64 = 600;

/// JSON-RPC id used for the rate-limits request, so its reply can be picked out
/// of the notification traffic the app server interleaves with responses.
const RATE_LIMITS_REQUEST_ID: i64 = 2;

/// The whole client side of the exchange, written in one go.
///
/// `initialize` must be answered before any other request is accepted, and the
/// server tolerates the follow-ups arriving before it has replied, so there is
/// no reason to round-trip three times.
const REQUESTS: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"WarpOss","title":"Warp","version":"1.0.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"initialized"}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read","params":{"excludeResetCreditDetails":true}}"#,
    "\n",
);

/// Stands in for the figures until a reading lands.
pub const PLACEHOLDER_LABEL: &str = "—";

/// One rolling limit window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexUsageWindow {
    pub used_pct: f32,
    /// Unix seconds at which the window rolls over, when the server says.
    pub resets_at: Option<i64>,
    /// Which metered bucket this figure came from, for the tooltip. `None` when
    /// the server reported a single unnamed bucket.
    pub bucket: Option<String>,
}

/// A single usage reading.
///
/// Both windows are optional and independently so: an account can be metered on
/// a weekly limit with no short window at all, and reporting a missing window as
/// 0% would read as "plenty of room" when the truth is "not measured".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexUsage {
    pub short_window: Option<CodexUsageWindow>,
    pub weekly: Option<CodexUsageWindow>,
    /// The plan the limits belong to (`plus`, `prolite`, `business`, …), shown
    /// in the tooltip so an unexpected figure can be traced to the plan.
    pub plan_type: Option<String>,
    /// The server's own verdict on whether included usage may still be spent.
    /// `Some(false)` means blocked, which the pill must show as critical however
    /// modest the percentages look.
    pub ordinary_usage_allowed: Option<bool>,
    pub severity: UsageSeverity,
}

impl CodexUsage {
    /// Compact always-visible label, e.g. `5h 21% · wk 3%`, or just `wk 3%` on
    /// an account with no short window.
    pub fn pill_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(window) = &self.short_window {
            parts.push(format!("5h {:.0}%", window.used_pct));
        }
        if let Some(window) = &self.weekly {
            parts.push(format!("wk {:.0}%", window.used_pct));
        }
        if parts.is_empty() {
            return PLACEHOLDER_LABEL.to_string();
        }
        parts.join(" · ")
    }

    /// Multi-line detail shown on hover.
    pub fn tooltip_text(&self) -> String {
        let mut lines = vec![match self.plan_type.as_deref() {
            Some(plan) => format!("Codex usage ({})", plan_label(plan)),
            None => "Codex usage".to_string(),
        }];
        if let Some(window) = &self.short_window {
            lines.push(format!(
                "5-hour limit: {:.0}%{}{}",
                window.used_pct,
                fmt_reset(window.resets_at),
                fmt_bucket(window.bucket.as_deref()),
            ));
        }
        if let Some(window) = &self.weekly {
            lines.push(format!(
                "Weekly limit: {:.0}%{}{}",
                window.used_pct,
                fmt_reset(window.resets_at),
                fmt_bucket(window.bucket.as_deref()),
            ));
        }
        if self.short_window.is_none() && self.weekly.is_none() {
            lines.push("No metered window reported for this account.".to_string());
        }
        if self.ordinary_usage_allowed == Some(false) {
            lines.push("Included usage is currently blocked.".to_string());
        }
        lines.join("\n")
    }
}

/// Tidy a plan slug for display. Only the plans worth distinguishing are named;
/// anything else is shown as the server spelled it, which is more useful than a
/// guess.
fn plan_label(plan: &str) -> String {
    match plan {
        "free" => "Free".to_string(),
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "prolite" => "Pro Lite".to_string(),
        "team" => "Team".to_string(),
        "business" => "Business".to_string(),
        "enterprise" => "Enterprise".to_string(),
        other => other.to_string(),
    }
}

/// Turn a unix reset timestamp into a compact ` · resets 09-12 14:20 UTC` hint,
/// matching the Claude pill's phrasing.
fn fmt_reset(resets_at: Option<i64>) -> String {
    match resets_at.and_then(|secs| DateTime::from_timestamp(secs, 0)) {
        Some(when) => format!(" · resets {} UTC", when.format("%m-%d %H:%M")),
        None => String::new(),
    }
}

/// Name the metered bucket a figure came from, but only when it isn't the plain
/// `codex` one — naming that on every line would be noise.
fn fmt_bucket(bucket: Option<&str>) -> String {
    match bucket {
        Some(bucket) if bucket != "codex" => format!(" ({bucket})"),
        Some(_) | None => String::new(),
    }
}

pub enum CodexUsageEvent {
    Updated,
}

/// Singleton model that polls the Codex app server and holds the latest
/// reading.
///
/// The reading is *sticky*, on the same reasoning as the Claude pill: a poll
/// that fails leaves the previous value in place rather than blanking the pill,
/// because the failures a user neither causes nor cares about (the CLI
/// mid-upgrade, a dropped connection) would otherwise make it flicker.
pub struct CodexUsageModel {
    usage: Option<CodexUsage>,
    /// Wall-clock time the reading was taken, so a reading restored from disk
    /// can be aged across a restart.
    taken_at: Option<SystemTime>,
    /// True once the reading is older than [`STALE_AFTER`]. Recomputed only when
    /// a poll completes, never at render time, so the pill's appearance can't
    /// change between two repaints of the same state.
    stale: bool,
    /// Failed polls since the last success, for the backoff.
    consecutive_failures: u32,
}

impl CodexUsageModel {
    /// Starts from the reading left by the previous launch, when there is one
    /// recent enough to still mean something.
    pub fn new() -> Self {
        Self::restoring(load_cached_reading(), SystemTime::now())
    }

    /// Builds a model around a reading recovered from disk, keeping it only
    /// while it is recent enough to still mean something.
    ///
    /// Split from [`Self::new`] so the freshness rule can be tested without
    /// depending on what happens to be cached on the machine running the tests.
    fn restoring(cached: Option<(CodexUsage, SystemTime)>, now: SystemTime) -> Self {
        let restored = cached.filter(|(_, taken_at)| age(*taken_at, now) < MAX_DISPLAY_AGE);
        Self {
            stale: restored.is_some(),
            usage: restored.as_ref().map(|(usage, _)| usage.clone()),
            taken_at: restored.map(|(_, taken_at)| taken_at),
            consecutive_failures: 0,
        }
    }

    /// Read by the tab bar via `CodexUsageModel::as_ref(ctx).usage()`.
    pub fn usage(&self) -> Option<&CodexUsage> {
        self.usage.as_ref()
    }

    /// True when the current reading is old enough that its severity colour
    /// should not be trusted. Meaningless when [`Self::usage`] is `None`.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Kick off the poll loop; call once from the singleton constructor.
    ///
    /// Never in integration tests: they run the real app against a throwaway
    /// `HOME`, and a poll would spend the account's real rate limit.
    pub fn start_polling(&mut self, ctx: &mut ModelContext<Self>) {
        if ChannelState::channel() == Channel::Integration {
            return;
        }
        self.refresh(ctx);
    }

    /// Folds one completed poll into the model state and returns how long to
    /// wait before the next one.
    fn record_poll(&mut self, fetched: Option<CodexUsage>, now: SystemTime) -> Duration {
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
                ctx.emit(CodexUsageEvent::Updated);
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

/// The last reading, as persisted between launches.
#[derive(Serialize, Deserialize)]
struct CachedReading {
    usage: CodexUsage,
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
fn load_cached_reading() -> Option<(CodexUsage, SystemTime)> {
    load_cached_reading_from(&cache_path())
}

/// Records a reading for the next launch. Best-effort: a cache that can't be
/// written costs a placeholder on next start and nothing else.
fn save_cached_reading(usage: &CodexUsage, taken_at: SystemTime) {
    save_cached_reading_to(&cache_path(), usage, taken_at);
}

fn load_cached_reading_from(path: &std::path::Path) -> Option<(CodexUsage, SystemTime)> {
    let body = std::fs::read(path).ok()?;
    let cached: CachedReading = serde_json::from_slice(&body).ok()?;
    let taken_at = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(cached.taken_at_unix))?;
    Some((cached.usage, taken_at))
}

fn save_cached_reading_to(path: &std::path::Path, usage: &CodexUsage, taken_at: SystemTime) {
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
        log::debug!("[codex_usage] could not cache reading: {e}");
    }
}

impl Default for CodexUsageModel {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for CodexUsageModel {
    type Event = CodexUsageEvent;
}

impl SingletonEntity for CodexUsageModel {}

/// Absolute path of the Codex CLI.
///
/// Resolved by path rather than left to `PATH`, because the bundled app is
/// launched by launchd with an empty environment — the same reason the Claude
/// pill calls `/usr/bin/security` and `/usr/bin/curl` outright. The installed
/// shim points at a `current` symlink the CLI repoints on upgrade, so this
/// keeps working across Codex versions.
fn codex_binary() -> Option<PathBuf> {
    let home = dirs::home_dir();
    let candidates = [
        // An explicit override wins, which also makes this testable.
        std::env::var_os("WARP_CODEX_BIN").map(PathBuf::from),
        home.as_ref().map(|h| h.join(".local/bin/codex")),
        home.as_ref()
            .map(|h| h.join(".codex/packages/standalone/current/bin/codex")),
        Some(PathBuf::from("/opt/homebrew/bin/codex")),
        Some(PathBuf::from("/usr/local/bin/codex")),
    ];
    let found = candidates.into_iter().flatten().find(|path| path.is_file());
    if found.is_none() {
        log::debug!("[codex_usage] no codex binary found; pill stays on its placeholder");
    }
    found
}

/// Run one `account/rateLimits/read` exchange against a fresh app server.
async fn fetch_usage() -> Option<CodexUsage> {
    let program = codex_binary()?;
    let mut command = Command::new(&program);
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The app server logs to stderr; nothing here reads it, and inheriting
        // it would scatter its output through the app's own log.
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            log::warn!("[codex_usage] could not start the codex app server: {e}");
            return None;
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        log::warn!("[codex_usage] app server gave no stdin");
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        log::warn!("[codex_usage] app server gave no stdout");
        return None;
    };

    // `stdin` is held for the whole exchange on purpose. The app server exits
    // the moment its stdin closes, and it closes long before a round trip to
    // OpenAI returns — closing early gets the `initialize` reply and nothing
    // else, which reads exactly like a server that doesn't support the method.
    let exchange = async move {
        if let Err(e) = stdin.write_all(REQUESTS.as_bytes()).await {
            log::warn!("[codex_usage] could not write to the app server: {e}");
            return None;
        }
        if let Err(e) = stdin.flush().await {
            log::warn!("[codex_usage] could not flush to the app server: {e}");
            return None;
        }

        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let Ok(line) = line else { break };
            if let Some(usage) = parse_rate_limits_reply(&line) {
                return Some(usage);
            }
        }
        log::warn!("[codex_usage] app server closed without answering the rate-limits request");
        None
    };

    let usage = match exchange.with_timeout(EXCHANGE_TIMEOUT).await {
        Ok(usage) => usage,
        Err(_) => {
            log::warn!("[codex_usage] app server exchange timed out");
            None
        }
    };

    // The child exists only for this exchange and has no work left to finish.
    let _ = child.kill();

    match &usage {
        Some(usage) => log::debug!("[codex_usage] reading: {}", usage.pill_label()),
        None => log::debug!("[codex_usage] no reading; keeping the previous one"),
    }
    usage
}

/// Pick our rate-limits reply out of one line of app-server output.
///
/// Returns `None` for every other line — the server interleaves notifications
/// and other responses with it — and also for an error reply, which leaves the
/// previous reading in place rather than blanking the pill.
fn parse_rate_limits_reply(line: &str) -> Option<CodexUsage> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("id").and_then(Value::as_i64) != Some(RATE_LIMITS_REQUEST_ID) {
        return None;
    }
    if let Some(error) = value.get("error") {
        log::warn!("[codex_usage] app server refused the rate-limits request: {error}");
        return None;
    }
    parse_rate_limits(value.get("result")?)
}

/// Fold an `account/rateLimits/read` result into a reading.
fn parse_rate_limits(result: &Value) -> Option<CodexUsage> {
    let mut short_window: Option<CodexUsageWindow> = None;
    let mut weekly: Option<CodexUsageWindow> = None;
    let mut plan_type: Option<String> = None;

    // Prefer the per-bucket map: any bucket can be the binding constraint, and
    // the flat `rateLimits` field only mirrors one of them. Fall back to the
    // flat field so an older server still reports something.
    let buckets: Vec<(Option<String>, &Value)> =
        match result.get("rateLimitsByLimitId").and_then(Value::as_object) {
            Some(map) if !map.is_empty() => map
                .iter()
                .map(|(name, snapshot)| (Some(name.clone()), snapshot))
                .collect(),
            Some(_) | None => match result.get("rateLimits") {
                Some(snapshot) => vec![(
                    snapshot
                        .get("limitId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    snapshot,
                )],
                None => Vec::new(),
            },
        };

    for (name, snapshot) in buckets {
        if plan_type.is_none() {
            plan_type = snapshot
                .get("planType")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        // A bucket's display name beats the map key when it has one: the key is
        // an internal id (`codex_bengalfox`) and the name is the model
        // (`GPT-5.3-Codex-Spark`).
        let label = snapshot
            .get("limitName")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(name);

        for key in ["primary", "secondary"] {
            let Some(reported) = snapshot.get(key) else {
                continue;
            };
            let Some(used_pct) = reported.get("usedPercent").and_then(Value::as_f64) else {
                continue;
            };

            // Slot by the window's own stated length, never by whether the
            // server called it primary or secondary. On a Pro-lite account the
            // `codex` bucket's *primary* window is the weekly one, so trusting
            // the position would print a weekly figure under a "5h" label.
            let slot = match reported.get("windowDurationMins").and_then(Value::as_i64) {
                Some(mins) if mins <= SHORT_WINDOW_MAX_MINS => &mut short_window,
                Some(_) => &mut weekly,
                // A window with no stated length can't be placed at all.
                // Guessing weekly would hide a short-window squeeze behind a
                // reassuring number, so it is dropped instead.
                None => continue,
            };

            keep_worse(
                slot,
                CodexUsageWindow {
                    used_pct: used_pct as f32,
                    resets_at: reported.get("resetsAt").and_then(Value::as_i64),
                    bucket: label.clone(),
                },
            );
        }
    }

    // Nothing placeable means this wasn't a usable payload — no pill figures,
    // rather than a confident 0%.
    if short_window.is_none() && weekly.is_none() && plan_type.is_none() {
        return None;
    }

    let ordinary_usage_allowed = result.get("ordinaryUsageAllowed").and_then(Value::as_bool);
    let worst_pct = [short_window.as_ref(), weekly.as_ref()]
        .into_iter()
        .flatten()
        .map(|window| window.used_pct)
        .fold(0.0f32, f32::max);
    // The server's own "you may not spend included usage" verdict outranks the
    // percentages. Its schema is explicit that a client must not infer recovery
    // from a percentage, and the converse holds too: a blocked account can read
    // well under 100% and still be blocked.
    let severity = if ordinary_usage_allowed == Some(false) {
        UsageSeverity::Critical
    } else {
        UsageSeverity::from_pct(worst_pct)
    };

    Some(CodexUsage {
        short_window,
        weekly,
        plan_type,
        ordinary_usage_allowed,
        severity,
    })
}

/// Keep whichever of two readings for the same window slot is closer to its
/// limit, so the pill reports the binding constraint rather than an average.
fn keep_worse(slot: &mut Option<CodexUsageWindow>, candidate: CodexUsageWindow) {
    match slot {
        Some(existing) if existing.used_pct >= candidate.used_pct => {}
        _ => *slot = Some(candidate),
    }
}

#[cfg(test)]
#[path = "codex_usage_tests.rs"]
mod tests;
