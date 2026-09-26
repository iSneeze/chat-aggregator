//! The settings window (⚙): the app's look, the overlay's replay history
//! and burst pacing, the port, the YouTube API key, the settings folder.
//!
//! gpui-kit's settings fields report every keystroke. Applying each one
//! would go wrong: typing "25" over "20" passes through "2", which would
//! throw away 18 messages of replay history, and every letter of an API key
//! would restart YouTube. So edits go into a draft that is applied (engine
//! and `config.toml`) once typing has paused for a moment: a "debounce".
//! The look is the exception: it applies on the spot.

use std::time::Duration;

use chat_engine::{
    AppSettings, Appearance, DEFAULT_HISTORY, DEFAULT_PORT, EngineHandle, ServerSettings, Stagger,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::*;

use crate::app_config::AppConfig;
use crate::app_view::report;
use crate::appearance;

/// How long typing must pause before edits apply.
const APPLY_AFTER: Duration = Duration::from_millis(800);

/// The dropdown's entries: the value saved in `config.toml`, and its label.
const APPEARANCES: [(Appearance, &str, &str); 4] = [
    (Appearance::System, "system", "System (light or dark)"),
    (Appearance::Light, "light", "Light"),
    (Appearance::Dark, "dark", "Dark"),
    (Appearance::HighContrast, "high-contrast", "High contrast"),
];

/// The settings being edited, not yet applied.
#[derive(Clone, PartialEq)]
struct Draft {
    server: ServerSettings,
    api_key: String,
}

pub struct SettingsWindow {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    /// For reporting problems from places that don't get the window
    /// (the timer, the settings fields' callbacks).
    window: AnyWindowHandle,
    draft: Draft,
    /// The pending "apply the draft" timer. Storing a new one drops the old
    /// one, and dropping a GPUI `Task` cancels it: that's the whole debounce.
    pending: Option<Task<()>>,
    /// The draft has changes that aren't applied yet.
    dirty: bool,
    api_key: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = config.read(cx);
        let draft = Draft {
            server: current.server().clone(),
            api_key: current.youtube().api_key.clone().unwrap_or_default(),
        };
        // Our own input (not gpui-kit's settings text field) to mask it.
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional")
                .masked(true)
                .default_value(draft.api_key.clone())
        });
        let subscriptions = vec![
            cx.subscribe(&api_key, |this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let key = input.read(cx).value().trim().to_string();
                    this.change(|draft| draft.api_key = key, cx);
                }
            }),
            // Closing the window within the pause mustn't lose the last
            // edit: apply it now. (No window to report in any more, so
            // problems only go to the log.)
            cx.on_release(|this, cx| {
                if this.dirty
                    && let Err(e) = this.apply(cx)
                {
                    tracing::warn!("couldn't save the settings: {e:#}");
                }
            }),
        ];
        Self {
            engine,
            config,
            window: window.window_handle(),
            draft,
            pending: None,
            dirty: false,
            api_key,
            _subscriptions: subscriptions,
        }
    }

    /// Edits the draft and (re)starts the timer that applies it.
    fn change(&mut self, edit: impl FnOnce(&mut Draft), cx: &mut Context<Self>) {
        let before = self.draft.clone();
        edit(&mut self.draft);
        if self.draft == before {
            return;
        }
        self.dirty = true;
        let window = self.window;
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPLY_AFTER).await;
            let _ = window.update(cx, |_, window, cx| {
                let result = this.update(cx, |this, cx| this.apply(cx));
                if let Ok(Err(e)) = result {
                    report(window, cx, format!("Couldn't save the settings: {e:#}"));
                }
            });
        }));
        cx.notify();
    }

    /// Hands the draft to the engine and saves it. Only what changed is
    /// touched: e.g. YouTube restarts only for a new API key. Takes `App`
    /// rather than `Context<Self>`: it's also called while the view is being
    /// released, when only `App` is available. (A `Context` works too: it
    /// derefs to `App`.)
    fn apply(&mut self, cx: &mut App) -> anyhow::Result<()> {
        self.dirty = false;
        let draft = self.draft.clone();
        let (server, mut youtube) = {
            let config = self.config.read(cx);
            (config.server().clone(), config.youtube().clone())
        };

        let new = &draft.server;
        if new.history != server.history {
            self.engine.set_history(new.history);
        }
        if (new.stagger_ms, new.stagger_max_ms) != (server.stagger_ms, server.stagger_max_ms) {
            self.engine.set_stagger(Stagger::new(
                Duration::from_millis(new.stagger_ms),
                Duration::from_millis(new.stagger_max_ms),
            ));
        }
        if new.api != server.api {
            self.engine.set_api_enabled(new.api);
        }
        // (A new port is only saved: the server keeps its socket until the
        // next start.)
        if *new != server {
            self.config
                .update(cx, |config, _| config.set_server(new.clone()))?;
        }

        let key = Some(draft.api_key).filter(|key| !key.is_empty());
        if key != youtube.api_key {
            youtube.api_key = key;
            self.config
                .update(cx, |config, _| config.set_youtube(youtube.clone()))?;
            let engine = self.engine.clone();
            cx.spawn(async move |_| {
                if let Err(e) = engine.update_youtube(youtube).await {
                    tracing::warn!("couldn't apply the YouTube API key: {e:#}");
                }
            })
            .detach();
        }
        Ok(())
    }

    /// Switches the JSON API on or off right away (a switch has no
    /// half-typed states to wait out). Edits still waiting for the pause go
    /// along with it.
    fn set_api(&mut self, on: bool, cx: &mut Context<Self>) {
        self.draft.server.api = on;
        // Dropping the pending timer cancels it; `apply` does its work now.
        self.pending = None;
        if let Err(e) = self.apply(cx) {
            let window = self.window;
            let message = format!("Couldn't save the settings: {e:#}");
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| report(window, cx, message));
            });
        }
        cx.notify();
    }

    /// Changes the look right away, and saves it.
    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        let saved = self
            .config
            .update(cx, |config, _| config.set_app(AppSettings { appearance }));
        // The settings fields call this while their window is busy
        // dispatching the click; `defer` runs it right after, when the
        // window is free to be updated.
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                appearance::apply(appearance, Some(window), cx);
                if let Err(e) = saved {
                    report(window, cx, format!("Couldn't save the settings: {e:#}"));
                }
            });
        });
    }

    // ---- drawing ----

    fn pages(&self, cx: &mut Context<Self>) -> Vec<SettingPage> {
        let view = cx.entity().downgrade();
        let settings_dir = self.config.read(cx).settings_dir();

        let look = {
            let (read, write) = (view.clone(), view.clone());
            SettingField::dropdown(
                APPEARANCES
                    .iter()
                    .map(|(_, key, label)| ((*key).into(), (*label).into()))
                    .collect(),
                move |cx| {
                    let current = read
                        .upgrade()
                        .map(|view| view.read(cx).config.read(cx).app().appearance)
                        .unwrap_or_default();
                    APPEARANCES
                        .iter()
                        .find(|(appearance, ..)| *appearance == current)
                        .map(|(_, key, _)| (*key).into())
                        .unwrap_or_default()
                },
                move |key: SharedString, cx| {
                    let chosen = APPEARANCES.iter().find(|(_, k, _)| *k == key.as_ref());
                    if let (Some((appearance, ..)), Some(view)) = (chosen, write.upgrade()) {
                        view.update(cx, |this, cx| this.set_appearance(*appearance, cx));
                    }
                },
            )
        };
        let open_folder = {
            let dir = settings_dir.clone();
            SettingField::render(move |_, _, _| {
                let dir = dir.clone();
                Button::new("settings-open-folder")
                    .icon(IconName::FolderOpen)
                    .label("Open folder")
                    .small()
                    .on_click(move |_, window, cx| {
                        // It exists once anything was saved; create it so
                        // there's something to open.
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            report(
                                window,
                                cx,
                                format!("Couldn't create {}: {e}", dir.display()),
                            );
                            return;
                        }
                        cx.open_with_system(&dir);
                    })
            })
        };
        let json_api = {
            let (read, write) = (view.clone(), view.clone());
            SettingField::switch(
                move |cx| {
                    read.upgrade()
                        .is_some_and(|view| view.read(cx).draft.server.api)
                },
                move |on, cx| {
                    if let Some(view) = write.upgrade() {
                        view.update(cx, |this, cx| this.set_api(on, cx));
                    }
                },
            )
        };
        // Where programs connect: the running server's address (a changed
        // port only applies after a restart).
        let api_url = self
            .engine
            .status()
            .borrow()
            .overlay_url
            .replacen("http://", "ws://", 1)
            + "api/v1/ws";
        let api_key = {
            let input = self.api_key.clone();
            SettingField::render(move |_, _, _| {
                div()
                    .w(px(240.))
                    .child(Input::new(&input).id("settings-api-key").mask_toggle())
            })
        };

        vec![
            SettingPage::new("General").groups([
                SettingGroup::new().title("Look").item(
                    SettingItem::new(
                        "Appearance",
                        look.default_value(SharedString::from("system")),
                    )
                    .description("High contrast: black, white and bright colours."),
                ),
                SettingGroup::new().title("Files").item(
                    SettingItem::new("Settings folder", open_folder).description(format!(
                        "config.toml, themes and emoji live in {}",
                        settings_dir.display()
                    )),
                ),
            ]),
            SettingPage::new("Overlay").group(
                SettingGroup::new()
                    .description(
                        "Changes reach overlays when they connect or reload; \
                         \"Reload overlays\" in the main window applies them to all.",
                    )
                    .items([
                        SettingItem::new(
                            "Replay history",
                            number(&view, 0., 200., 1., |d| &mut d.server.history)
                                .default_value(DEFAULT_HISTORY as f64),
                        )
                        .description("Messages a (re)loaded overlay shows right away."),
                        SettingItem::new(
                            "Burst spacing (ms)",
                            number(&view, 0., 1000., 50., |d| &mut d.server.stagger_ms)
                                .default_value(ServerSettings::default().stagger_ms as f64),
                        )
                        .description(
                            "When many messages arrive at once, show them this far apart \
                             (0: off).",
                        ),
                        SettingItem::new(
                            "Longest delay (ms)",
                            number(&view, 0., 5000., 250., |d| &mut d.server.stagger_max_ms)
                                .default_value(ServerSettings::default().stagger_max_ms as f64),
                        )
                        .description("A message is never held back longer than this."),
                    ]),
            ),
            SettingPage::new("Connection").group(
                SettingGroup::new().items([
                    SettingItem::new("JSON API", json_api.default_value(false)).description(
                        format!(
                            "Chat for your own programs (games, bots) at {api_url}. Off \
                             unless you need it: any web page open in your browser could \
                             connect to it too."
                        ),
                    ),
                    SettingItem::new(
                        "Port",
                        number(&view, 1024., 65535., 1., |d| &mut d.server.port)
                            .default_value(DEFAULT_PORT as f64),
                    )
                    .description(
                        "The overlay's address is http://127.0.0.1:<port>/. \
                     Takes effect after restarting the app.",
                    ),
                    SettingItem::new("YouTube API key", api_key).description(
                        "Optional: reads a video's chat (\"YouTube (a video)\") \
                     without the login.",
                    ),
                ]),
            ),
        ]
    }
}

