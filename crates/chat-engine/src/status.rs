//! What the engine reports: per-source state, and a traffic-light `Health`
//! for status displays. The mapping to colours lives here, not in the UI,
//! so headless mode and the app always agree.

use std::fmt;

use chat_core::Activity;
use chat_reactions::{OutputState, ReactionStatus};
use jiff::Timestamp;

use crate::config::{SourceConfig, YouTubeSettings};

/// Identifies a source for its whole life in the engine, across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub(crate) u64);

impl SourceId {
    /// The raw number, e.g. to build UI element ids.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The engine's view of a source.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceState {
    /// Turned off by you.
    Stopped,
    /// Its task is running, and this is what the source reports it's doing.
    /// Inside the variant because only a running source has an activity:
    /// a stopped one with a leftover "receiving chat" can't be written.
    Running(Activity),
    /// Failed; starts again automatically at `next_try`.
    Retrying {
        attempt: u32,
        next_try: Timestamp,
        error: String,
    },
    /// Failed in a way only you can fix (login, missing setting): not
    /// restarted until you change something.
    NeedsAttention { error: String },
    /// Ended normally, e.g. the watched YouTube video's stream is over.
    Finished,
}

/// Traffic light. Ordered from best to worst so the overall health is
/// simply the `max()` over all sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Health {
    /// Grey: stopped or finished.
    Off,
    /// Green: working (receiving chat, or waiting for your broadcast).
    Ok,
    /// Yellow: disrupted, but recovering by itself.
    Warning,
    /// Red: no chat until something changes (you, or time: e.g. quota).
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceStatus {
    pub id: SourceId,
    /// Short name for status displays: "Twitch: your_channel", "YouTube:
    /// <the account's channel name>", "Test messages".
    pub label: String,
    /// Its settings; `None` for the app's test window source, which has
    /// none: the window feeds it, and closing the window removes it.
    pub config: Option<SourceConfig>,
    pub state: SourceState,
    /// Chat messages received since the engine started.
    pub messages: u64,
    pub last_message: Option<Timestamp>,
}

impl SourceStatus {
    pub fn health(&self) -> Health {
        match &self.state {
            SourceState::Stopped | SourceState::Finished => Health::Off,
            SourceState::Retrying { .. } => Health::Warning,
            SourceState::NeedsAttention { .. } => Health::Error,
            SourceState::Running(activity) => match activity {
                Activity::Receiving | Activity::Idle(_) => Health::Ok,
                Activity::Connecting | Activity::Degraded(_) => Health::Warning,
                Activity::Blocked(_) => Health::Error,
            },
        }
    }

    /// One line for next to the status light. Colour alone isn't enough
    /// (about 1 in 12 men can't tell red from green), and the text is where
    /// the actual information is anyway.
    pub fn summary(&self) -> String {
        match &self.state {
            SourceState::Stopped => "stopped".into(),
            SourceState::Finished => "ended".into(),
            SourceState::NeedsAttention { error } => error.clone(),
            SourceState::Retrying {
                next_try, error, ..
            } => {
                let secs = next_try.duration_since(Timestamp::now()).as_secs().max(0);
                format!("retrying in {secs} s: {error}")
            }
            SourceState::Running(activity) => match activity {
                Activity::Connecting => "connecting…".into(),
                Activity::Receiving => "receiving chat".into(),
                Activity::Idle(text) | Activity::Degraded(text) | Activity::Blocked(text) => {
                    text.clone()
                }
            },
        }
    }
}

/// Short name for status displays and logs: from the source's settings
/// (and the YouTube accounts' names), or the test window's (which has
/// none).
pub(crate) fn label(config: Option<&SourceConfig>, youtube: &YouTubeSettings) -> String {
    config.map_or_else(|| "Test messages".into(), |c| c.label_with(youtube))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Where to point OBS: `http://127.0.0.1:7878/`.
    pub overlay_url: String,
    /// Overlays currently connected (e.g. OBS browser sources): 0 means
    /// nothing is showing the chat right now.
    pub overlays_connected: usize,
    /// Programs currently connected to the JSON API.
    pub api_clients: usize,
    /// Widgets currently connected (pages from `/widgets/`, see
    /// `chat_server::widgets`). Also counted in `api_clients`.
    pub widgets_connected: usize,
    /// Web sites waiting for the streamer to allow or deny them the JSON
    /// API, oldest first: answer with `EngineHandle::answer_api_request`.
    pub api_requests: Vec<String>,
    /// Why saving `config.toml` failed the last time; `None` once a save
    /// works again (and always, if the engine has no file to save to).
    pub save_error: Option<String>,
    /// The active profile: whose settings and sources these are.
    pub profile: String,
    /// All profiles, the active one too, sorted by name (case doesn't
    /// count). Empty if the engine has no settings folder (headless mode,
    /// tests): nothing to switch to.
    pub profiles: Vec<String>,
    /// How often another profile became the active one since the engine
    /// started. Windows holding a profile's settings (a draft, a form)
    /// reload when this changes; renaming the active profile changes its
    /// name, but not this.
    pub profile_switches: u64,
    /// This run converted a `config.toml` from before profiles into today's
    /// layout (the old file is kept as `config.old.toml` next to it): worth
    /// telling the streamer once.
    pub converted_to_profiles: bool,
    /// In the order they were added.
    pub sources: Vec<SourceStatus>,
    /// Chat reactions: the outputs in use, with a light each
    /// (`Health::from(&output.state)`), and how often each rule fired.
    pub reactions: ReactionStatus,
}

/// An output's light. Can't reach the program: red, since only the streamer
/// can fix that (start Warudo, correct the address); it's retried anyway.
// (`From` is implemented here, next to `Health`: Rust allows implementing
// a trait for a type only in the crate of either one, and `OutputState`
// lives in chat-reactions, which doesn't know about lights.)
impl From<&OutputState> for Health {
    fn from(state: &OutputState) -> Self {
        match state {
            OutputState::Connected => Health::Ok,
            OutputState::Connecting => Health::Warning,
            OutputState::Retrying { .. } => Health::Error,
        }
    }
}

impl Status {
    /// The worst health of all sources (`Off` without sources): e.g. for a
    /// window title or tray icon.
    pub fn overall(&self) -> Health {
        self.sources
            .iter()
            .map(SourceStatus::health)
            .max()
            .unwrap_or(Health::Off)
    }

    pub fn source(&self, id: SourceId) -> Option<&SourceStatus> {
        self.sources.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: SourceState) -> SourceStatus {
        SourceStatus {
            id: SourceId(1),
            label: "Demo".into(),
            config: Some(SourceConfig::Demo),
            state,
            messages: 0,
            last_message: None,
        }
    }

    #[test]
    fn overall_is_the_worst() {
        let mut all = Status {
            overlay_url: String::new(),
            overlays_connected: 0,
            api_clients: 0,
            widgets_connected: 0,
            api_requests: vec![],
            save_error: None,
            profile: "Main".into(),
            profiles: vec![],
            profile_switches: 0,
            converted_to_profiles: false,
            sources: vec![],
            reactions: ReactionStatus::default(),
        };
        assert_eq!(all.overall(), Health::Off);
        all.sources
            .push(status(SourceState::Running(Activity::Receiving)));
        assert_eq!(all.overall(), Health::Ok);
        all.sources
            .push(status(SourceState::Running(Activity::Blocked("q".into()))));
        assert_eq!(all.overall(), Health::Error);
    }
}
