//! The engine's control plane as an "actor": one task owns all source state
//! and the settings, everyone else sends it messages (see `EngineHandle`).
//! Since only this task ever changes the state, it needs no locks, and
//! commands can't interfere with each other: they're handled one after
//! another. (Two windows changing settings at the same moment both get their
//! change in: each runs on the settings the other one left.)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use chat_core::{Activity, ChatEvent, ChatSource, Hub, Reporter, SetupError};
use chat_reactions::ReactionStatus;
use chat_server::Connections;
use chat_youtube::oauth::OAuthApp;
use jiff::Timestamp;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::config::{SourceConfig, YouTubeSettings};
use crate::config_file::{ConfigFile, Settings, SourceEntry};
use crate::factory::{SourceFactory, is_login_required, needs_attention};
use crate::settings::Controls;
use crate::status::{SourceId, SourceState, SourceStatus, Status, label};

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
    /// The `bool`: start it right away.
    Add(SourceConfig, bool, Reply<SourceId>),
    /// Answers with the removed source's settings (to forget its secrets).
    Remove(SourceId, Reply<Option<SourceConfig>>),
    Start(SourceId, Reply<()>),
    Stop(SourceId, Reply<()>),
    Update(SourceId, SourceConfig, Reply<()>),
    AddManual(Reply<(SourceId, ManualInput)>),
    /// Runs the change on the current settings and applies what changed.
    EditSettings(SettingsChange, Reply<()>),
    /// The streamer's answer about a site waiting to use the JSON API.
    AnswerApiRequest {
        origin: String,
        allow: bool,
        reply: Reply<()>,
    },
    /// The stored YouTube logins changed (one made, moved or removed): the
    /// change brings the connected channels in the settings up to date.
    /// Answers whether they're saved.
    YouTubeLoginsChanged(SettingsChange, Reply<bool>),
    /// The OAuth client in the current settings (`SetupError`: none yet).
    YouTubeClient(Reply<OAuthApp>),
    /// Use this custom emoji export, and load it even if it's the file
    /// already set (a new export imported over the old one).
    UseYouTubeEmojis(PathBuf, Reply<()>),
}

/// A settings change: a closure that edits them. Every closure has a type
/// of its own, made up by the compiler and without a name, so an enum
/// variant can't hold "any closure" directly. Boxed as a trait object
/// (`dyn FnOnce`) it can. Here `dyn` is the right tool: unlike the sources
/// (`SourceSpec`), the set of types isn't known in advance.
pub(crate) type SettingsChange = Box<dyn FnOnce(&mut Settings) + Send>;

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
    /// The current settings: changed only here, watched by everyone else.
    settings: watch::Sender<Settings>,
    /// What the settings steer (overlays, the JSON API, the hub).
    controls: Arc<Controls>,
    /// Where the settings and sources are saved; `None`: nowhere.
    save_to: Option<PathBuf>,
    /// Why the last save failed, for the status; `None` once one works.
    save_error: Option<String>,
    // A BTreeMap keeps sources ordered by id, i.e. in the order they were
    // added, so status lists don't jump around.
    slots: BTreeMap<SourceId, Slot>,
    next_id: u64,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    status: watch::Sender<Status>,
    connections: Arc<Connections>,
    /// Sites waiting for an answer (`ApiOrigins::requests`): shown in the
    /// status, and a change is published right away.
    api_requests: watch::Receiver<Vec<String>>,
    /// The reactor's report (outputs, counts): a change is published right
    /// away, like the sites waiting above.
    reactions: watch::Receiver<ReactionStatus>,
}

struct Slot {
    source: SlotSource,
    run: Run,
    /// Bumped on every start and stop. Messages from an older run (a late
    /// "ended", a retry timer) carry the old number and are ignored.
    generation: u64,
    stats: Arc<Stats>,
    /// How many times in a row it has failed; kept while it runs, since
    /// the next failure counts on from here (unless it ran long enough,
    /// see `STABLE_AFTER`).
    attempt: u32,
}

impl Slot {
    /// Added, not running yet.
    fn new(source: SlotSource) -> Self {
        Self {
            source,
            run: Run::Stopped,
            generation: 0,
            stats: Arc::default(),
            attempt: 0,
        }
    }

