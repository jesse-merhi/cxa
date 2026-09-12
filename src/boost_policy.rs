use chrono::DateTime;

use crate::account_store::{UsageBucket, UsageRecord, UsageWindow};
use crate::{Error, Result};

/// Model used during both the temporary boost and the post-boost baseline.
pub const BOOST_MODEL: &str = "gpt-6-astra";

/// Reasoning effort used while the temporary boost is active.
pub const BOOST_EFFORT: &str = "ultra";

/// Reasoning effort restored after the temporary boost ends.
pub const BASELINE_EFFORT: &str = "medium";

/// Codex service tier used to enable fast mode during the temporary boost.
pub const FAST_TIER: &str = "fast";

/// Explicitly opts out of a model catalog's default Fast tier, like `/fast off`.
pub const BASELINE_TIER: &str = "default";

const WEEKLY_WINDOW_MINUTES: i64 = 10_080;

/// Whether a fresh snapshot includes a usable regular Codex weekly window.
pub fn has_weekly_quota(usage: &UsageRecord) -> bool {
    usage.succeeded()
        && usage
            .buckets
            .iter()
            .filter(|bucket| is_regular_codex_bucket(bucket))
            .any(|bucket| {
                bucket.windows().any(|(_, window)| {
                    window.window_minutes == Some(WEEKLY_WINDOW_MINUTES)
                        && valid_percent(window.used_percent).is_some()
                })
            })
}

/// Parses an explicit RFC 3339 deadline and requires it to be later than `now`.
///
/// RFC 3339 requires a numeric UTC offset or `Z`, so a local time without a
/// timezone is rejected rather than interpreted using the machine timezone.
pub fn parse_deadline(value: &str, now: i64) -> Result<i64> {
    let deadline = DateTime::parse_from_rfc3339(value)
        .map_err(|_| {
            Error::Message(
                "Boost deadline must be RFC 3339 with a timezone, for example 2026-09-13T09:00:00+10:00."
                    .into(),
            )
        })?
        .timestamp();

    if deadline <= now {
        return Err(Error::Message(
            "Boost deadline must be in the future.".into(),
        ));
    }

    Ok(deadline)
}

/// Returns whether two observations provide a conservative signal that a regular
/// Codex weekly quota reset.
///
/// Records with errors, stale or repeated observation times, Spark buckets,
/// non-Codex buckets, non-weekly windows, and invalid percentages provide no
/// reset evidence. Matching windows signal a reset when usage drops, or when a
/// previously announced reset boundary falls between the observations and the
/// new observation announces a later boundary. A backend correction can also
/// lower usage; treating that as a reset deliberately ends the boost early. The
/// boundary rule covers a `0%` to `0%` rollover, which percentage comparison
/// cannot detect. `codex_bengalfox` is excluded because existing usage responses
/// identify it as the Codex Spark bucket even when its display name is absent.
pub fn weekly_reset_detected(previous: &UsageRecord, current: &UsageRecord) -> bool {
    if !previous.succeeded() || !current.succeeded() || current.observed_at <= previous.observed_at
    {
        return false;
    }

    for previous_bucket in previous
        .buckets
        .iter()
        .filter(|bucket| is_regular_codex_bucket(bucket))
    {
        let Some(current_bucket) = current.buckets.iter().find(|bucket| {
            bucket.limit_id == previous_bucket.limit_id && is_regular_codex_bucket(bucket)
        }) else {
            continue;
        };

        for (window_name, previous_window) in previous_bucket.windows() {
            let Some(current_window) = current_bucket
                .windows()
                .find_map(|(name, window)| (name == window_name).then_some(window))
            else {
                continue;
            };

            if weekly_window_reset(
                previous_window,
                current_window,
                previous.observed_at,
                current.observed_at,
            ) {
                return true;
            }
        }
    }

    false
}

fn is_regular_codex_bucket(bucket: &UsageBucket) -> bool {
    let id = bucket.limit_id.to_ascii_lowercase();
    let name = bucket
        .limit_name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    (id == "codex" || id.starts_with("codex_"))
        && id != "codex_bengalfox"
        && !name.contains("spark")
}

fn weekly_window_reset(
    previous: &UsageWindow,
    current: &UsageWindow,
    previous_observed_at: i64,
    current_observed_at: i64,
) -> bool {
    if previous.window_minutes != Some(WEEKLY_WINDOW_MINUTES)
        || current.window_minutes != Some(WEEKLY_WINDOW_MINUTES)
    {
        return false;
    }

    let (Some(previous_percent), Some(current_percent)) = (
        valid_percent(previous.used_percent),
        valid_percent(current.used_percent),
    ) else {
        return false;
    };

    if current_percent < previous_percent {
        return true;
    }

    matches!(
        (previous.resets_at, current.resets_at),
        (Some(previous_reset), Some(current_reset))
            if previous_observed_at <= previous_reset
                && previous_reset <= current_observed_at
                && current_reset > previous_reset
    )
}

