use std::time::{Duration, SystemTime};

use serde_json::json;

use super::{
    parse_rate_limits, parse_rate_limits_reply, CodexUsage, CodexUsageModel, CodexUsageWindow,
    MAX_DISPLAY_AGE, PLACEHOLDER_LABEL, POLL_INTERVAL, STALE_AFTER,
};
use crate::agent_usage::UsageSeverity;

/// The real `account/rateLimits/read` result from a Pro-lite account: the plain
/// `codex` bucket is metered on a *weekly* primary window with no secondary at
/// all, while a model-specific bucket carries both. Slotting by position rather
/// than by `windowDurationMins` would label the weekly figure "5h" here, which
/// is the whole reason this payload is the fixture.
fn prolite_result() -> serde_json::Value {
    json!({
        "ordinaryUsageAllowed": true,
        "rateLimits": {
            "limitId": "codex",
            "primary": {"usedPercent": 4, "windowDurationMins": 10080, "resetsAt": 1789795594},
            "secondary": null,
            "planType": "prolite"
        },
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "limitName": null,
                "primary": {"usedPercent": 4, "windowDurationMins": 10080, "resetsAt": 1789795594},
                "secondary": null,
                "planType": "prolite"
            },
            "codex_bengalfox": {
                "limitId": "codex_bengalfox",
                "limitName": "GPT-5.3-Codex-Spark",
                "primary": {"usedPercent": 21, "windowDurationMins": 300, "resetsAt": 1789208794},
                "secondary": {"usedPercent": 2, "windowDurationMins": 10080, "resetsAt": 1789795001},
                "planType": "prolite"
            }
        },
        "accountId": "an-account-id"
    })
}

#[test]
fn slots_each_window_by_its_stated_length_not_its_position() {
    let usage = parse_rate_limits(&prolite_result()).expect("a usable payload");

    // 300-minute window -> the 5-hour slot, from the model bucket.
    let short = usage.short_window.as_ref().expect("a short window");
    assert_eq!(short.used_pct, 21.0);
    assert_eq!(short.resets_at, Some(1789208794));

    // The two weekly windows (4% and 2%) fold to the worse one.
    let weekly = usage.weekly.as_ref().expect("a weekly window");
    assert_eq!(weekly.used_pct, 4.0);

    assert_eq!(usage.plan_type.as_deref(), Some("prolite"));
    assert_eq!(usage.ordinary_usage_allowed, Some(true));
    assert_eq!(usage.severity, UsageSeverity::Normal);
    assert_eq!(usage.pill_label(), "5h 21% · wk 4%");
}

#[test]
fn reports_the_binding_bucket_and_names_it_when_it_is_not_the_plain_one() {
    let usage = parse_rate_limits(&prolite_result()).expect("a usable payload");
    let tooltip = usage.tooltip_text();

    assert!(tooltip.starts_with("Codex usage (Pro Lite)"), "{tooltip}");
    // The short window came from the model-specific bucket, so it is named.
    assert!(tooltip.contains("5-hour limit: 21%"), "{tooltip}");
    assert!(tooltip.contains("(GPT-5.3-Codex-Spark)"), "{tooltip}");
    // The weekly window came from the plain `codex` bucket, which isn't worth
    // naming on every line.
    assert!(tooltip.contains("Weekly limit: 4%"), "{tooltip}");
    assert!(!tooltip.contains("(codex)"), "{tooltip}");
    assert!(tooltip.contains("resets"), "{tooltip}");
}

#[test]
fn a_missing_window_is_absent_rather_than_zero() {
    let result = json!({
        "ordinaryUsageAllowed": true,
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "primary": {"usedPercent": 63, "windowDurationMins": 10080, "resetsAt": 1789795594},
                "secondary": null,
                "planType": "plus"
            }
        }
    });

    let usage = parse_rate_limits(&result).expect("a usable payload");
    // Reporting an unmeasured window as 0% would read as "plenty of room".
    assert!(usage.short_window.is_none());
    assert_eq!(usage.weekly.as_ref().map(|w| w.used_pct), Some(63.0));
    assert_eq!(usage.pill_label(), "wk 63%");
    assert_eq!(usage.severity, UsageSeverity::Normal);
}