    /// What the status shows: the same states as [`Run`], with a running
    /// source's activity as it is right now.
    fn state(&self) -> SourceState {
        match &self.run {
            Run::Stopped => SourceState::Stopped,
            Run::Running { activity, .. } => SourceState::Running(activity.borrow().clone()),
            Run::Retrying { next_try, error } => SourceState::Retrying {
                attempt: self.attempt,
                next_try: *next_try,
                error: error.clone(),
            },
            Run::NeedsAttention { error } => SourceState::NeedsAttention {
                error: error.clone(),
            },
            Run::Finished => SourceState::Finished,
        }
    }

    /// Switched off by you (`enabled = false` in the file), as opposed to
    /// failed or ended: those are still meant to run.
    fn is_stopped(&self) -> bool {
        matches!(self.run, Run::Stopped)
    }

    /// Ends the current run, if there is one. Callers that only restart it
    /// call `Actor::start` right after.
    fn stop(&mut self) {
        // Invalidates pending retry timers and late "ended"s.
        self.generation += 1;
        // Assigning drops the old value: for a running source that's its
        // task handle, which aborts the task.
        self.run = Run::Stopped;
    }
}

/// Where a slot is in its life: the actor's own version of `SourceState`,
/// which is what it publishes (see [`Slot::state`]).
///
/// What only a running source has, its task and the activity it reports,
/// lives inside `Running`. So "stopped, but with a task still going" can't
/// be written down, and stopping is one assignment instead of three fields
/// to keep in step.
enum Run {
    /// Turned off by you.
    Stopped,
    Running {
        /// What the source says it's doing (its `Reporter`'s other end).
        activity: watch::Receiver<Activity>,
        /// Dropping the handle aborts the task. Never read, only held:
        /// the leading `_` tells the compiler that's on purpose.
        _task: AbortOnDropHandle<()>,
    },
    /// Failed; a timer starts it again at `next_try` (`Internal::RetryDue`).
    Retrying { next_try: Timestamp, error: String },
    /// Failed in a way only you can fix.
    NeedsAttention { error: String },
    /// Ended normally.
    Finished,
}

/// Where a slot's chat comes from. An enum rather than a `SourceConfig`
/// plus an optional receiver: each kind has exactly what it needs, so the
/// test window's source, which has no settings, can't get into the config
/// file by mistake (`SourceConfig` has no variant for it).
#[derive(Clone)]
enum SlotSource {
    /// A source built from its settings, anew on every (re)start.
    Configured(SourceConfig),
    /// The app's test window: its messages arrive here, sent through a
    /// `ManualInput`. Shared behind a Mutex so a stop/start can re-attach:
    /// the running task holds the lock, aborting it releases the lock for
    /// the next run.
    Manual(Arc<tokio::sync::Mutex<mpsc::Receiver<ChatEvent>>>),
}

impl SlotSource {
    /// Its settings; `None` for the test window's source.
    fn config(&self) -> Option<&SourceConfig> {
        match self {
            SlotSource::Configured(config) => Some(config),
            SlotSource::Manual(_) => None,
        }
    }

    /// For logs: without the settings, a YouTube account is its id.
    fn label(&self) -> String {
        label(self.config(), &YouTubeSettings::default())
    }
}

/// Sends messages into a manual source (the app's test window). The source
/// ends when this is dropped, e.g. when the window closes.
#[derive(Clone)]
pub struct ManualInput(mpsc::Sender<ChatEvent>);

impl ManualInput {
    pub async fn send(&self, event: ChatEvent) -> anyhow::Result<()> {
        self.0
            .send(event)
            .await
            .map_err(|_| anyhow!("the test source has been removed"))
    }
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
                .store(Timestamp::now().as_millisecond(), Ordering::Relaxed);
        }
    }

    fn last_message(&self) -> Option<Timestamp> {
        match self.last_message_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Timestamp::from_millisecond(ms).ok(),
        }
    }
}

impl<F: SourceFactory> Actor<F> {
    pub(crate) fn new(
        factory: F,
        settings: watch::Sender<Settings>,
        controls: Arc<Controls>,
        status: watch::Sender<Status>,
        connections: Arc<Connections>,
        reactions: watch::Receiver<ReactionStatus>,
        save_to: Option<PathBuf>,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let api_requests = controls.origins.requests();
        Self {
            factory: Arc::new(factory),
            settings,
            controls,
            save_to,
            save_error: None,
            slots: BTreeMap::new(),
            next_id: 1,
            internal_tx,
            internal_rx,
            status,
            connections,
            api_requests,
            reactions,
        }
    }