/// A number field editing one number in the draft. `field` picks which
/// one: a plain function (`fn`, not a closure) is enough, since it captures
/// nothing, and it works for every number type via `Number`.
fn number<T: Number>(
    view: &WeakEntity<SettingsWindow>,
    min: f64,
    max: f64,
    step: f64,
    field: fn(&mut Draft) -> &mut T,
) -> SettingField<f64> {
    // Each closure owns its own handle: both must be `'static`, so they
    // can't borrow `view`. Weak handles: the fields mustn't keep the
    // window's view alive.
    let (read, write) = (view.clone(), view.clone());
    SettingField::number_input(
        NumberFieldOptions { min, max, step },
        move |cx| {
            let Some(view) = read.upgrade() else {
                return 0.;
            };
            // `field` hands out `&mut`, so it gets a copy of the draft:
            // reading must not change anything.
            let mut draft = view.read(cx).draft.clone();
            field(&mut draft).to_f64()
        },
        move |value, cx| {
            if let Some(view) = write.upgrade() {
                view.update(cx, |this, cx| {
                    this.change(|draft| *field(draft) = T::from_f64(value), cx)
                });
            }
        },
    )
}

/// The number types in the draft. gpui-kit's number fields work in `f64`;
/// their min/max already keep values in range, and rounding drops the
/// fraction of a half-typed "2.5".
// `'static`: the field's closures are stored and outlive this call, so
// the type they work with can't contain borrowed data.
trait Number: Copy + 'static {
    fn to_f64(self) -> f64;
    fn from_f64(value: f64) -> Self;
}