fn valid_percent(percent: Option<f64>) -> Option<f64> {
    percent.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used_percent: f64, resets_at: Option<i64>) -> UsageWindow {
        UsageWindow {
            used_percent: Some(used_percent),
            resets_at,
            window_minutes: Some(WEEKLY_WINDOW_MINUTES),
        }
    }

    fn usage(observed_at: i64, bucket: UsageBucket) -> UsageRecord {
        UsageRecord {
            observed_at,
            last_attempted_at: observed_at,
            buckets: vec![bucket],
            error: None,
        }
    }

    fn bucket(limit_id: &str, limit_name: Option<&str>, weekly: UsageWindow) -> UsageBucket {
        UsageBucket {
            limit_id: limit_id.into(),
            limit_name: limit_name.map(Into::into),
            secondary_window: Some(weekly),
            ..UsageBucket::default()
        }
    }

    #[test]
    fn parses_timezone_qualified_future_deadline() {
        let deadline = parse_deadline("2026-09-13T09:00:00+10:00", 1_789_200_000).unwrap();

        assert_eq!(deadline, 1_789_254_000);
    }

    #[test]
    fn rejects_deadline_without_timezone_or_in_the_past() {
        assert!(parse_deadline("2026-09-13T09:00:00", 0).is_err());
        assert!(parse_deadline("2026-09-13T09:00:00Z", 1_789_290_000).is_err());
    }

    #[test]
    fn detects_weekly_usage_drop_in_regular_codex_bucket() {
        let previous = usage(100, bucket("codex", None, window(64.0, None)));
        let current = usage(200, bucket("codex", None, window(3.0, None)));

        assert!(weekly_reset_detected(&previous, &current));
    }

    #[test]
    fn weekly_monitoring_requires_usable_regular_quota() {
        let mut snapshot = usage(100, bucket("codex", None, window(64.0, None)));
        assert!(has_weekly_quota(&snapshot));

        snapshot.buckets[0]
            .secondary_window
            .as_mut()
            .unwrap()
            .used_percent = None;
        assert!(!has_weekly_quota(&snapshot));
        snapshot.buckets[0].secondary_window = Some(window(64.0, None));
        snapshot.buckets[0].limit_id = "codex_bengalfox".into();
        assert!(!has_weekly_quota(&snapshot));
        snapshot.buckets[0].limit_id = "codex".into();
        snapshot.error = Some("quota unavailable".into());
        assert!(!has_weekly_quota(&snapshot));
    }

    #[test]
    fn detects_zero_to_zero_rollover_at_announced_boundary() {
        let previous = usage(100, bucket("codex", None, window(0.0, Some(150))));
        let current = usage(200, bucket("codex", None, window(0.0, Some(750))));

        assert!(weekly_reset_detected(&previous, &current));
    }

    #[test]
    fn ignores_spark_and_non_codex_buckets() {
        for candidate in [
            bucket("codex_bengalfox", None, window(80.0, None)),
            bucket(
                "codex_special",
                Some("GPT-5.3-Codex-Spark"),
                window(80.0, None),
            ),
            bucket("api", Some("API"), window(80.0, None)),
        ] {
            let previous = usage(100, candidate.clone());
            let mut current_bucket = candidate;
            current_bucket.secondary_window = Some(window(1.0, None));
            let current = usage(200, current_bucket);

            assert!(!weekly_reset_detected(&previous, &current));
        }
    }

    #[test]
    fn requires_fresh_successful_observations_with_valid_weekly_percentages() {
        let previous = usage(100, bucket("codex", None, window(80.0, None)));

        let mut stale = usage(100, bucket("codex", None, window(1.0, None)));
        assert!(!weekly_reset_detected(&previous, &stale));

        stale.observed_at = 200;
        stale.error = Some("quota unavailable".into());
        assert!(!weekly_reset_detected(&previous, &stale));

        let missing = UsageRecord {
            observed_at: 200,
            ..UsageRecord::default()
        };
        assert!(!weekly_reset_detected(&previous, &missing));

        let invalid = usage(200, bucket("codex", None, window(f64::NAN, None)));
        assert!(!weekly_reset_detected(&previous, &invalid));

        let mut five_hour = window(1.0, None);
        five_hour.window_minutes = Some(300);
        let wrong_duration = usage(200, bucket("codex", None, five_hour));
        assert!(!weekly_reset_detected(&previous, &wrong_duration));
    }

    #[test]
    fn reset_timestamp_advance_without_crossing_boundary_is_not_enough() {
        let previous = usage(100, bucket("codex", None, window(40.0, Some(300))));
        let current = usage(200, bucket("codex", None, window(40.0, Some(900))));

        assert!(!weekly_reset_detected(&previous, &current));
    }
}
