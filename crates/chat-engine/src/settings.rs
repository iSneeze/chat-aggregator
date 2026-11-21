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
use chat_core::tiers::TipSettings;
use chat_reactions::ReactionSettings;
use chat_server::{ApiOrigins, OverlayConfig};
use tokio::sync::watch;

use crate::config_file::Settings;

/// The running parts that settings steer, besides the sources (the actor
/// restarts those itself). The server reads the other ends of these
/// channels (`ServerState`), the reactor its own (`reactions`).
pub(crate) struct Controls {
    pub(crate) hub: Arc<Hub>,
    pub(crate) overlay: watch::Sender<OverlayConfig>,
    pub(crate) reload: watch::Sender<u64>,
    pub(crate) api: watch::Sender<bool>,
    pub(crate) origins: Arc<ApiOrigins>,
    /// The chat reactions' rules and outputs: the reactor applies a change
    /// right away (and reconnects an output whose address changed).
    pub(crate) reactions: watch::Sender<ReactionSettings>,
    /// The tip settings: one channel for everything that reads them, the
    /// reactor's amount rules (the tip currency) and the tip sources
    /// (through the source factory, see `Engine::start`).
    pub(crate) tips: watch::Sender<TipSettings>,
    /// Where theme names in the settings are looked up.
    pub(crate) themes_dir: Option<PathBuf>,
}

impl Controls {
    /// Hands what changed from `old` to `new` to the running parts, and
    /// leaves everything else alone. The YouTube settings are the actor's
    /// business (they mean restarting sources): returns whether they
    /// changed.
    pub(crate) fn apply(&self, old: &Settings, new: &Settings) -> bool {
        let (was_server, now_server) = (&old.server, &new.server);
        if now_server.history != was_server.history {
            self.hub.set_history(now_server.history);
        }
        // Off also disconnects the connected clients.
        if now_server.api != was_server.api {
            self.api.send_replace(now_server.api);
        }
        // A removed site's clients are disconnected.
        if now_server.api_origins != was_server.api_origins {
            self.origins.set_allowed(now_server.api_origins.clone());
        }

        let (was_overlay, now_overlay) = (&old.profile.overlay, &new.profile.overlay);
        let stagger_changed = now_server.stagger() != was_server.stagger();
        let hide_changed = now_overlay.hide != was_overlay.hide;
        let newest_changed = now_overlay.newest != was_overlay.newest;
        let theme_changed = now_overlay.theme != was_overlay.theme;
        let options_changed = now_overlay.theme_options != was_overlay.theme_options;
        if stagger_changed || hide_changed || newest_changed || theme_changed || options_changed {
            self.overlay.send_replace(OverlayConfig {
                theme: now_overlay.theme_source(self.themes_dir.as_deref()),
                theme_options: now_overlay.theme_options.clone(),
                stagger: now_server.stagger(),
                newest: now_overlay.newest,
                hide: now_overlay.hide.clone(),
            });
        }
        // Theme and direction are picked up when an overlay connects/loads, so
        // changing either reloads connected overlays once (explicitly, after
        // the new `OverlayConfig` is published above). Pacing, `hide`, and
        // `theme_options` apply live without a reload.
        if theme_changed || newest_changed {
            self.reload_overlays();
        }

        let (was, now) = (&old.profile, &new.profile);
        if now.reactions != was.reactions {
            self.reactions.send_replace(now.reactions.clone());
        }
        // Read for the next message or tip: no restart of anything.
        if now.tips != was.tips {
            self.tips.send_replace(now.tips.clone());
        }
        // (A new port only applies at the next start: the server keeps its
        // socket until then.)
        old.youtube != new.youtube
    }

