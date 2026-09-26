//! What a source is doing right now, for status displays.
//!
//! The engine only knows whether a source's task is running. Whether chat
//! actually comes in only the source knows: a YouTube source waiting for the
//! daily quota reset is "running" but delivers nothing for hours. So sources
//! report it through a [`Reporter`].

use tokio::sync::watch;

/// A source's current activity. The text is for humans (shown next to the
/// status light), so it should say what's happening and, if it's waiting,
/// until when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// Setting up the connection.
    Connecting,
    /// Connected; chat comes in whenever someone writes.
    Receiving,
    /// Nothing to receive right now, and that's normal: e.g. waiting for
    /// your broadcast to start.
    Idle(String),
    /// Disrupted, but recovering by itself: e.g. rate limited, reconnecting
    /// after an error.
    Degraded(String),
    /// No chat until something changes that the source can't fix quickly:
    /// e.g. the daily API quota is used up.
    Blocked(String),
}

/// Handed to [`crate::ChatSource::run`]; the source calls [`set`](Self::set)
/// whenever its activity changes.
///
/// A thin wrapper around a `watch` channel: it only keeps the latest value,
/// which is all a status display needs, and setting it never blocks or fails,
/// even if nobody is listening.
pub struct Reporter(watch::Sender<Activity>);

impl Reporter {
    /// A reporter and the receiving end to watch it.
    pub fn new() -> (Self, watch::Receiver<Activity>) {
        let (tx, rx) = watch::channel(Activity::Connecting);
        (Self(tx), rx)
    }

    /// A reporter nobody listens to (examples, tests).
    pub fn detached() -> Self {
        Self::new().0
    }

    pub fn set(&self, activity: Activity) {
        // Only notify watchers if something actually changed.
        self.0.send_if_modified(|current| {
            if *current == activity {
                false
            } else {
                *current = activity;
                true
            }
        });
    }
}