#[test]
fn drops_a_window_whose_length_the_server_did_not_state() {
    let result = json!({
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "primary": {"usedPercent": 88},
                "planType": "plus"
            }
        }
    });

    let usage = parse_rate_limits(&result).expect("plan type still makes it usable");
    assert!(usage.short_window.is_none());
    assert!(usage.weekly.is_none());
    assert_eq!(usage.pill_label(), PLACEHOLDER_LABEL);
    assert!(usage.tooltip_text().contains("No metered window reported"));
}

#[test]
fn a_blocked_account_is_critical_however_low_the_percentages_read() {
    let result = json!({
        // The server's own verdict: included usage may not be spent. Its schema
        // is explicit that a client must not infer state from percentages.
        "ordinaryUsageAllowed": false,
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "primary": {"usedPercent": 11, "windowDurationMins": 10080},
                "planType": "plus"
            }
        }
    });

    let usage = parse_rate_limits(&result).expect("a usable payload");
    assert_eq!(usage.severity, UsageSeverity::Critical);
    assert!(usage
        .tooltip_text()
        .contains("Included usage is currently blocked."));
}

#[test]
fn derives_severity_from_the_worst_window() {
    for (short_pct, weekly_pct, expected) in [
        (10.0, 10.0, UsageSeverity::Normal),
        (75.0, 10.0, UsageSeverity::Warning),
        (10.0, 75.0, UsageSeverity::Warning),
        (95.0, 10.0, UsageSeverity::Critical),
        (10.0, 95.0, UsageSeverity::Critical),
    ] {
        let result = json!({
            "ordinaryUsageAllowed": true,
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "primary": {"usedPercent": short_pct, "windowDurationMins": 300},
                    "secondary": {"usedPercent": weekly_pct, "windowDurationMins": 10080},
                    "planType": "plus"
                }
            }
        });
        let usage = parse_rate_limits(&result).expect("a usable payload");
        assert_eq!(
            usage.severity, expected,
            "short={short_pct} weekly={weekly_pct}"
        );
    }
}

#[test]
fn falls_back_to_the_flat_field_when_there_is_no_bucket_map() {
    let result = json!({
        "ordinaryUsageAllowed": true,
        "rateLimits": {
            "limitId": "codex",
            "primary": {"usedPercent": 30, "windowDurationMins": 300, "resetsAt": 1789208794},
            "secondary": {"usedPercent": 7, "windowDurationMins": 10080},
            "planType": "plus"
        },
        "rateLimitsByLimitId": {}
    });

    let usage = parse_rate_limits(&result).expect("a usable payload");
    assert_eq!(usage.pill_label(), "5h 30% · wk 7%");
    assert_eq!(usage.plan_type.as_deref(), Some("plus"));
}

#[test]
fn rejects_a_payload_with_nothing_usable_in_it() {
    assert!(parse_rate_limits(&json!({})).is_none());
    assert!(parse_rate_limits(&json!({"rateLimitsByLimitId": {}})).is_none());
    assert!(parse_rate_limits(&json!({"rateLimits": {"primary": null}})).is_none());
}

#[test]
fn picks_our_reply_out_of_the_app_servers_line_traffic() {
    let reply = json!({"jsonrpc": "2.0", "id": 2, "result": prolite_result()}).to_string();
    assert!(parse_rate_limits_reply(&reply).is_some());

    for other in [
        // The initialize reply.
        json!({"jsonrpc": "2.0", "id": 1, "result": {"userAgent": "x"}}).to_string(),
        // A notification the server interleaves with responses.
        json!({"jsonrpc": "2.0", "method": "remoteControl/status/changed"}).to_string(),
        // An error reply: leaves the previous reading in place.
        json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32601, "message": "no"}}).to_string(),
        // Not JSON at all.
        "warning: something on stdout".to_string(),
        String::new(),
    ] {
        assert!(
            parse_rate_limits_reply(&other).is_none(),
            "expected to be ignored: {other}"
        );
    }
}

