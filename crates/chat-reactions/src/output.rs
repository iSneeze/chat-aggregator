//! What every output shares: the job a fired rule becomes and how it
//! reports back, the reactions waiting for a connection (`queue_secs`),
//! and the pace of reconnecting.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// A connection attempt that takes longer than this has failed: a LAN
/// address that doesn't answer would otherwise hang for minutes.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The wait after a failed attempt: 1 s, doubling up to this. Short, since
/// the program is usually on the same machine: once it's started,
/// reactions should work within seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// At most this many reactions wait for a connection; older ones go first.
const MAX_WAITING: usize = 32;

/// A fired rule's action, on its way to an output. `A` is what that output
/// needs to know (Warudo: an action name; OBS: an effect).
#[derive(Debug)]
pub(crate) struct Job<A> {
    /// For the log, and whose status the outcome goes to.
    pub(crate) rule: String,
    pub(crate) action: A,
    /// When the rule fired: how long it may still wait.
    pub(crate) fired_at: Instant,
    /// Who hears how it went.
    pub(crate) report: Report,
}

/// How a job went, once the program had it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Sent,
    /// The program answered with an error: a filter or source that doesn't
    /// exist (OBS). The streamer's to fix, so the rule's status shows it.
    Refused(String),
}

/// Where a job's outcome goes. A job dropped on the way (not connected,
/// waited too long) reports nothing: its `reply` is dropped with it, which
/// the waiting test notices.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The reactor, for the rule's status. Unbounded: an output must never
    /// wait for the reactor (which hands it jobs), and there are never more
    /// outcomes than jobs, which are bounded.
    pub(crate) results: Option<mpsc::UnboundedSender<(String, Outcome)>>,
    /// A test from the app, waiting for the answer.
    pub(crate) reply: Option<oneshot::Sender<Outcome>>,
}

impl<A> Job<A> {
    /// A job for `rule` fired at `fired_at`, reporting to nobody.
    pub(crate) fn new(rule: impl Into<String>, action: A, fired_at: Instant) -> Self {
        Self {
            rule: rule.into(),
            action,
            fired_at,
            report: Report::default(),
        }
    }

    /// Reports how it went, and logs it: a send quietly, unless it had to
    /// wait. `&mut`: the reply can only be sent once, so it's taken out.
    pub(crate) fn finish(&mut self, target: &str, outcome: Outcome) {
        match &outcome {
            Outcome::Sent => {
                let waited = Instant::now().duration_since(self.fired_at);
                if waited > Duration::from_secs(1) {
                    let secs = waited.as_secs();
                    info!(rule = %self.rule, "reaction sent to {target} after waiting {secs} s");
                } else {
                    debug!(rule = %self.rule, "reaction sent to {target}");
                }
            }
            Outcome::Refused(why) => {
                warn!(rule = %self.rule, "{target} refused the reaction: {why}");
            }
        }
        if let Some(results) = &self.report.results {
            // Gone only when the reactor is: then nobody needs it.
            let _ = results.send((self.rule.clone(), outcome.clone()));
        }
        if let Some(reply) = self.report.reply.take() {
            let _ = reply.send(outcome);
        }
    }
}

/// Drives `work` (connecting, or the pause before the next try) while
/// taking in the reactor's jobs, so its channel never fills up meanwhile.
/// `None`: the reactor closed the channel; stop.
pub(crate) async fn taking_jobs<F: Future, A>(
    work: F,
    jobs: &mut mpsc::Receiver<Job<A>>,
    waiting: &mut Waiting<A>,
) -> Option<F::Output> {
    // `select!` polls `work` again on every turn of the loop, so it must
    // stay in one place in memory: pinned (a future may point into itself).
    let mut work = std::pin::pin!(work);
    loop {
        tokio::select! {
            output = &mut work => return Some(output),
            job = jobs.recv() => waiting.push(job?),
        }
    }
}

