//! What the engine reports: per-source state, and a traffic-light `Health`
//! for status displays. The mapping to colours lives here, not in the UI,
//! so headless mode and the app always agree.

use std::fmt;

use chat_core::Activity;
use chrono::{DateTime, Utc};

use crate::config::SourceConfig;

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
    /// Its task is running; what it's doing is in `SourceStatus::activity`.
    Running,
    /// Failed; starts again automatically at `next_try`.
    Retrying {
        attempt: u32,
        next_try: DateTime<Utc>,
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
    pub config: SourceConfig,
    pub state: SourceState,
    /// What the source reports it's doing; only while `Running`.
    pub activity: Option<Activity>,
    /// Chat messages received since the engine started.
    pub messages: u64,
    pub last_message: Option<DateTime<Utc>>,
}

impl SourceStatus {
    pub fn label(&self) -> String {
        self.config.label()
    }

    pub fn health(&self) -> Health {
        match &self.state {
            SourceState::Stopped | SourceState::Finished => Health::Off,
            SourceState::Retrying { .. } => Health::Warning,
            SourceState::NeedsAttention { .. } => Health::Error,
            SourceState::Running => match &self.activity {
                Some(Activity::Receiving | Activity::Idle(_)) => Health::Ok,
                Some(Activity::Connecting | Activity::Degraded(_)) | None => Health::Warning,
                Some(Activity::Blocked(_)) => Health::Error,
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
                let secs = (*next_try - Utc::now()).num_seconds().max(0);
                format!("retrying in {secs} s: {error}")
            }
            SourceState::Running => match &self.activity {
                None | Some(Activity::Connecting) => "connecting…".into(),
                Some(Activity::Receiving) => "receiving chat".into(),
                Some(Activity::Idle(text) | Activity::Degraded(text) | Activity::Blocked(text)) => {
                    text.clone()
                }
            },
        }
    }
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
    /// In the order they were added.
    pub sources: Vec<SourceStatus>,
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

    fn status(state: SourceState, activity: Option<Activity>) -> SourceStatus {
        SourceStatus {
            id: SourceId(1),
            config: SourceConfig::Demo,
            state,
            activity,
            messages: 0,
            last_message: None,
        }
    }

    #[test]
    fn traffic_light_mapping() {
        use Activity::*;
        let running = |a| status(SourceState::Running, Some(a)).health();
        assert_eq!(running(Receiving), Health::Ok);
        assert_eq!(running(Idle("waiting for broadcast".into())), Health::Ok);
        assert_eq!(running(Connecting), Health::Warning);
        assert_eq!(running(Degraded("rate limited".into())), Health::Warning);
        assert_eq!(running(Blocked("quota".into())), Health::Error);

        let retrying = SourceState::Retrying {
            attempt: 1,
            next_try: Utc::now(),
            error: "x".into(),
        };
        assert_eq!(status(retrying, None).health(), Health::Warning);
        let attention = SourceState::NeedsAttention { error: "x".into() };
        assert_eq!(status(attention, None).health(), Health::Error);
        assert_eq!(status(SourceState::Stopped, None).health(), Health::Off);
        assert_eq!(status(SourceState::Finished, None).health(), Health::Off);
    }

    #[test]
    fn overall_is_the_worst() {
        let mut all = Status {
            overlay_url: String::new(),
            overlays_connected: 0,
            api_clients: 0,
            sources: vec![],
        };
        assert_eq!(all.overall(), Health::Off);
        all.sources
            .push(status(SourceState::Running, Some(Activity::Receiving)));
        assert_eq!(all.overall(), Health::Ok);
        all.sources.push(status(
            SourceState::Running,
            Some(Activity::Blocked("q".into())),
        ));
        assert_eq!(all.overall(), Health::Error);
    }
}
