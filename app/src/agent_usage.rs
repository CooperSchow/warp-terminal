//! Shared vocabulary for the CLI-agent subscription usage pills.
//!
//! Two providers are reported in the tab bar — Claude (see
//! [`crate::claude_usage`]) and Codex (see [`crate::codex_usage`]) — and each
//! polls a different source on its own cadence. What they agree on is how a
//! reading is *presented*: a percentage of a rolling window, coloured by how
//! close it sits to the limit. That agreement lives here so the two pills can't
//! drift into disagreeing about what "warning" means.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// How close to a limit we are, for colouring a pill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSeverity {
    #[default]
    Normal,
    Warning,
    Critical,
}

impl UsageSeverity {
    /// Map a provider's own severity string onto ours. Anthropic tags each
    /// active limit this way; Codex does not, and derives from the percentage.
    pub fn from_api(s: &str) -> UsageSeverity {
        match s {
            "warning" => UsageSeverity::Warning,
            "critical" | "blocked" | "exceeded" => UsageSeverity::Critical,
            _ => UsageSeverity::Normal,
        }
    }

    /// Fallback when a provider doesn't tag a severity: derive from the
    /// percentage. The thresholds are shared deliberately, so 80% means the
    /// same shade of amber whichever provider reported it.
    pub fn from_pct(pct: f32) -> UsageSeverity {
        if pct >= 90.0 {
            UsageSeverity::Critical
        } else if pct >= 70.0 {
            UsageSeverity::Warning
        } else {
            UsageSeverity::Normal
        }
    }

    /// The more severe of two readings. Used to fold several windows, or
    /// several metered buckets, into the one colour a pill can wear.
    pub fn combine(self, other: UsageSeverity) -> UsageSeverity {
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }
}

/// Elapsed wall-clock time between two instants, saturating at zero so a clock
/// adjustment can't make a reading look like it came from the future.
pub fn age(taken_at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(taken_at).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
#[path = "agent_usage_tests.rs"]
mod tests;
