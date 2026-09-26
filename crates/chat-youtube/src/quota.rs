//! Recognising "daily quota used up" and waiting for the reset.
//!
//! YouTube quota is per Google project and resets at midnight Pacific time.
//! Once it's used up, every call fails until then, so instead of failing
//! (or retrying pointlessly) the source sleeps until the reset.
//!
//! Both a used-up daily quota and a short-term rate limit can arrive as
//! "resource exhausted"; the second must *not* make us wait a whole day.

use std::fmt;
use std::time::Duration;

use chat_core::{Activity, Reporter};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use tonic::{Code, Status};

/// After the computed reset, give Google a minute to actually reset.
const RESET_GRACE: Duration = Duration::from_secs(60);

/// If the quota is still exhausted this soon after a reset, the reset hasn't
/// reached us yet: check again after `RECHECK` instead of waiting a day.
const JUST_RESET: chrono::Duration = chrono::Duration::minutes(30);
const RECHECK: Duration = Duration::from_secs(10 * 60);

/// The daily quota of the Google project is used up. Returned inside
/// `anyhow::Error` by REST calls; check with `downcast_ref`.
#[derive(Debug)]
pub struct QuotaExhausted(pub String);

impl fmt::Display for QuotaExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "YouTube API quota used up for today: {}", self.0)
    }
}

impl std::error::Error for QuotaExhausted {}

/// How a gRPC `ResourceExhausted` should be treated.
#[derive(Debug, PartialEq)]
pub(crate) enum Exhaustion {
    /// Daily quota: wait for the reset.
    Daily,
    /// Short-term rate limit: back off briefly and retry.
    RateLimit,
}

/// Classifies a failed gRPC call; `None` if it isn't about quota at all.
///
/// Google's messages look like "…exceeded your quota…" for the daily quota
/// and "…quota metric … per minute…" / "rate limit" for short-term limits.
/// When in doubt it's treated as a rate limit: retrying a bit too early costs
/// little, waiting a day by mistake would cost the stream.
pub(crate) fn classify(status: &Status) -> Option<Exhaustion> {
    if status.code() != Code::ResourceExhausted {
        return None;
    }
    let message = status.message().to_lowercase();
    let short_term = ["per minute", "per second", "rate limit", "ratelimit"]
        .iter()
        .any(|s| message.contains(s));
    Some(if !short_term && message.contains("quota") {
        Exhaustion::Daily
    } else {
        Exhaustion::RateLimit
    })
}

/// Whether a REST error body (`{"error": {"errors": [{"reason": …}]}}`)
/// reports the daily quota as used up.
pub(crate) fn is_quota_body(body: &str) -> bool {
    body.contains("\"quotaExceeded\"") || body.contains("\"dailyLimitExceeded\"")
}

/// When the daily quota resets next: midnight in Los Angeles, which is 08:00
/// UTC in winter and 07:00 UTC in summer (daylight saving time).
pub(crate) fn next_reset(now: DateTime<Utc>) -> DateTime<Utc> {
    let tomorrow = now
        .with_timezone(&Los_Angeles)
        .date_naive()
        .succ_opt()
        .expect("not the end of time");
    let midnight = tomorrow.and_hms_opt(0, 0, 0).expect("00:00:00 is valid");
    Los_Angeles
        // `earliest`: a local time can be ambiguous or missing around DST
        // switches. Los Angeles switches at 02:00, so midnight always
        // exists exactly once, but the API makes us say what we'd want.
        .from_local_datetime(&midnight)
        .earliest()
        .expect("midnight exists in Los Angeles")
        .with_timezone(&Utc)
}

/// How long to wait before trying again after the quota ran out.
pub(crate) fn wait_duration(now: DateTime<Utc>) -> Duration {
    let reset = next_reset(now);
    let previous_reset = reset - chrono::Duration::days(1);
    if now - previous_reset < JUST_RESET {
        return RECHECK;
    }
    (reset - now).to_std().unwrap_or_default() + RESET_GRACE
}

/// Reports and logs a clear message, then sleeps until the quota should be
/// back.
pub(crate) async fn wait_for_reset(detail: &str, activity: &Reporter) {
    let now = Utc::now();
    let wait = wait_duration(now);
    let resume = now + chrono::Duration::from_std(wait).unwrap_or_default();
    let resume = resume.with_timezone(&chrono::Local).format("%H:%M");
    activity.set(Activity::Blocked(format!(
        "YouTube quota used up for today; resuming at {resume}"
    )));
    tracing::warn!(
        "YouTube API quota used up for today ({detail}); resuming at {resume} (in {}h {:02}m). \
         The quota resets at midnight Pacific time.",
        wait.as_secs() / 3600,
        wait.as_secs() % 3600 / 60,
    );
    tokio::time::sleep(wait).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn reset_is_midnight_pacific_in_winter_and_summer() {
        // Winter: PST = UTC-8, so midnight is 08:00 UTC.
        assert_eq!(
            next_reset(utc("2026-01-15T10:00:00Z")),
            utc("2026-01-16T08:00:00Z")
        );
        // Summer: PDT = UTC-7, so midnight is 07:00 UTC.
        assert_eq!(
            next_reset(utc("2026-07-15T10:00:00Z")),
            utc("2026-07-16T07:00:00Z")
        );
        // 23:30 in Los Angeles: the reset is half an hour away, same UTC day.
        assert_eq!(
            next_reset(utc("2026-07-15T06:30:00Z")),
            utc("2026-07-15T07:00:00Z")
        );
        // Across the March DST switch (8 March 2026): the next midnight is
        // already PDT.
        assert_eq!(
            next_reset(utc("2026-03-08T12:00:00Z")),
            utc("2026-03-09T07:00:00Z")
        );
    }

    #[test]
    fn waits_until_the_reset_plus_grace() {
        let wait = wait_duration(utc("2026-07-15T05:00:00Z"));
        assert_eq!(wait, Duration::from_secs(2 * 3600) + RESET_GRACE);
    }

    #[test]
    fn right_after_a_reset_only_rechecks_soon() {
        // 10 minutes after the reset, still exhausted: Google hasn't caught
        // up yet. Don't wait a whole day.
        assert_eq!(wait_duration(utc("2026-07-15T07:10:00Z")), RECHECK);
    }

    #[test]
    fn classifies_resource_exhausted() {
        let daily = Status::resource_exhausted(
            "The request cannot be completed because you have exceeded your quota.",
        );
        assert_eq!(classify(&daily), Some(Exhaustion::Daily));

        let per_minute = Status::resource_exhausted(
            "Quota exceeded for quota metric 'Queries' and limit 'Queries per minute'",
        );
        assert_eq!(classify(&per_minute), Some(Exhaustion::RateLimit));

        let unclear = Status::resource_exhausted("too many requests");
        assert_eq!(classify(&unclear), Some(Exhaustion::RateLimit));

        assert_eq!(classify(&Status::not_found("x")), None);
    }

    #[test]
    fn recognises_rest_quota_errors() {
        let body = r#"{"error":{"code":403,"message":"The request cannot be completed because you have exceeded your quota.","errors":[{"message":"…","domain":"youtube.quota","reason":"quotaExceeded"}]}}"#;
        assert!(is_quota_body(body));
        assert!(!is_quota_body(
            r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#
        ));
    }
}