    /// Makes connected overlays reload, e.g. after editing a theme's files.
    pub(crate) fn reload_overlays(&self) {
        self.reload.send_modify(|n| *n += 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_server::{Builtin, Hide, Newest, Stagger, ThemeSource};

    /// Controls, and the receiving ends the server would hold.
    struct Server {
        overlay: watch::Receiver<OverlayConfig>,
        reload: watch::Receiver<u64>,
        api: watch::Receiver<bool>,
        reactions: watch::Receiver<ReactionSettings>,
        tips: watch::Receiver<TipSettings>,
    }

    fn controls() -> (Controls, Server) {
        let (overlay, overlay_rx) = watch::channel(OverlayConfig::default());
        let (reload, reload_rx) = watch::channel(0u64);
        let (api, api_rx) = watch::channel(false);
        let (reactions, reactions_rx) = watch::channel(ReactionSettings::default());
        let (tips, tips_rx) = watch::channel(TipSettings::default());
        let controls = Controls {
            hub: Arc::new(Hub::new(20)),
            overlay,
            reload,
            api,
            origins: Arc::new(ApiOrigins::new([], false)),
            reactions,
            tips,
            themes_dir: None,
        };
        let server = Server {
            overlay: overlay_rx,
            reload: reload_rx,
            api: api_rx,
            reactions: reactions_rx,
            tips: tips_rx,
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
        assert!(changed(&server.overlay));
        assert!(server.overlay.borrow().stagger.is_off());
        assert!(!changed(&server.reload), "no reload for pacing");
        assert!(!changed(&server.api));
        assert!(!changed(&server.reactions));
        assert!(!changed(&server.tips));
    }

    #[test]
    fn new_tip_settings_are_sent_without_a_restart() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.profile.tips.currency = "EUR".into();
        assert!(!controls.apply(&old, &new), "no YouTube restart");
        assert!(changed(&server.tips));
        assert_eq!(server.tips.borrow().currency(), Some("EUR"));
        assert!(!changed(&server.reactions));
        assert!(!changed(&server.reload), "overlays don't reload");
    }

    #[test]
    fn what_overlays_hide_applies_without_a_reload() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.profile.overlay.hide = vec![Hide::StreamElements];
        controls.apply(&old, &new);
        assert!(changed(&server.overlay));
        assert_eq!(server.overlay.borrow().hide, [Hide::StreamElements]);
        assert!(!changed(&server.reload), "overlays don't reload");
    }

    #[test]
    fn new_reaction_settings_reach_the_reactor() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.profile.reactions.paused = true;
        assert!(!controls.apply(&old, &new));
        assert!(changed(&server.reactions));
        assert!(server.reactions.borrow().paused);
        assert!(!changed(&server.reload), "overlays don't reload");
    }

    #[test]
    fn a_new_direction_reloads_the_overlays() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.profile.overlay.newest = Newest::Top;
        controls.apply(&old, &new);
        assert_eq!(server.overlay.borrow().newest, Newest::Top);
        assert!(changed(&server.reload), "overlays reload");
        assert_eq!(
            server.overlay.borrow().theme,
            ThemeSource::default(),
            "same theme"
        );
    }

    #[test]
    fn a_new_theme_reaches_the_overlays() {
        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        new.profile.overlay.theme = Some("minimal".into());
        new.profile.overlay.newest = Newest::Top;
        controls.apply(&old, &new);
        assert_eq!(
            server.overlay.borrow().theme,
            ThemeSource::Builtin(Builtin::Minimal)
        );
        assert_eq!(server.overlay.borrow().newest, Newest::Top);
        assert_eq!(*server.reload.borrow(), 1, "reloaded once for both");
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
        assert!(!changed(&server.overlay));
        assert!(!changed(&server.reload));
    }

    #[test]
    fn theme_options_apply_without_a_reload() {
        use crate::theme_options::{ThemeValue, ThemeValues};

        let (controls, server) = controls();
        let old = Settings::default();
        let mut new = old.clone();
        let mut opts = ThemeValues::new();
        opts.insert("text_color".into(), ThemeValue::Text("#ff00aa".into()));
        new.profile
            .overlay
            .theme_options
            .insert("default".into(), opts.clone());

        controls.apply(&old, &new);
        assert!(changed(&server.overlay));
        assert_eq!(
            server.overlay.borrow().theme_options.get("default"),
            Some(&opts)
        );
        assert!(
            !changed(&server.reload),
            "theme options update live over SSE without reloading"
        );
    }
}
