//! Counts connected overlays and API clients, for "OBS is connected" in the
//! status.
//!
//! Each connection holds a [`Guard`]; dropping it counts down again. That's
//! Rust's RAII pattern ("resource acquisition is initialization"): the
//! count goes down however the connection ends (closed normally, broken,
//! server shutting down) because Rust always runs `drop` when a value goes
//! away. There's no code path that could forget it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
pub struct Connections {
    overlays: AtomicUsize,
    api_clients: AtomicUsize,
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

    pub(crate) fn overlay(self: &Arc<Self>) -> Guard {
        Guard::new(self.clone(), Kind::Overlay)
    }

    pub(crate) fn api_client(self: &Arc<Self>) -> Guard {
        Guard::new(self.clone(), Kind::ApiClient)
    }

    fn counter(&self, kind: Kind) -> &AtomicUsize {
        match kind {
            Kind::Overlay => &self.overlays,
            Kind::ApiClient => &self.api_clients,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Overlay,
    ApiClient,
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
        let a = connections.overlay();
        let b = connections.overlay();
        let c = connections.api_client();
        assert_eq!((connections.overlays(), connections.api_clients()), (2, 1));
        drop(a);
        drop(c);
        assert_eq!((connections.overlays(), connections.api_clients()), (1, 0));
        drop(b);
        assert_eq!(connections.overlays(), 0);
    }
}
