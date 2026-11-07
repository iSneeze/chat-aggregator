//! The reactor: one task that reads the live chat, asks the rules what
//! fires, and hands that to the outputs.
//!
//! It never waits for an output: actions go in with `try_send`, and an
//! output that's down or busy loses them (or queues them itself, see
//! `queue_secs`). So a slow or missing Warudo can't hold up the reactor,
//! and a flood of `!bonk`s doesn't pile up into a burst that all fires once
//! Warudo is back.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chat_core::secrets::SecretStore;
use chat_core::tiers::TipSettings;
use chat_core::{ChatEvent, ChatMessage, Hub};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::obs::{self, Effect};
use crate::output::{Job, Outcome, Report};
use crate::rules::{Action, Rules, Target};
use crate::warudo;
use crate::{ObsSettings, ReactionSettings, WarudoSettings};

/// How many actions may be on their way to an output at once. Beyond that
/// the output isn't keeping up, and more would only arrive late.
const OUTPUT_BUFFER: usize = 16;
/// At shutdown, how long an output may take to finish (OBS switching its
/// timed effects back off) before it's cut off.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a test waits for the program's answer. OBS needs a few
/// requests for one effect, each answered within milliseconds when all is
/// well.
const TEST_ANSWER: Duration = Duration::from_secs(15);

/// What the reactor reports, for status displays.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReactionStatus {
    /// The outputs some enabled rule sends to; the others aren't connected.
    pub outputs: Vec<OutputStatus>,
    /// Every rule, in rule order.
    pub rules: Vec<RuleStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleStatus {
    pub name: String,
    /// How often chat fired it since the engine started (tests don't
    /// count).
    pub fired: u64,
    /// Why the program refused its last reaction (OBS: a filter or source
    /// that doesn't exist); cleared by the next one that works.
    pub problem: Option<String>,
}

/// What the app asks of the reactor, besides new settings.
#[derive(Debug)]
pub enum ReactionRequest {
    /// Carries out the saved rule of this name now, without a trigger or
    /// guards, also when it's switched off or reactions are paused. Only
    /// while its program is connected (a test doesn't start an output);
    /// the answer is `Err` with a reason for the streamer otherwise.
    Test {
        rule: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Try to connect to `Target` now instead of after the pause between
    /// attempts: a new password was stored.
    Retry(Target),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputStatus {
    pub target: Target,
    pub state: OutputState,
}

/// An output's connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputState {
    Connecting,
    Connected,
    /// Can't connect (the program isn't started, a wrong password); trying
    /// again by itself.
    Retrying {
        error: String,
    },
}

impl OutputStatus {
    /// One line for next to the status light.
    pub fn summary(&self) -> String {
        match &self.state {
            OutputState::Connecting => "connecting…".into(),
            OutputState::Connected => "connected".into(),
            OutputState::Retrying { error } => {
                format!("can't connect, trying again: {error}")
            }
        }
    }
}

/// Reacts to what's published in `hub` from now on, with the rules in
/// `settings` (watched: edits apply right away), and reports to `status`.
/// `tips`: the streamer's tip settings, for amount rules (watched too).
/// `secrets`: where the OBS password is (`None`: nowhere). `requests`: the
/// app's tests and nudges.
///
/// Runs until `shutdown`, then stops the outputs and waits (briefly) for
/// them: OBS switches its timed effects back off.
pub async fn run(
    hub: Arc<Hub>,
    mut settings: watch::Receiver<ReactionSettings>,
    tips: watch::Receiver<TipSettings>,
    status: watch::Sender<ReactionStatus>,
    secrets: Option<SecretStore>,
    mut requests: mpsc::Receiver<ReactionRequest>,
    shutdown: CancellationToken,
) {
    // The replay history is dropped on purpose: a reaction belongs to the
    // moment a message arrives. Reacting to history would fire everything
    // again after every restart.
    let (_history, mut events) = hub.subscribe();
    let initial = settings.borrow_and_update().clone();
    let mut reactor = Reactor::new(initial, tips, status, secrets);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(ChatEvent::Message(msg)) => reactor.react(&msg, Instant::now()),
                // Moderation: nothing to take back, the reaction has happened.
                Ok(_) => {}
                Err(RecvError::Lagged(missed)) => {
                    warn!("reactions fell behind the chat: {missed} events skipped");
                }
                Err(RecvError::Closed) => break,
            },
            changed = settings.changed() => {
                if changed.is_err() {
                    break;
                }
                let new = settings.borrow_and_update().clone();
                reactor.apply(new);
            }
            Some(request) = requests.recv() => {
                // A rule saved a moment ago and tested right away: the
                // settings may have changed without this loop seeing it
                // yet (`select!` picks among ready branches at random).
                if settings.has_changed().unwrap_or(false) {
                    let new = settings.borrow_and_update().clone();
                    reactor.apply(new);
                }
                reactor.request(request);
            }
            Some((rule, outcome)) = reactor.results.recv() => reactor.record(rule, outcome),
            () = output_changed(&mut reactor.warudo) => {}
            () = output_changed(&mut reactor.obs) => {}
            () = shutdown.cancelled() => break,
        }
        reactor.publish();
    }
    reactor.stop().await;
}