fn reading(pct: f32) -> CodexUsage {
    CodexUsage {
        short_window: None,
        weekly: Some(CodexUsageWindow {
            used_pct: pct,
            resets_at: None,
            bucket: None,
        }),
        plan_type: Some("plus".to_string()),
        ordinary_usage_allowed: Some(true),
        severity: UsageSeverity::from_pct(pct),
    }
}

#[test]
fn keeps_a_cached_reading_that_is_still_recent_and_marks_it_stale() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    let taken_at = now - Duration::from_secs(60);

    let model = CodexUsageModel::restoring(Some((reading(12.0), taken_at)), now);
    assert_eq!(model.usage().map(|u| u.pill_label()), Some("wk 12%".into()));
    // Shown, but its colour isn't vouched for until a live poll confirms it.
    assert!(model.is_stale());
}

#[test]
fn discards_a_cached_reading_that_has_aged_out() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let taken_at = now - MAX_DISPLAY_AGE;

    let model = CodexUsageModel::restoring(Some((reading(12.0), taken_at)), now);
    assert!(model.usage().is_none());
    assert!(!model.is_stale());
}

#[test]
fn a_failed_poll_keeps_the_last_reading_then_ages_it_out() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let mut model = CodexUsageModel::restoring(None, start);

    assert_eq!(model.record_poll(Some(reading(20.0)), start), POLL_INTERVAL);
    assert!(!model.is_stale());

    // A fresh failure keeps the figures and keeps vouching for them. It also
    // doesn't stretch the cadence: a single miss is noise, so the backoff only
    // starts once failures repeat (covered by the backoff test below).
    let soon = start + Duration::from_secs(60);
    assert_eq!(model.record_poll(None, soon), POLL_INTERVAL);
    assert_eq!(model.usage().map(|u| u.pill_label()), Some("wk 20%".into()));
    assert!(!model.is_stale());

    // Past the stale threshold they stay on screen but lose their colour.
    let later = start + STALE_AFTER;
    model.record_poll(None, later);
    assert!(model.usage().is_some());
    assert!(model.is_stale());

    // Past the display threshold they go away entirely.
    let much_later = start + MAX_DISPLAY_AGE;
    model.record_poll(None, much_later);
    assert!(model.usage().is_none());
    assert!(!model.is_stale());
}

#[test]
fn backoff_doubles_while_polls_fail_and_resets_on_success() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let mut model = CodexUsageModel::restoring(None, now);

    let first = model.record_poll(None, now);
    let second = model.record_poll(None, now);
    let third = model.record_poll(None, now);
    assert_eq!(first, POLL_INTERVAL);
    assert_eq!(second, POLL_INTERVAL * 2);
    assert_eq!(third, POLL_INTERVAL * 4);

    // A run of failures can't push the delay past the ceiling.
    for _ in 0..40 {
        assert!(model.record_poll(None, now) <= super::MAX_POLL_BACKOFF);
    }

    assert_eq!(model.record_poll(Some(reading(5.0)), now), POLL_INTERVAL);
}

#[test]
fn round_trips_a_reading_through_the_disk_cache() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("nested").join("codex_usage_cache.json");
    let taken_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    super::save_cached_reading_to(&path, &reading(42.0), taken_at);
    let (restored, restored_at) = super::load_cached_reading_from(&path).expect("cached reading");

    assert_eq!(restored.pill_label(), "wk 42%");
    assert_eq!(restored.severity, UsageSeverity::Normal);
    assert_eq!(restored_at, taken_at);
}

#[test]
fn a_missing_or_corrupt_cache_reads_as_no_reading() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let missing = dir.path().join("absent.json");
    assert!(super::load_cached_reading_from(&missing).is_none());

    let corrupt = dir.path().join("corrupt.json");
    std::fs::write(&corrupt, b"{not json").expect("write");
    assert!(super::load_cached_reading_from(&corrupt).is_none());
}
