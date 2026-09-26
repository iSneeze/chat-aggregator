//! The engine's control plane as an "actor": one task owns all source state,
//! everyone else sends it messages (see `EngineHandle`). Since only this task
//! ever changes the state, it needs no locks, and commands can't interfere
//! with each other: they're handled one after another.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use chat_core::{Activity, ChatEvent, ChatSource, Hub, Reporter};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::config::SourceConfig;
use crate::factory::{SourceFactory, is_login_required, needs_attention};
use crate::status::{SourceId, SourceState, SourceStatus, Status};

/// First retry after 2 s, doubling up to this.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
/// A source that ran at least this long before failing counts as having
/// worked: its retry count starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

pub(crate) type Reply<T> = oneshot::Sender<anyhow::Result<T>>;

/// What `EngineHandle` asks for. Each carries a `oneshot` sender for the
/// answer: a channel that is used exactly once.
pub(crate) enum Command {
    Add(SourceConfig, Reply<SourceId>),
    Remove(SourceId, Reply<()>),
    Start(SourceId, Reply<()>),
    Stop(SourceId, Reply<()>),
    Update(SourceId, SourceConfig, Reply<()>),
}

/// Messages from the actor's own helper tasks back to it.
enum Internal {
    Ended {
        id: SourceId,
        generation: u64,
        result: anyhow::Result<()>,
        ran_for: Duration,
    },
    RetryDue {
        id: SourceId,
        generation: u64,
    },
    /// A source reported a new activity: publish the status now.
    ActivityChanged,
}

pub(crate) struct Actor<F> {
    factory: Arc<F>,
    hub: Arc<Hub>,
    // A BTreeMap keeps sources ordered by id, i.e. in the order they were
    // added, so status lists don't jump around.
    slots: BTreeMap<SourceId, Slot>,
    next_id: u64,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    status: watch::Sender<Status>,
}

struct Slot {
    config: SourceConfig,
    state: SourceState,
    /// Bumped on every start and stop. Messages from an older run (a late
    /// "ended", a retry timer) carry the old number and are ignored.
    generation: u64,
    /// Dropping the handle aborts the task: stopping a source is just
    /// setting this to `None`.
    task: Option<AbortOnDropHandle<()>>,
    activity: Option<watch::Receiver<Activity>>,
    stats: Arc<Stats>,
    attempt: u32,
}

/// Counted by the forwarding task, read by the actor. Atomics instead of a
/// Mutex: two independent numbers, each updated in a single step.
#[derive(Default)]
struct Stats {
    messages: AtomicU64,
    /// Unix milliseconds; 0 = none yet.
    last_message_ms: AtomicI64,
}

impl Stats {
    fn record(&self, event: &ChatEvent) {
        if matches!(event, ChatEvent::Message(_)) {
            self.messages.fetch_add(1, Ordering::Relaxed);
            self.last_message_ms
                .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
        }
    }

    fn last_message(&self) -> Option<DateTime<Utc>> {
        match self.last_message_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => DateTime::from_timestamp_millis(ms),
        }
    }
}