fn tip_currency(tips: &mut watch::Receiver<TipSettings>) -> Option<String> {
    tips.borrow_and_update().currency().map(str::to_owned)
}

struct Reactor {
    rules: Rules,
    settings: ReactionSettings,
    /// Read when a message arrives rather than in the loop's `select!`:
    /// only amount rules need it, and only then.
    tips: watch::Receiver<TipSettings>,
    /// Each running while some enabled rule sends to it.
    warudo: Option<Output<WarudoSettings, warudo::Job>>,
    obs: Option<Output<ObsSettings, obs::Job>>,
    secrets: Option<SecretStore>,
    status: watch::Sender<ReactionStatus>,
    /// How the outputs' jobs went (the sender goes into every job).
    results_tx: mpsc::UnboundedSender<(String, Outcome)>,
    results: mpsc::UnboundedReceiver<(String, Outcome)>,
    /// Each rule's last refusal, by name.
    problems: HashMap<String, String>,
}

/// A running output task and the reactor's ends of its channels.
///
/// Dropping it (rules changed, new settings) closes the jobs channel, and
/// the task ends by itself: after switching back what it switched on.
/// That's why the task isn't aborted.
struct Output<S, J> {
    /// What it was started with: different settings, a new connection.
    settings: S,
    jobs: mpsc::Sender<J>,
    state: watch::Receiver<OutputState>,
    /// Cuts its pause between connection attempts short.
    wake: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Reactor {
    fn new(
        settings: ReactionSettings,
        tips: watch::Receiver<TipSettings>,
        status: watch::Sender<ReactionStatus>,
        secrets: Option<SecretStore>,
    ) -> Self {
        let (results_tx, results) = mpsc::unbounded_channel();
        let mut reactor = Self {
            rules: Rules::new(&settings),
            settings,
            tips,
            warudo: None,
            obs: None,
            secrets,
            status,
            results_tx,
            results,
            problems: HashMap::new(),
        };
        // A fresh receiver counts its first value as seen (`has_changed` is
        // false), so it's read here once.
        reactor.rules.tip_currency = tip_currency(&mut reactor.tips);
        reactor.start_outputs();
        reactor.publish();
        reactor
    }

    fn apply(&mut self, settings: ReactionSettings) {
        self.rules.update(&settings);
        // A changed rule may well be the fix: a problem is only kept for a
        // rule whose action is still the one OBS refused.
        let old = std::mem::take(&mut self.settings.rules);
        self.problems.retain(|name, _| {
            let action = |rules: &[crate::Rule]| {
                rules
                    .iter()
                    .find(|r| r.name == *name)
                    .map(|r| r.action.clone())
            };
            action(&settings.rules).is_some_and(|new| Some(new) == action(&old))
        });
        self.settings = settings;
        self.start_outputs();
    }

    /// Starts the outputs the rules need (and restarts those whose
    /// settings changed), and stops the others.
    fn start_outputs(&mut self) {
        let needed = |target| {
            self.rules
                .rules()
                .iter()
                .any(|rule| rule.enabled && rule.action.target() == target)
        };
        let (needs_warudo, needs_obs) = (needed(Target::Warudo), needed(Target::Obs));
        let warudo = self.settings.warudo.clone();
        keep(
            &mut self.warudo,
            needs_warudo,
            warudo,
            Target::Warudo,
            warudo::run,
        );
        let secrets = self.secrets.clone();
        let obs = self.settings.obs.clone();
        keep(
            &mut self.obs,
            needs_obs,
            obs,
            Target::Obs,
            |settings, jobs, state, wake| obs::run(settings, secrets, jobs, state, wake),
        );
    }

    fn react(&mut self, msg: &ChatMessage, now: Instant) {
        // Copied over only when changed: a new currency set in the app
        // applies from the next message on.
        if self.tips.has_changed().unwrap_or(false) {
            self.rules.tip_currency = tip_currency(&mut self.tips);
        }
        for fire in self.rules.check(msg, now) {
            // Only the rule's name: chatter names stay out of the log.
            info!(rule = %fire.rule, "reaction fired");
            if let Err(e) = self.dispatch(fire.rule.clone(), fire.action, now, None) {
                warn!(rule = %fire.rule, "{e}: reaction dropped");
            }
        }
    }

    fn request(&mut self, request: ReactionRequest) {
        match request {
            ReactionRequest::Test { rule, reply } => self.test(rule, reply),
            ReactionRequest::Retry(target) => {
                let wake = |output: Option<&Arc<Notify>>| {
                    if let Some(wake) = output {
                        wake.notify_one();
                    }
                };
                // Only one that isn't connected is pausing; for a connected
                // one the wake would only shorten the next outage's first
                // (one second) pause.
                match target {
                    Target::Warudo => wake(waking(&self.warudo)),
                    Target::Obs => wake(waking(&self.obs)),
                }
            }
        }
    }

    /// Carries out `rule`'s action now, and answers `reply` with how it
    /// went (in a task of its own: the reactor never waits for an output).
    fn test(&mut self, rule: String, reply: oneshot::Sender<Result<(), String>>) {
        let Some(action) = self.rules.rules().iter().find(|r| r.name == rule) else {
            let _ = reply.send(Err(format!("there's no rule \"{rule}\"")));
            return;
        };
        let action = action.action.clone();
        let label = action.target().label();
        info!(%rule, "testing a reaction");
        let (answer_tx, answer) = oneshot::channel();
        if let Err(e) = self.dispatch(rule, action, Instant::now(), Some(answer_tx)) {
            let _ = reply.send(Err(e));
            return;
        }
        tokio::spawn(async move {
            let result = match tokio::time::timeout(TEST_ANSWER, answer).await {
                Ok(Ok(Outcome::Sent)) => Ok(()),
                Ok(Ok(Outcome::Refused(why))) => Err(why),
                // Dropped on the way: the connection broke meanwhile.
                Ok(Err(_)) => Err(format!("{label} isn't connected")),
                Err(_) => Err(format!("{label} didn't answer")),
            };
            let _ = reply.send(result);
        });
    }

    /// Hands `action` to its output, as `rule`'s. A test (`reply`) only
    /// goes to a connected output: it never waits in a queue.
    fn dispatch(
        &self,
        rule: String,
        action: Action,
        now: Instant,
        reply: Option<oneshot::Sender<Outcome>>,
    ) -> Result<(), String> {
        let seconds = |secs: Option<u32>| secs.map(|s| Duration::from_secs(s.into()));
        let test = reply.is_some();
        let report = Report {
            results: Some(self.results_tx.clone()),
            reply,
        };
        match action {
            Action::Warudo { action } => send(
                &self.warudo,
                Target::Warudo,
                rule,
                action,
                now,
                report,
                test,
            ),
            Action::ObsFilter {
                source,
                filter,
                seconds: secs,
            } => {
                let effect = Effect::Filter {
                    source,
                    filter,
                    duration: seconds(secs),
                };
                send(&self.obs, Target::Obs, rule, effect, now, report, test)
            }
            Action::ObsSource {
                scene,
                source,
                seconds: secs,
            } => {
                let effect = Effect::Show {
                    scene,
                    source,
                    duration: seconds(secs),
                };
                send(&self.obs, Target::Obs, rule, effect, now, report, test)
            }
            Action::ObsMedia { source } => {
                let effect = Effect::Restart { source };
                send(&self.obs, Target::Obs, rule, effect, now, report, test)
            }
        }
    }

    /// Keeps (or clears) a rule's problem: how its last reaction went.
    fn record(&mut self, rule: String, outcome: Outcome) {
        match outcome {
            Outcome::Sent => {
                self.problems.remove(&rule);
            }
            // Only for a rule that still exists: one deleted meanwhile has
            // nothing to show it on.
            Outcome::Refused(why) => {
                if self.rules.rules().iter().any(|r| r.name == rule) {
                    self.problems.insert(rule, why);
                }
            }
        }
    }

    /// Sends the status if it changed.
    fn publish(&self) {
        let mut outputs = Vec::new();
        if let Some(output) = &self.warudo {
            outputs.push(output.status(Target::Warudo));
        }
        if let Some(output) = &self.obs {
            outputs.push(output.status(Target::Obs));
        }
        let rules = self
            .rules
            .counts()
            .into_iter()
            .map(|(name, fired)| RuleStatus {
                problem: self.problems.get(&name).cloned(),
                name,
                fired,
            });
        let new = ReactionStatus {
            outputs,
            rules: rules.collect(),
        };
        self.status.send_if_modified(|status| {
            let changed = *status != new;
            *status = new;
            changed
        });
    }

    /// Stops the outputs, all at once, and waits until they're done.
    async fn stop(self) {
        let warudo = async {
            if let Some(output) = self.warudo {
                output.stop().await;
            }
        };
        let obs = async {
            if let Some(output) = self.obs {
                output.stop().await;
            }
        };
        tokio::join!(warudo, obs);
    }
}

/// Starts, restarts or stops one output to match what the rules need.
fn keep<S, J, F, Fut>(
    slot: &mut Option<Output<S, J>>,
    needed: bool,
    wanted: S,
    target: Target,
    run: F,
) where
    S: Clone + PartialEq,
    J: Send + 'static,
    F: FnOnce(S, mpsc::Receiver<J>, watch::Sender<OutputState>, Arc<Notify>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    match &*slot {
        _ if !needed => *slot = None,
        Some(running) if running.settings == wanted => {}
        _ => {
            info!("starting the {} output", target.label());
            // Assigning drops the old output first, if any: it finishes
            // on its own.
            *slot = Some(Output::start(wanted, run));
        }
    }
}

/// Hands a rule's action to its output; `Err` says why it can't go.
fn send<S, A>(
    output: &Option<Output<S, Job<A>>>,
    target: Target,
    rule: String,
    action: A,
    now: Instant,
    report: Report,
    test: bool,
) -> Result<(), String> {
    let label = target.label();
    // No output: no enabled rule sends there (only a test gets here then).
    let Some(output) = output else {
        return Err(format!("{label} isn't connected"));
    };
    if test && *output.state.borrow() != OutputState::Connected {
        return Err(format!("{label} isn't connected"));
    }
    let mut job = Job::new(rule, action, now);
    job.report = report;
    output.jobs.try_send(job).map_err(|e| match e {
        mpsc::error::TrySendError::Full(_) => format!("{label} isn't keeping up"),
        mpsc::error::TrySendError::Closed(_) => format!("{label} isn't connected"),
    })
}

/// An output's wake, if it runs and isn't connected.
fn waking<S, J>(output: &Option<Output<S, J>>) -> Option<&Arc<Notify>> {
    output
        .as_ref()
        .filter(|output| *output.state.borrow() != OutputState::Connected)
        .map(|output| &output.wake)
}

impl<S, J: Send + 'static> Output<S, J> {
    /// Spawns `run(settings, jobs, state)`: the same shape for every
    /// output, so starting one is the same code.
    fn start<F, Fut>(settings: S, run: F) -> Self
    where
        S: Clone,
        F: FnOnce(S, mpsc::Receiver<J>, watch::Sender<OutputState>, Arc<Notify>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (jobs, jobs_rx) = mpsc::channel(OUTPUT_BUFFER);
        let (state_tx, state) = watch::channel(OutputState::Connecting);
        // Shared: the reactor wakes, the output waits. `Arc`, since the
        // output task owns its half and may outlive this `Output`.
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run(settings.clone(), jobs_rx, state_tx, wake.clone()));
        Self {
            settings,
            jobs,
            state,
            wake,
            task,
        }
    }

    fn status(&self, target: Target) -> OutputStatus {
        OutputStatus {
            target,
            state: self.state.borrow().clone(),
        }
    }

    /// Closes the jobs channel and waits for the task to finish, at most
    /// `STOP_TIMEOUT`; then it's aborted.
    async fn stop(self) {
        let Output { jobs, mut task, .. } = self;
        drop(jobs);
        if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
            warn!("an output didn't stop in time");
            task.abort();
        }
    }
}