/// How long to wait after `failures` failed attempts in a row.
pub(crate) fn backoff(failures: u32) -> Duration {
    let secs = 1u64 << failures.saturating_sub(1).min(5);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// The pause after `failures` failed attempts, cut short by `wake`: the
/// app stored a new password, so the streamer has most likely just fixed
/// what was wrong and shouldn't wait up to 30 s.
pub(crate) async fn pause(failures: u32, wake: &Notify) {
    tokio::select! {
        () = tokio::time::sleep(backoff(failures)) => {}
        () = wake.notified() => {}
    }
}

/// Reactions waiting for a connection, oldest first.
pub(crate) struct Waiting<A> {
    jobs: VecDeque<Job<A>>,
    /// How long one may wait (`queue_secs`); zero: none waits at all.
    max_age: Duration,
    /// The program's name, for the log.
    target: &'static str,
}

impl<A> Waiting<A> {
    pub(crate) fn new(target: &'static str, queue_secs: u32) -> Self {
        Self {
            jobs: VecDeque::new(),
            max_age: Duration::from_secs(queue_secs.into()),
            target,
        }
    }

    pub(crate) fn push(&mut self, job: Job<A>) {
        let target = self.target;
        if self.max_age.is_zero() {
            info!(rule = %job.rule, "{target} isn't connected: reaction dropped");
            return;
        }
        if self.jobs.len() == MAX_WAITING
            && let Some(oldest) = self.jobs.pop_front()
        {
            warn!(rule = %oldest.rule, "too many reactions waiting for {target}: dropped the oldest");
        }
        self.jobs.push_back(job);
    }

    pub(crate) fn extend(&mut self, job: Option<Job<A>>) {
        if let Some(job) = job {
            self.push(job);
        }
    }

    /// The oldest job that may still be sent at `now`; older ones are
    /// dropped on the way.
    pub(crate) fn next_fresh(&mut self, now: Instant) -> Option<Job<A>> {
        while let Some(job) = self.jobs.pop_front() {
            if now.duration_since(job.fired_at) <= self.max_age {
                return Some(job);
            }
            info!(rule = %job.rule, "reaction waited too long for {}: dropped", self.target);
        }
        None
    }

    /// Everything that may still be sent at `now`, oldest first.
    pub(crate) fn take_fresh(&mut self, now: Instant) -> Vec<Job<A>> {
        std::iter::from_fn(|| self.next_fresh(now)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(action: &str, fired_at: Instant) -> Job<String> {
        Job::new(action, action.to_string(), fired_at)
    }

    fn actions(waiting: &mut Waiting<String>, now: Instant) -> Vec<String> {
        waiting
            .take_fresh(now)
            .into_iter()
            .map(|j| j.action)
            .collect()
    }

    #[test]
    fn without_a_queue_nothing_waits() {
        let now = Instant::now();
        let mut waiting = Waiting::new("test", 0);
        waiting.push(job("a", now));
        assert!(actions(&mut waiting, now).is_empty());
    }

    #[test]
    fn the_queue_keeps_fresh_ones_in_order_and_drops_old_ones() {
        let start = Instant::now();
        let mut waiting = Waiting::new("test", 30);
        waiting.push(job("old", start));
        waiting.push(job("newer", start + Duration::from_secs(20)));
        waiting.push(job("newest", start + Duration::from_secs(25)));
        assert_eq!(
            actions(&mut waiting, start + Duration::from_secs(45)),
            ["newer", "newest"]
        );
    }

    #[test]
    fn finishing_reports_to_the_reactor_and_the_test() {
        let (results, mut results_rx) = mpsc::unbounded_channel();
        let (reply, mut reply_rx) = oneshot::channel();
        let mut job = job("bonk", Instant::now());
        job.report = Report {
            results: Some(results),
            reply: Some(reply),
        };
        job.finish("test", Outcome::Refused("no filter".into()));
        let refused = Outcome::Refused("no filter".into());
        assert_eq!(results_rx.try_recv(), Ok(("bonk".into(), refused.clone())));
        assert_eq!(reply_rx.try_recv(), Ok(refused));
    }

    #[test]
    fn the_queue_is_bounded() {
        let now = Instant::now();
        let mut waiting = Waiting::new("test", 30);
        for n in 0..MAX_WAITING + 3 {
            waiting.push(job(&n.to_string(), now));
        }
        let kept = actions(&mut waiting, now);
        assert_eq!(kept.len(), MAX_WAITING);
        assert_eq!(kept[0], "3", "the oldest went first");
    }

    #[tokio::test(start_paused = true)]
    async fn waking_cuts_the_pause_short() {
        let wake = Notify::new();
        let start = Instant::now();
        // Woken before the pause even began (the app was quick): it still
        // counts, `Notify` keeps it.
        wake.notify_one();
        pause(6, &wake).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        pause(6, &wake).await;
        assert_eq!(start.elapsed(), backoff(6), "unwoken, the full pause");
    }

    #[test]
    fn backoff_grows_to_its_cap() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