macro_rules! number_type {
    ($($t:ty),*) => {$(
        impl Number for $t {
            fn to_f64(self) -> f64 {
                self as f64
            }
            fn from_f64(value: f64) -> Self {
                value.round() as $t
            }
        }
    )*};
}
number_type!(u16, u64, usize);

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                Settings::new("settings")
                    .sidebar_width(px(170.))
                    .pages(self.pages(cx)),
            )
    }
}

/// UI tests, headless (see `app_view::tests` for why imports are explicit).
#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    use chat_core::ChatEvent;
    use chat_engine::{Appearance, ConfigFile, Engine, EngineConfig};
    use gpui_kit::component::{Root, Theme};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use super::{APPLY_AFTER, SettingsWindow};
    use crate::app_config::AppConfig;

    struct Ui {
        window: WindowHandle<Root>,
        view: Option<Entity<SettingsWindow>>,
        engine: Engine,
        path: PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    fn open(cx: &mut TestAppContext, name: &str) -> Ui {
        cx.update(gpui_kit::init);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let engine = runtime
            .block_on(Engine::start(EngineConfig {
                bind: (Ipv4Addr::LOCALHOST, 0).into(),
                ..EngineConfig::default()
            }))
            .unwrap();
        let path = std::env::temp_dir()
            .join(format!("chat-app-settings-{}-{name}", std::process::id()))
            .join("config.toml");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let config = AppConfig::new(ConfigFile::default(), path.clone());
        let handle = engine.handle();
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = cx.new(|_| config);
            let settings = cx.new(|cx| SettingsWindow::new(handle, config, window, cx));
            view = Some(settings.clone());
            Root::new(settings, window, cx)
        });
        Ui {
            window,
            view,
            engine,
            path,
            _runtime: runtime,
        }
    }

    impl Ui {
        fn view(&self) -> &Entity<SettingsWindow> {
            self.view.as_ref().unwrap()
        }

        /// What a number field does on each keystroke.
        fn type_history(&self, value: usize, cx: &mut TestAppContext) {
            self.view().update(cx, |this, cx| {
                this.change(|draft| draft.server.history = value, cx);
            });
        }

        fn fill_history(&self, count: usize) {
            for i in 0..count {
                let mut msg = chat_core::demo::sample_messages(0).remove(0);
                msg.id = format!("m{i}");
                self.engine.hub().publish(ChatEvent::Message(msg));
            }
        }

        fn replayed(&self) -> usize {
            self.engine.hub().subscribe().0.len()
        }

        fn saved(&self) -> Option<ConfigFile> {
            ConfigFile::load(&self.path).ok()
        }
    }

    #[gpui_kit::test]
    fn typing_applies_only_the_final_value_after_a_pause(cx: &mut TestAppContext) {
        let ui = open(cx, "debounce");
        ui.fill_history(20);

        // "20" → "2" → "25", typed quickly.
        ui.type_history(2, cx);
        cx.executor().advance_clock(APPLY_AFTER / 2);
        ui.type_history(25, cx);
        cx.executor().advance_clock(APPLY_AFTER / 2);
        cx.run_until_parked();
        assert!(ui.saved().is_none(), "nothing applied while typing");
        assert_eq!(ui.replayed(), 20);

        cx.executor().advance_clock(APPLY_AFTER);
        cx.run_until_parked();
        assert_eq!(ui.saved().unwrap().server.history, 25);
        assert_eq!(ui.replayed(), 20, "the \"2\" in between never applied");
        ui.fill_history(10);
        assert_eq!(ui.replayed(), 25, "the engine uses the new size");
    }

    #[gpui_kit::test]
    fn closing_the_window_applies_what_is_pending(cx: &mut TestAppContext) {
        let mut ui = open(cx, "close");
        ui.view().update(cx, |this, cx| {
            this.change(|draft| draft.server.stagger_ms = 0, cx);
        });
        // Nothing may hold the view any more for it to be released.
        ui.view = None;
        cx.update_window(ui.window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(ui.saved().unwrap().server.stagger_ms, 0);
        assert!(ui.engine.handle().stagger().is_off());
    }

    #[gpui_kit::test]
    fn the_json_api_switches_at_once_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, "api");
        assert!(!ui.engine.handle().api_enabled(), "off by default");
        ui.view().update(cx, |this, cx| this.set_api(true, cx));
        assert!(ui.engine.handle().api_enabled(), "no waiting");
        assert!(ui.saved().unwrap().server.api);

        ui.view().update(cx, |this, cx| this.set_api(false, cx));
        assert!(!ui.engine.handle().api_enabled());
        assert!(!ui.saved().unwrap().server.api);
    }

    #[gpui_kit::test]
    fn a_new_look_applies_at_once_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, "look");
        ui.view().update(cx, |this, cx| {
            this.set_appearance(Appearance::HighContrast, cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "High Contrast");
        });
        assert_eq!(ui.saved().unwrap().app.appearance, Appearance::HighContrast);
    }
}