/// Finishes when the output's connection state changes; never without an
/// output. (A function rather than inline in `select!`, which needs one
/// future per branch, also when there's nothing to wait for.)
async fn output_changed<S, J>(output: &mut Option<Output<S, J>>) {
    match output {
        Some(output) => {
            if output.state.changed().await.is_err() {
                // The task ended (it doesn't by itself): don't spin.
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::tests::{command, message, rule};
    use crate::rules::{Guards, Trigger};
    use crate::warudo::tests::{fake_warudo, listener};
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(10);

    fn settings(url: &str, rules: Vec<crate::Rule>) -> ReactionSettings {
        ReactionSettings {
            warudo: WarudoSettings {
                url: url.into(),
                queue_secs: 0,
            },
            rules,
            ..ReactionSettings::default()
        }
    }

    struct Running {
        hub: Arc<Hub>,
        settings: watch::Sender<ReactionSettings>,
        status: watch::Receiver<ReactionStatus>,
        requests: mpsc::Sender<ReactionRequest>,
        shutdown: CancellationToken,
        task: tokio_util::task::AbortOnDropHandle<()>,
    }

    fn start(hub: Arc<Hub>, initial: ReactionSettings) -> Running {
        let (settings, settings_rx) = watch::channel(initial);
        let (status_tx, status) = watch::channel(ReactionStatus::default());
        let shutdown = CancellationToken::new();
        let (requests, requests_rx) = mpsc::channel(4);
        let (_, tips) = watch::channel(TipSettings::default());
        let reactor = run(
            hub.clone(),
            settings_rx,
            tips,
            status_tx,
            None,
            requests_rx,
            shutdown.clone(),
        );
        Running {
            hub,
            settings,
            status,
            requests,
            shutdown,
            task: tokio_util::task::AbortOnDropHandle::new(tokio::spawn(reactor)),
        }
    }

    async fn until(running: &mut Running, f: impl Fn(&ReactionStatus) -> bool) {
        tokio::time::timeout(WAIT, running.status.wait_for(f))
            .await
            .expect("status not reached")
            .unwrap();
    }

    fn connected(status: &ReactionStatus) -> bool {
        status.outputs.first().map(|o| &o.state) == Some(&OutputState::Connected)
    }

    /// Each rule's (name, fired) in the status.
    fn fired(status: &ReactionStatus) -> Vec<(&str, u64)> {
        status
            .rules
            .iter()
            .map(|r| (r.name.as_str(), r.fired))
            .collect()
    }

    async fn test(running: &Running, rule: &str) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        let rule = rule.to_string();
        running
            .requests
            .send(ReactionRequest::Test { rule, reply })
            .await
            .unwrap();
        tokio::time::timeout(WAIT, answer).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_chat_command_reaches_warudo_and_chat_text_never_does() {
        let (listener, url) = listener().await;
        let mut warudo = fake_warudo(listener, false);
        let bonk = rule("bonk", command("!bonk"), Guards::default());
        let mut running = start(Arc::new(Hub::new(10)), settings(&url, vec![bonk]));
        until(&mut running, connected).await;

        // Whatever the chatter writes after the command, Warudo only ever
        // gets the action configured in the rule.
        let hostile = r#"!bonk "},{"action":"delete_everything"} <script>"#;
        running.hub.publish(ChatEvent::Message(message(hostile)));
        let got = tokio::time::timeout(WAIT, warudo.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, r#"{"action":"bonk"}"#);
        until(&mut running, |s| fired(s) == [("bonk", 1)]).await;
    }

    #[tokio::test]
    async fn history_is_never_reacted_to() {
        let (listener, url) = listener().await;
        let mut warudo = fake_warudo(listener, false);
        let hub = Arc::new(Hub::new(10));
        hub.publish(ChatEvent::Message(message("!old")));
        let rules = vec![
            rule("old", command("!old"), Guards::default()),
            rule("new", command("!new"), Guards::default()),
        ];
        // With a queue: a replayed `!old` fired before the connection is up
        // would wait for it and arrive, instead of being dropped.
        let mut with_queue = settings(&url, rules);
        with_queue.warudo.queue_secs = 30;
        let mut running = start(hub, with_queue);
        until(&mut running, connected).await;
        running.hub.publish(ChatEvent::Message(message("!new")));
        let got = tokio::time::timeout(WAIT, warudo.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, r#"{"action":"new"}"#, "not the replayed !old");
    }

    #[tokio::test]
    async fn warudo_is_only_connected_while_a_rule_needs_it() {
        let (listener, url) = listener().await;
        let _warudo = fake_warudo(listener, false);
        let mut running = start(Arc::new(Hub::new(10)), settings(&url, vec![]));
        assert!(running.status.borrow().outputs.is_empty());

        let on = rule("a", Trigger::Membership, Guards::default());
        running
            .settings
            .send_replace(settings(&url, vec![on.clone()]));
        until(&mut running, connected).await;

        let off = crate::Rule {
            enabled: false,
            ..on
        };
        running.settings.send_replace(settings(&url, vec![off]));
        until(&mut running, |s| s.outputs.is_empty()).await;
    }

    #[tokio::test]
    async fn a_new_address_means_a_new_connection() {
        let (first_listener, first) = listener().await;
        let (second_listener, second) = listener().await;
        let _to_first = fake_warudo(first_listener, false);
        let mut to_second = fake_warudo(second_listener, false);
        let rules = vec![rule("bonk", command("!bonk"), Guards::default())];
        let mut running = start(Arc::new(Hub::new(10)), settings(&first, rules.clone()));
        until(&mut running, connected).await;

        running.settings.send_replace(settings(&second, rules));
        // Until the reactor has the new settings and the new connection is
        // up, a `!bonk` goes to the old address or is dropped: keep trying.
        // (Waiting for the status instead could miss the switch: the old
        // and the new connection both read "connected".)
        let mut got = None;
        for _ in 0..100 {
            running.hub.publish(ChatEvent::Message(message("!bonk")));
            let wait = Duration::from_millis(100);
            if let Ok(Some(text)) = tokio::time::timeout(wait, to_second.recv()).await {
                got = Some(text);
                break;
            }
        }
        assert_eq!(got.as_deref(), Some(r#"{"action":"bonk"}"#));
    }

    #[tokio::test]
    async fn obs_reactions_and_shutdown_switching_them_back() {
        use crate::obs::tests::{Scene, fake_obs};
        let (warudo_listener, warudo_url) = listener().await;
        let _warudo = fake_warudo(warudo_listener, false);
        let (obs_listener, obs_url) = crate::obs::tests::listener().await;
        let scene = Scene::default().filter("Mic", "Chipmunk", false);
        let mut obs = fake_obs(obs_listener, None, scene);

        let chipmunk = crate::Rule {
            action: Action::ObsFilter {
                source: "Mic".into(),
                filter: "Chipmunk".into(),
                seconds: Some(600),
            },
            ..rule(
                "chipmunk",
                Trigger::Gift { min_count: 5 },
                Guards::default(),
            )
        };
        let bonk = rule("bonk", command("!bonk"), Guards::default());
        let mut both = settings(&warudo_url, vec![bonk, chipmunk]);
        both.obs.url = obs_url;
        let mut running = start(Arc::new(Hub::new(10)), both);
        let all_connected = |s: &ReactionStatus| {
            s.outputs.len() == 2 && s.outputs.iter().all(|o| o.state == OutputState::Connected)
        };
        until(&mut running, all_connected).await;
        let targets: Vec<Target> = running
            .status
            .borrow()
            .outputs
            .iter()
            .map(|o| o.target)
            .collect();
        assert_eq!(targets, [Target::Warudo, Target::Obs]);

        let mut gift = message("");
        gift.kind = chat_core::MessageKind::MembershipGift { count: 5 };
        running.hub.publish(ChatEvent::Message(gift));
        let change = tokio::time::timeout(WAIT, obs.changes.recv())
            .await
            .unwrap();
        assert_eq!(change.as_deref(), Some("filter Mic/Chipmunk on"));

        // The app quits ten minutes early: the filter goes back off, and the
        // reactor waits for that before it ends.
        running.shutdown.cancel();
        tokio::time::timeout(WAIT, running.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            obs.changes.try_recv().as_deref(),
            Ok("filter Mic/Chipmunk off"),
            "switched back before the reactor ended"
        );
    }

    #[tokio::test]
    async fn a_test_fires_a_switched_off_rule_while_its_program_is_connected() {
        let (listener, url) = listener().await;
        let mut warudo = fake_warudo(listener, false);
        let on = rule("bonk", command("!bonk"), Guards::default());
        let off = crate::Rule {
            enabled: false,
            ..rule("wave", command("!wave"), Guards::default())
        };
        let mut paused = settings(&url, vec![on, off]);
        paused.paused = true;
        let mut running = start(Arc::new(Hub::new(10)), paused);
        until(&mut running, connected).await;

        // Switched off, and everything paused: a test fires anyway...
        assert_eq!(test(&running, "wave").await, Ok(()));
        let got = tokio::time::timeout(WAIT, warudo.recv()).await.unwrap();
        assert_eq!(got.as_deref(), Some(r#"{"action":"wave"}"#));
        // ...and isn't counted: the count is what chat did.
        assert_eq!(fired(&running.status.borrow()), [("bonk", 0), ("wave", 0)]);

        let unknown = test(&running, "nope").await.unwrap_err();
        assert!(unknown.contains("nope"), "{unknown}");
    }

    #[tokio::test]
    async fn a_test_does_not_start_a_program_no_rule_needs() {
        let (listener, url) = listener().await;
        let mut warudo = fake_warudo(listener, false);
        let off = crate::Rule {
            enabled: false,
            ..rule("wave", command("!wave"), Guards::default())
        };
        let running = start(Arc::new(Hub::new(10)), settings(&url, vec![off]));
        assert_eq!(
            test(&running, "wave").await,
            Err("Warudo isn't connected".into())
        );
        assert!(running.status.borrow().outputs.is_empty());
        let quiet = tokio::time::timeout(Duration::from_millis(200), warudo.recv()).await;
        assert!(quiet.is_err(), "nothing reached Warudo");
    }

    #[tokio::test]
    async fn obs_refusing_a_rule_shows_on_it_until_the_rule_changes() {
        use crate::obs::tests::{Scene, fake_obs};
        let (obs_listener, obs_url) = crate::obs::tests::listener().await;
        let scene = Scene::default().filter("Mic", "Chipmunk", false);
        let mut obs = fake_obs(obs_listener, None, scene);
        let filter = |name: &str| crate::Rule {
            action: Action::ObsFilter {
                source: "Mic".into(),
                filter: name.into(),
                seconds: None,
            },
            ..rule("chipmunk", command("!chipmunk"), Guards::default())
        };
        let mut typo = settings("ws://127.0.0.1:1", vec![filter("Typo")]);
        typo.obs.url = obs_url;
        let mut running = start(Arc::new(Hub::new(10)), typo.clone());
        until(&mut running, connected).await;

        // Chat fires it, OBS refuses: the rule says why.
        running
            .hub
            .publish(ChatEvent::Message(message("!chipmunk")));
        let problem = |s: &ReactionStatus| s.rules[0].problem.clone();
        until(&mut running, |s| problem(s).is_some()).await;
        let why = problem(&running.status.borrow()).unwrap();
        assert!(why.contains(r#"filter "Typo" on "Mic""#), "{why}");
        // A test says the same.
        let tested = test(&running, "chipmunk").await.unwrap_err();
        assert!(tested.contains("Typo"), "{tested}");

        // Fixed: the problem goes with the old action.
        let mut fixed = typo;
        fixed.rules = vec![filter("Chipmunk")];
        running.settings.send_replace(fixed);
        until(&mut running, |s| problem(s).is_none()).await;
        assert_eq!(test(&running, "chipmunk").await, Ok(()));
        let change = tokio::time::timeout(WAIT, obs.changes.recv())
            .await
            .unwrap();
        assert_eq!(change.as_deref(), Some("filter Mic/Chipmunk on"));
    }
}