    /// Adds the sources from the config file, before the actor runs, so
    /// they're in the very first status. Not saved: nothing changed, and an
    /// untouched file keeps its comments.
    pub(crate) fn add_from_config(&mut self, sources: Vec<SourceEntry>) {
        for entry in sources {
            self.add(entry.config, entry.enabled);
        }
        self.publish();
    }

    /// Handles commands until the engine shuts down.
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
                // A question for the streamer: show it now, not at the next
                // tick. (`Err` would mean the server is gone: shutting down.)
                Ok(()) = self.api_requests.changed() => {}
                // An output connected or lost its program, a rule fired.
                Ok(()) = self.reactions.changed() => {}
                () = shutdown.cancelled() => break,
            }
            self.publish();
        }
        // Commands sent before the shutdown still count, e.g. the settings
        // window's last change, sent while the app quits: it's applied and
        // saved. (A source one of them starts is stopped again right below.)
        while let Ok(command) = commands.try_recv() {
            self.handle(command);
        }
        // Returning drops `self` and with it every slot's task handle, which
        // aborts all running sources.
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Add(config, start, reply) => {
                let id = self.add(config, start);
                self.save();
                let _ = reply.send(Ok(id));
            }
            Command::AddManual(reply) => {
                let id = SourceId(self.next_id);
                self.next_id += 1;
                let (tx, rx) = mpsc::channel(64);
                info!(%id, "test window source added");
                let input = Arc::new(tokio::sync::Mutex::new(rx));
                self.slots.insert(id, Slot::new(SlotSource::Manual(input)));
                self.start(id);
                let _ = reply.send(Ok((id, ManualInput(tx))));
            }
            Command::Remove(id, reply) => {
                let result = match self.slots.remove(&id) {
                    Some(slot) => {
                        info!(source = %slot.source.label(), "removed");
                        let config = slot.source.config().cloned();
                        if config.is_some() {
                            self.save();
                        }
                        Ok(config) // dropping the slot aborts its task
                    }
                    None => Err(unknown(id)),
                };
                let _ = reply.send(result);
            }
            Command::Start(id, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    // Already running: nothing to do.
                    Some(slot) if matches!(slot.run, Run::Running { .. }) => Ok(()),
                    Some(slot) => {
                        // Switched on, rather than started again after a
                        // failure: that goes into the file.
                        let switched_on = slot.is_stopped() && slot.source.config().is_some();
                        slot.attempt = 0;
                        self.start(id);
                        if switched_on {
                            self.save();
                        }
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
            Command::Stop(id, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    Some(slot) => {
                        let switched_off = !slot.is_stopped() && slot.source.config().is_some();
                        slot.stop();
                        info!(source = %slot.source.label(), "stopped");
                        if switched_off {
                            self.save();
                        }
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
            Command::EditSettings(change, reply) => {
                let _ = reply.send(self.edit_settings(change).map(drop));
            }
            Command::AnswerApiRequest {
                origin,
                allow,
                reply,
            } => {
                self.controls.origins.answer(&origin, allow);
                // "Allow" is for good: it goes into the settings. ("Deny"
                // only lasts until the engine restarts.)
                if allow {
                    // Can't be refused: only reaction settings are checked.
                    let _ = self.edit_settings(Box::new(move |settings| {
                        let allowed = &mut settings.server.api_origins;
                        if !allowed.contains(&origin) {
                            allowed.push(origin);
                        }
                    }));
                }
                let _ = reply.send(Ok(()));
            }
            Command::YouTubeLoginsChanged(change, reply) => {
                info!("YouTube logins changed");
                let result = self.edit_settings(change).map(|restarted| {
                    // A channel logged in again changes no setting, but
                    // it's a new login all the same.
                    if !restarted {
                        self.restart_youtube();
                    }
                    self.save_to.is_some() && self.save_error.is_none()
                });
                let _ = reply.send(result);
            }
            Command::YouTubeClient(reply) => {
                let app = self.settings.borrow().youtube.oauth_app();
                let _ = reply.send(app.map_err(Into::into));
            }
            Command::UseYouTubeEmojis(path, reply) => {
                // A different file is a settings change: that restarts the
                // YouTube sources by itself. The same file is news too, but
                // only in its contents, which the settings can't see.
                let same_file = self.settings.borrow().youtube.emojis.as_ref() == Some(&path);
                // Can't be refused: only reaction settings are checked.
                let _ = self.edit_settings(Box::new(move |settings| {
                    settings.youtube.emojis = Some(path);
                }));
                if same_file {
                    info!("YouTube emoji file replaced");
                    self.restart_youtube();
                }
                let _ = reply.send(Ok(()));
            }
            Command::Update(id, config, reply) => {
                let result = match self.slots.get_mut(&id) {
                    None => Err(unknown(id)),
                    Some(Slot {
                        source: SlotSource::Manual(_),
                        ..
                    }) => Err(anyhow!("the test messages source has no settings")),
                    // A tip service's token is stored under the account or
                    // name (`SourceConfig::token`). Settings with another
                    // one, or another kind of source, would leave the old
                    // token behind unused and find no token for the new.
                    Some(Slot {
                        source: SlotSource::Configured(old),
                        ..
                    }) if old.token().map(|token| token.secret_name)
                        != config.token().map(|token| token.secret_name) =>
                    {
                        Err(SetupError(
                            "a tip source's token is stored under its account or name, so those \
                             can't change: remove the source and add it again with its token"
                                .into(),
                        )
                        .into())
                    }
                    Some(slot) => {
                        let was_stopped = slot.is_stopped();
                        slot.stop();
                        slot.source = SlotSource::Configured(config);
                        slot.attempt = 0;
                        // New settings may fix whatever was wrong, so restart
                        // it; unless you had turned it off.
                        if !was_stopped {
                            self.start(id);
                        }
                        self.save();
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
        }
    }

    /// Adds a source from its settings: started right away, or listed as
    /// switched off.
    fn add(&mut self, config: SourceConfig, start: bool) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id += 1;
        info!(source = %config.label(), %id, "added");
        self.slots
            .insert(id, Slot::new(SlotSource::Configured(config)));
        if start {
            self.start(id);
        }
        id
    }

    /// Writes `config.toml`, if the engine has one: the settings, and the
    /// sources with their on/off switch (the test window's source has no
    /// settings, so it isn't in there). Called after every change to
    /// either, but never at startup: an untouched file keeps its comments.
    /// A problem shows in the status until a later save works.
    fn save(&mut self) {
        let Some(path) = &self.save_to else {
            return;
        };
        let Settings {
            server,
            youtube,
            app,
            tips,
            reactions,
            widgets,
        } = self.settings.borrow().clone();
        let sources = self
            .slots
            .values()
            .filter_map(|slot| {
                slot.source.config().map(|config| SourceEntry {
                    config: config.clone(),
                    // On unless you switched it off: a source that failed
                    // or ended is still meant to run next time.
                    enabled: !slot.is_stopped(),
                })
            })
            .collect();
        let file = ConfigFile {
            server,
            youtube,
            app,
            tips,
            reactions,
            widgets,
            sources,
        };
        self.save_error = match file.save(path) {
            Ok(()) => None,
            Err(e) => {
                warn!("couldn't save the settings: {e:#}");
                Some(format!("{e:#}"))
            }
        };
    }

    /// Runs `change` on the current settings, hands what changed to the
    /// running parts and publishes the new settings.
    /// Applies `change`; refused (and nothing changes) if it makes the
    /// chat reactions invalid, as `config.toml` would be refused at
    /// startup: the app's forms check most of it themselves, but the engine
    /// is what keeps the file loadable.
    /// Answers whether that restarted the YouTube sources (new YouTube
    /// settings do).
    fn edit_settings(&mut self, change: SettingsChange) -> anyhow::Result<bool> {
        let old = self.settings.borrow().clone();
        let mut new = old.clone();
        change(&mut new);
        if new == old {
            return Ok(false);
        }
        if new.tips != old.tips {
            new.tips.check().map_err(|e| anyhow!(e))?;
        }
        if new.reactions != old.reactions {
            new.reactions.check().map_err(|e| anyhow!(e))?;
        }
        let youtube_changed = self.controls.apply(&old, &new);
        if youtube_changed {
            self.factory.set_youtube(new.youtube.clone());
        }
        // Published (and saved) before the caller gets its answer: whoever
        // awaited the change finds it in `settings()` and in the file.
        self.settings.send_replace(new);
        if youtube_changed {
            info!("YouTube settings changed");
            self.restart_youtube();
        }
        self.save();
        Ok(youtube_changed)
    }

    /// Restarts every YouTube source that is meant to run, with a fresh
    /// login: after new settings (a different client, API key or emoji) or
    /// a login made or removed, the cached one is no longer the right one.
    /// So a red "login required" turns green without restarting the app.
    fn restart_youtube(&mut self) {
        self.factory.forget_login();
        let youtube: Vec<SourceId> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                matches!(
                    slot.source,
                    SlotSource::Configured(SourceConfig::YouTube { .. })
                ) && !slot.is_stopped()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in youtube {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.stop();
                slot.attempt = 0;
            }
            self.start(id);
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

        let factory = self.factory.clone();
        let hub = self.controls.hub.clone();
        let stats = slot.stats.clone();
        let source = slot.source.clone();
        let internal = self.internal_tx.clone();
        info!(source = %source.label(), "starting");
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
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
            let run = async {
                match &source {
                    SlotSource::Configured(config) => {
                        run_source(&*factory, config, &hub, &stats, reporter).await
                    }
                    SlotSource::Manual(input) => run_manual(input, &hub, &stats, reporter).await,
                }
            };
            let result = tokio::select! {
                result = run => result,
                () = relay => unreachable!("the relay never finishes"),
            };
            let _ = internal.send(Internal::Ended {
                id,
                generation,
                result,
                ran_for: started.elapsed(),
            });
        }));
        slot.run = Run::Running {
            activity,
            _task: task,
        };
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
                // The run is over either way: each arm below replaces
                // `Running` (and with it the finished task's handle).
                let label = slot.source.label();
                match result {
                    Ok(()) => {
                        info!(source = %label, "finished");
                        slot.run = Run::Finished;
                    }
                    Err(e) if needs_attention(&e) => {
                        warn!(source = %label, "needs attention: {e:#}");
                        if is_login_required(&e) {
                            self.factory.forget_login();
                        }
                        slot.run = Run::NeedsAttention {
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
                        slot.run = Run::Retrying {
                            // jiff adds a `std::time::Duration` directly;
                            // the backoff is capped, far from overflowing.
                            next_try: Timestamp::now() + delay,
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
                    slot.generation == generation && matches!(slot.run, Run::Retrying { .. })
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
        let youtube = self.settings.borrow().youtube.clone();
        let sources = self
            .slots
            .iter()
            .map(|(id, slot)| SourceStatus {
                id: *id,
                label: label(slot.source.config(), &youtube),
                config: slot.source.config().cloned(),
                state: slot.state(),
                messages: slot.stats.messages.load(Ordering::Relaxed),
                last_message: slot.stats.last_message(),
            })
            .collect::<Vec<_>>();
        let overlays = self.connections.overlays();
        let api_clients = self.connections.api_clients();
        let widgets = self.connections.widgets();
        let api_requests = self.api_requests.borrow().clone();
        let save_error = self.save_error.clone();
        let reactions = self.reactions.borrow().clone();
        self.status.send_if_modified(|status| {
            let unchanged = status.sources == sources
                && status.overlays_connected == overlays
                && status.api_clients == api_clients
                && status.widgets_connected == widgets
                && status.api_requests == api_requests
                && status.save_error == save_error
                && status.reactions == reactions;
            status.sources = sources;
            status.overlays_connected = overlays;
            status.api_clients = api_clients;
            status.widgets_connected = widgets;
            status.api_requests = api_requests;
            status.save_error = save_error;
            status.reactions = reactions;
            !unchanged
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

/// A manual source: forwards what the test window sends until the window's
/// `ManualInput` is dropped (then it ends normally).
async fn run_manual(
    input: &tokio::sync::Mutex<mpsc::Receiver<ChatEvent>>,
    hub: &Hub,
    stats: &Stats,
    activity: Reporter,
) -> anyhow::Result<()> {
    let mut input = input.lock().await;
    activity.set(Activity::Receiving);
    while let Some(event) = input.recv().await {
        stats.record(&event);
        hub.publish(event);
    }
    Ok(())
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
