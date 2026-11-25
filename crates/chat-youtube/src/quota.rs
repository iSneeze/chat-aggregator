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
use jiff::tz::{self, TimeZone};
use jiff::{SignedDuration, Timestamp};
use tonic::{Code, Status};

/// Where the quota day starts and ends. `get!` builds the zone's rules into
/// the binary at compile time, so it can't be missing on the user's machine
/// (Windows has no time zone database jiff could read at runtime anyway).
static LOS_ANGELES: TimeZone = tz::get!("America/Los_Angeles");

/// After the computed reset, give Google a minute to actually reset.
const RESET_GRACE: Duration = Duration::from_secs(60);

/// If the quota is still exhausted this soon after a reset, the reset hasn't
/// reached us yet: check again after `RECHECK` instead of waiting a day.
const JUST_RESET: SignedDuration = SignedDuration::from_mins(30);
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
pub(crate) fn next_reset(now: Timestamp) -> Timestamp {
    // `start_of_day` is the first moment of that day. Los Angeles switches
    // DST at 02:00, so midnight always exists exactly once; in a zone where
    // it didn't, jiff would pick the first time that does exist.
    now.to_zoned(LOS_ANGELES.clone())
        .tomorrow()
        .and_then(|day| day.start_of_day())
        .expect("not the end of time")
        .timestamp()
}

/// How long to wait before trying again after the quota ran out.
pub(crate) fn wait_duration(now: Timestamp) -> Duration {
    let reset = next_reset(now);
    // Midnight today in Los Angeles. Not `reset` minus 24 h: the days of
    // the DST switches are 23 and 25 hours long.
    let previous_reset = now
        .to_zoned(LOS_ANGELES.clone())
        .start_of_day()
        .expect("not the end of time")
        .timestamp();
    if now.duration_since(previous_reset) < JUST_RESET {
        return RECHECK;
    }
    // `try_from` fails only for a negative wait, which `next_reset` (always
    // in the future) doesn't produce.
    Duration::try_from(reset.duration_since(now)).unwrap_or_default() + RESET_GRACE
}

/// Maximum chunk for a single `tokio::time::sleep` while waiting for the daily
/// reset. `tokio::time::sleep` uses the monotonic clock, which pauses while a
/// Linux machine is suspended; waking periodically to compare against the wall
/// clock ensures we resume on time after laptop sleep.
const SLEEP_CHUNK: Duration = Duration::from_secs(5 * 60);

/// Reports and logs a clear message, then sleeps until the quota should be
/// back.
pub(crate) async fn wait_for_reset(detail: &str, activity: &Reporter) {
    let now = Timestamp::now();
    let wait = wait_duration(now);
    let resume_at = now + wait;
    // The user's own clock time; `wait` is at most a day, far from overflow.
    let resume = resume_at.to_zoned(TimeZone::system());
    let resume = resume.strftime("%H:%M");
    activity.set(Activity::Blocked(format!(
        "YouTube quota used up for today; resuming at {resume}"
    )));
    tracing::warn!(
        "YouTube API quota used up for today ({detail}); resuming at {resume} (in {}h {:02}m). \
         The quota resets at midnight Pacific time.",
        wait.as_secs() / 3600,
        wait.as_secs() % 3600 / 60,
    );
    sleep_until_wall_clock(resume_at).await;
}

/// Sleeps until wall-clock `target`, waking at most every [`SLEEP_CHUNK`] to
/// account for system suspend pausing the monotonic clock.
async fn sleep_until_wall_clock(target: Timestamp) {
    loop {
        let remaining = target.duration_since(Timestamp::now());
        let Ok(remaining) = Duration::try_from(remaining) else {
            return;
        };
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining.min(SLEEP_CHUNK)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> Timestamp {
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
    fn wait_duration_handles_reset_grace_recheck_and_dst() {
        assert_eq!(
            wait_duration(utc("2026-07-15T05:00:00Z")),
            Duration::from_secs(2 * 3600) + RESET_GRACE
        );
        // 10 minutes after the reset, still exhausted: recheck soon instead of waiting a day.
        assert_eq!(wait_duration(utc("2026-07-15T07:10:00Z")), RECHECK);
        // 8 March 2026 has 23 hours in Los Angeles: 20 minutes after midnight PST, recheck soon.
        assert_eq!(wait_duration(utc("2026-03-08T08:20:00Z")), RECHECK);
        // 1 November 2026 has 25 hours: 70 minutes after midnight PDT, wait for tomorrow's PST reset.
        assert_eq!(
            wait_duration(utc("2026-11-01T08:10:00Z")),
            Duration::from_secs(23 * 3600 + 50 * 60) + RESET_GRACE
        );
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
