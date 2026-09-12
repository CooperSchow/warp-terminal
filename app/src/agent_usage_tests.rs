use std::time::{Duration, SystemTime};

use super::{age, UsageSeverity};

#[test]
fn maps_provider_severity_strings() {
    assert_eq!(UsageSeverity::from_api("warning"), UsageSeverity::Warning);
    assert_eq!(UsageSeverity::from_api("critical"), UsageSeverity::Critical);
    assert_eq!(UsageSeverity::from_api("blocked"), UsageSeverity::Critical);
    assert_eq!(UsageSeverity::from_api("exceeded"), UsageSeverity::Critical);
    assert_eq!(UsageSeverity::from_api("anything"), UsageSeverity::Normal);
}

#[test]
fn derives_severity_from_a_percentage_at_shared_thresholds() {
    assert_eq!(UsageSeverity::from_pct(0.0), UsageSeverity::Normal);
    assert_eq!(UsageSeverity::from_pct(69.9), UsageSeverity::Normal);
    assert_eq!(UsageSeverity::from_pct(70.0), UsageSeverity::Warning);
    assert_eq!(UsageSeverity::from_pct(89.9), UsageSeverity::Warning);
    assert_eq!(UsageSeverity::from_pct(90.0), UsageSeverity::Critical);
    assert_eq!(UsageSeverity::from_pct(140.0), UsageSeverity::Critical);
}

#[test]
fn combine_keeps_the_worse_of_two_readings_either_way_round() {
    use UsageSeverity::{Critical, Normal, Warning};
    for (a, b, worst) in [
        (Normal, Warning, Warning),
        (Warning, Normal, Warning),
        (Warning, Critical, Critical),
        (Critical, Warning, Critical),
        (Normal, Normal, Normal),
        (Critical, Critical, Critical),
    ] {
        assert_eq!(a.combine(b), worst, "combine({a:?}, {b:?})");
    }
}

#[test]
fn age_saturates_rather_than_reporting_a_reading_from_the_future() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(400);
    let later = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);

    assert_eq!(age(earlier, now), Duration::from_secs(600));
    assert_eq!(age(later, now), Duration::ZERO);
    assert_eq!(age(now, now), Duration::ZERO);
}
