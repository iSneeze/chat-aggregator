//! Applying settings while the engine runs.
//!
//! The engine owns the settings ([`Settings`]). Windows send a change as a
//! closure (`EngineHandle::edit_settings`), the actor runs it on the current
//! settings, and [`Controls::apply`] hands what changed to the running
//! parts. So one place decides what a change means (a new chat direction
//! reloads the overlays, a new port waits for the next start, …) and a
//! window only says what it wants changed, never touching what it doesn't
//! know about.

use std::path::PathBuf;
use std::sync::Arc;

use chat_core::Hub;
use chat_server::{ApiOrigins, Newest, Stagger, ThemeSource};
use tokio::sync::watch;

use crate::config_file::Settings;

/// The running parts that settings steer, besides the sources (the actor
/// restarts those itself). The server reads the other ends of these
/// channels (`ServerState`).
pub(crate) struct Controls {
    pub(crate) hub: Arc<Hub>,
    pub(crate) theme: watch::Sender<ThemeSource>,
    pub(crate) stagger: watch::Sender<Stagger>,
    pub(crate) api: watch::Sender<bool>,
    pub(crate) newest: watch::Sender<Newest>,
    pub(crate) origins: Arc<ApiOrigins>,
    /// Where theme names in the settings are looked up.
    pub(crate) themes_dir: Option<PathBuf>,
}

impl Controls {
    /// Hands what changed from `old` to `new` to the running parts, and
    /// leaves everything else alone. The YouTube settings are the actor's
    /// business (they mean restarting sources): returns whether they
    /// changed.
    pub(crate) fn apply(&self, old: &Settings, new: &Settings) -> bool {
        let (was, now) = (&old.server, &new.server);
        if now.history != was.history {
            self.hub.set_history(now.history);
        }
        // Read by overlays when they connect: no reload for this one.
        if now.stagger() != was.stagger() {
            self.stagger.send_replace(now.stagger());
        }
        // Off also disconnects the connected clients.
        if now.api != was.api {
            self.api.send_replace(now.api);
        }
        // A removed site's clients are disconnected.
        if now.api_origins != was.api_origins {
            self.origins.set_allowed(now.api_origins.clone());
        }
        // An overlay takes up both when it connects (the direction is
        // written into its page), so either change reloads the overlays,
        // once. The direction goes first, so the reloaded page has it.
        if now.newest != was.newest {
            self.newest.send_replace(now.newest);
        }
        if now.theme != was.theme {
            // A new value notifies the overlays, which then reload.
            let theme = now.theme_source(self.themes_dir.as_deref());
            self.theme.send_replace(theme);
        } else if now.newest != was.newest {
            self.reload_overlays();
        }
        // (A new port only applies at the next start: the server keeps its
        // socket until then.)
        old.youtube != new.youtube
    }

    /// Makes connected overlays reload, e.g. after editing a theme's files.
    pub(crate) fn reload_overlays(&self) {
        // `send_modify` notifies watchers even though nothing changed.
        self.theme.send_modify(|_| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_server::Builtin;

    /// Controls, and the receiving ends the server would hold.
    struct Server {
        theme: watch::Receiver<ThemeSource>,
        stagger: watch::Receiver<Stagger>,
        api: watch::Receiver<bool>,
        newest: watch::Receiver<Newest>,
    }

    fn controls() -> (Controls, Server) {
        let (theme, theme_rx) = watch::channel(ThemeSource::default());
        let (stagger, stagger_rx) = watch::channel(Stagger::default());
        let (api, api_rx) = watch::channel(false);
        let (newest, newest_rx) = watch::channel(Newest::Bottom);
        let controls = Controls {
            hub: Arc::new(Hub::new(20)),
            theme,
            stagger,
            api,
            newest,
            origins: Arc::new(ApiOrigins::new([], false)),
            themes_dir: None,
        };
        let server = Server {
            theme: theme_rx,
            stagger: stagger_rx,
            api: api_rx,
            newest: newest_rx,
        };
        (controls, server)
    }

    fn changed<T>(rx: &watch::Receiver<T>) -> bool {
        rx.has_changed().unwrap()
    }

    #[test]
    fn only_what_changed_is_touched() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.server.stagger_ms = 0;
        new.server.port = 9000;
        new.app.appearance = crate::Appearance::Dark;

        assert!(!controls.apply(&old, &new), "YouTube unchanged");
        assert!(changed(&server.stagger));
        assert!(server.stagger.borrow().is_off());
        assert!(!changed(&server.theme), "no reload for pacing");
        assert!(!changed(&server.api));
        assert!(!changed(&server.newest));
    }

    #[test]
    fn a_new_direction_reloads_the_overlays() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.server.newest = Newest::Top;
        controls.apply(&old, &new);
        assert_eq!(*server.newest.borrow(), Newest::Top);
        assert!(changed(&server.theme), "overlays reload");
        assert_eq!(*server.theme.borrow(), ThemeSource::default(), "same theme");
    }

    #[test]
    fn a_new_theme_reaches_the_overlays() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.server.theme = Some("minimal".into());
        new.server.newest = Newest::Top;
        controls.apply(&old, &new);
        assert_eq!(
            *server.theme.borrow(),
            ThemeSource::Builtin(Builtin::Minimal)
        );
        assert_eq!(*server.newest.borrow(), Newest::Top);
    }

    #[test]
    fn history_and_the_api_switch_apply_at_once() {
        let (controls, server) = controls();
        for i in 0..5 {
            let mut msg = chat_core::demo::sample_messages(0).remove(0);
            msg.id = format!("m{i}");
            controls.hub.publish(chat_core::ChatEvent::Message(msg));
        }
        let old = Settings::default();
        let mut new = old.clone();
        new.server.history = 2;
        new.server.api = true;
        controls.apply(&old, &new);
        assert_eq!(controls.hub.subscribe().0.len(), 2, "the oldest went");
        assert!(*server.api.borrow());
    }

    #[test]
    fn youtube_changes_are_left_to_the_caller() {
        let (controls, _server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.youtube.api_key = Some("key".into());
        assert!(controls.apply(&old, &new));
    }

    #[test]
    fn the_same_pacing_written_differently_changes_nothing() {
        let (controls, server) = controls();
        // The longest delay is capped at 5 s: 5 s and 6 s are the same.
        let mut old = Settings::default();
        old.server.stagger_max_ms = Stagger::MAX_DELAY_LIMIT.as_millis() as u64;
        let mut new = old.clone();
        new.server.stagger_max_ms += 1000;
        controls.apply(&old, &new);
        assert!(!changed(&server.stagger));
    }
}