impl<F: SourceFactory> Actor<F> {
    pub(crate) fn new(factory: F, hub: Arc<Hub>, status: watch::Sender<Status>) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            factory: Arc::new(factory),
            hub,
            slots: BTreeMap::new(),
            next_id: 1,
            internal_tx,
            internal_rx,
            status,
        }
    }

    pub(crate) async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        shutdown: CancellationToken,
    ) {
        // Counters and activities change without the actor noticing, so the
        // status is also refreshed once a second.
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break, // every EngineHandle is gone
                },
                // `internal_tx` lives in `self`, so this channel never closes
                // while the actor runs: `Some` is guaranteed.
                Some(message) = self.internal_rx.recv() => self.internal(message),
                _ = tick.tick() => {}
                () = shutdown.cancelled() => break,
            }
            self.publish();
        }
        // Returning drops `self` and with it every slot's task handle, which
        // aborts all running sources.
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Add(config, reply) => {
                let id = SourceId(self.next_id);
                self.next_id += 1;
                info!(source = %config.label(), %id, "added");
                self.slots.insert(
                    id,
                    Slot {
                        config,
                        state: SourceState::Stopped,
                        generation: 0,
                        task: None,
                        activity: None,
                        stats: Arc::default(),
                        attempt: 0,
                    },
                );
                self.start(id);
                let _ = reply.send(Ok(id));
            }
            Command::Remove(id, reply) => {
                let result = match self.slots.remove(&id) {
                    Some(slot) => {
                        info!(source = %slot.config.label(), "removed");
                        Ok(()) // dropping the slot aborts its task
                    }
                    None => Err(unknown(id)),
                };
                let _ = reply.send(result);
            }
            Command::Start(id, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    // Already running: nothing to do.
                    Some(slot) if slot.task.is_some() => Ok(()),
                    Some(slot) => {
                        slot.attempt = 0;
                        self.start(id);
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
            Command::Stop(id, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    Some(slot) => {
                        stop(slot);
                        slot.state = SourceState::Stopped;
                        info!(source = %slot.config.label(), "stopped");
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
            Command::Update(id, config, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    Some(slot) => {
                        let was_stopped = slot.state == SourceState::Stopped;
                        stop(slot);
                        slot.config = config;
                        slot.attempt = 0;
                        // New settings may fix whatever was wrong, so restart
                        // it; unless you had turned it off.
                        if !was_stopped {
                            self.start(id);
                        }
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
        }
    }

    /// Builds and runs a slot's source in a new task.
    fn start(&mut self, id: SourceId) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        slot.generation += 1;
        let generation = slot.generation;
        let (reporter, activity) = Reporter::new();
        let mut activity_changes = activity.clone();
        slot.activity = Some(activity);
        slot.state = SourceState::Running;

        let factory = self.factory.clone();
        let hub = self.hub.clone();
        let stats = slot.stats.clone();
        let config = slot.config.clone();
        let internal = self.internal_tx.clone();
        info!(source = %config.label(), "starting");
        slot.task = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            let started = Instant::now();
            // Wakes the actor whenever the source reports a new activity,
            // so the status light changes right away rather than at the next
            // 1 s tick. Once the source is done (its reporter dropped), it
            // just waits forever: only `run_source` ends the `select!`.
            let relay = async {
                while activity_changes.changed().await.is_ok() {
                    let _ = internal.send(Internal::ActivityChanged);
                }
                std::future::pending::<()>().await
            };
            let result = tokio::select! {
                result = run_source(&*factory, &config, &hub, &stats, reporter) => result,
                () = relay => unreachable!("the relay never finishes"),
            };
            let _ = internal.send(Internal::Ended {
                id,
                generation,
                result,
                ran_for: started.elapsed(),
            });
        })));
    }

    fn internal(&mut self, message: Internal) {
        match message {
            Internal::Ended {
                id,
                generation,
                result,
                ran_for,
            } => {
                let Some(slot) = self.slots.get_mut(&id) else {
                    return; // removed meanwhile
                };
                if slot.generation != generation {
                    return; // an older run; the slot has moved on
                }
                slot.task = None;
                slot.activity = None;
                let label = slot.config.label();
                match result {
                    Ok(()) => {
                        info!(source = %label, "finished");
                        slot.state = SourceState::Finished;
                    }
                    Err(e) if needs_attention(&e) => {
                        warn!(source = %label, "needs attention: {e:#}");
                        if is_login_required(&e) {
                            self.factory.forget_login();
                        }
                        slot.state = SourceState::NeedsAttention {
                            error: format!("{e:#}"),
                        };
                    }
                    Err(e) => {
                        slot.attempt = if ran_for >= STABLE_AFTER {
                            1
                        } else {
                            slot.attempt + 1
                        };
                        let delay = backoff(slot.attempt);
                        warn!(source = %label, attempt = slot.attempt, ?delay, "failed, retrying: {e:#}");
                        slot.state = SourceState::Retrying {
                            attempt: slot.attempt,
                            next_try: Utc::now()
                                + chrono::Duration::from_std(delay).unwrap_or_default(),
                            error: format!("{e:#}"),
                        };
                        let internal = self.internal_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = internal.send(Internal::RetryDue { id, generation });
                        });
                    }
                }
            }
            // Nothing to do: `run` publishes after every message.
            Internal::ActivityChanged => {}
            Internal::RetryDue { id, generation } => {
                let due = self.slots.get(&id).is_some_and(|slot| {
                    slot.generation == generation
                        && matches!(slot.state, SourceState::Retrying { .. })
                });
                if due {
                    self.start(id);
                }
            }
        }
    }

    /// Sends the current status to watchers, but only if it changed: a
    /// watcher (the UI) is woken up only when there's something new to show.
    fn publish(&self) {
        let sources = self
            .slots
            .iter()
            .map(|(id, slot)| SourceStatus {
                id: *id,
                config: slot.config.clone(),
                state: slot.state.clone(),
                activity: slot.activity.as_ref().map(|a| a.borrow().clone()),
                messages: slot.stats.messages.load(Ordering::Relaxed),
                last_message: slot.stats.last_message(),
            })
            .collect::<Vec<_>>();
        self.status.send_if_modified(|status| {
            if status.sources == sources {
                false
            } else {
                status.sources = sources;
                true
            }
        });
    }
}

/// Builds the source, runs it in its own task and forwards its events into
/// the hub, counting them on the way.
async fn run_source<F: SourceFactory>(
    factory: &F,
    config: &SourceConfig,
    hub: &Hub,
    stats: &Stats,
    activity: Reporter,
) -> anyhow::Result<()> {
    let source = factory.build(config).await?;
    let (tx, mut rx) = mpsc::channel(256);
    // Its own task, so a panic inside the source becomes an error here
    // instead of silently taking this task down with it. Abort-on-drop, so
    // aborting this task (stop, shutdown) also stops the source.
    let run = AbortOnDropHandle::new(tokio::spawn(source.run(tx, activity)));
    // Ends when the source drops its sender, i.e. when it has finished.
    while let Some(event) = rx.recv().await {
        stats.record(&event);
        hub.publish(event);
    }
    match run.await {
        Ok(result) => result,
        Err(e) => Err(anyhow!("the source crashed: {e}")),
    }
}

fn stop(slot: &mut Slot) {
    slot.generation += 1; // invalidates pending retry timers and late "ended"s
    slot.task = None; // aborts the task
    slot.activity = None;
}

fn backoff(attempt: u32) -> Duration {
    // 2 s, 4 s, 8 s, ... `saturating_pow`/`min` keep this from overflowing
    // after many attempts.
    FIRST_BACKOFF
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(MAX_BACKOFF)
}

fn unknown(id: SourceId) -> anyhow::Error {
    anyhow!("no source {id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        let secs: Vec<_> = (1..=10).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }
}
