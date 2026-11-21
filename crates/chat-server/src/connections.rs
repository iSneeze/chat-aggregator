//! Counts connected overlays, API clients and widgets, for "OBS is
//! connected" in the status.
//!
//! Each connection holds a [`Guard`]; dropping it counts down again. That's
//! Rust's RAII pattern ("resource acquisition is initialization"): the
//! count goes down however the connection ends (closed normally, broken,
//! server shutting down) because Rust always runs `drop` when a value goes
//! away. There's no code path that could forget it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Caps on simultaneous overlay SSE streams and JSON API WebSocket clients.
/// Far above any real OBS/widget setup while protecting the 2-thread runtime
/// from connection floods.
pub(crate) const MAX_OVERLAYS: usize = 64;
pub(crate) const MAX_API_CLIENTS: usize = 64;

#[derive(Debug, Default)]
pub struct Connections {
    overlays: AtomicUsize,
    api_clients: AtomicUsize,
    widgets: AtomicUsize,
}

impl Connections {
    /// Overlays (e.g. OBS browser sources) currently connected.
    pub fn overlays(&self) -> usize {
        self.overlays.load(Ordering::Relaxed)
    }

    /// JSON API clients currently connected.
    pub fn api_clients(&self) -> usize {
        self.api_clients.load(Ordering::Relaxed)
    }

    /// Widgets currently connected: the API clients that are pages this
    /// server served (see `api::ws`). Also counted as API clients.
    pub fn widgets(&self) -> usize {
        self.widgets.load(Ordering::Relaxed)
    }

    pub(crate) fn try_overlay(self: &Arc<Self>) -> Option<Guard> {
        Guard::try_new(self.clone(), Kind::Overlay, MAX_OVERLAYS)
    }

    pub(crate) fn try_api_client(self: &Arc<Self>) -> Option<Guard> {
        Guard::try_new(self.clone(), Kind::ApiClient, MAX_API_CLIENTS)
    }

    pub(crate) fn widget(self: &Arc<Self>) -> Guard {
        Guard::new(self.clone(), Kind::Widget)
    }

    fn counter(&self, kind: Kind) -> &AtomicUsize {
        match kind {
            Kind::Overlay => &self.overlays,
            Kind::ApiClient => &self.api_clients,
            Kind::Widget => &self.widgets,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Overlay,
    ApiClient,
    Widget,
}

/// One live connection; counted while it exists.
pub(crate) struct Guard {
    connections: Arc<Connections>,
    kind: Kind,
}

impl Guard {
    fn new(connections: Arc<Connections>, kind: Kind) -> Self {
        connections.counter(kind).fetch_add(1, Ordering::Relaxed);
        Self { connections, kind }
    }

    /// Atomically increments the counter if below `limit`, returning `None`
    /// when the cap is reached.
    fn try_new(connections: Arc<Connections>, kind: Kind, limit: usize) -> Option<Self> {
        let counter = connections.counter(kind);
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < limit).then_some(n + 1)
            })
            .ok()?;
        Some(Self { connections, kind })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.connections
            .counter(self.kind)
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_count_up_and_down() {
        let connections = Arc::new(Connections::default());
        let a = connections.try_overlay().unwrap();
        let b = connections.try_overlay().unwrap();
        let c = connections.try_api_client().unwrap();
        let d = connections.widget();
        assert_eq!((connections.overlays(), connections.api_clients()), (2, 1));
        assert_eq!(connections.widgets(), 1);
        drop(a);
        drop(c);
        drop(d);
        assert_eq!((connections.overlays(), connections.api_clients()), (1, 0));
        assert_eq!(connections.widgets(), 0);
        drop(b);
        assert_eq!(connections.overlays(), 0);
    }

    #[test]
    fn connection_cap_refuses_above_limit() {
        let connections = Arc::new(Connections::default());
        let guards: Vec<_> = (0..MAX_OVERLAYS)
            .map(|_| connections.try_overlay().expect("within cap"))
            .collect();
        assert!(connections.try_overlay().is_none());
        drop(guards);
        assert!(connections.try_overlay().is_some());
    }
}
