//! The YouTube section of the window: entering your Google project's
//! client, logging in through the browser, "connected as …", logging out.
//!
//! The engine does the login itself (`EngineHandle::youtube_login` and
//! friends): it has the client from the settings and the tokio runtime the
//! login needs, and restarts the YouTube sources afterwards. The panel just
//! awaits the results. The settings themselves live in the engine (read
//! through the shared `AppConfig`): the settings window changes the API key
//! too, so the panel keeps no copy of its own that could go stale.

use chat_engine::{EngineHandle, Health, YouTubeAccount, YouTubeSettings};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, IconName, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;

use crate::app_config::{AppConfig, edit_settings};
use crate::app_view::{light, report};
use crate::emoji_import;

const SETUP_GUIDE: &str = "https://isneeze.github.io/chat-aggregator/youtube-setup";
const EMOJI_GUIDE: &str = "https://isneeze.github.io/chat-aggregator/youtube-emoji";

/// The custom emoji export in use.
#[derive(Clone)]
enum Emoji {
    None,
    Loaded(usize),
    /// Set in the config, but unusable (moved, broken).
    Problem(String),
    Importing,
}

enum Login {
    /// Looking for a stored login (at start, or after new credentials).
    Checking,
    NotConnected {
        error: Option<String>,
    },
    /// Waiting for the user to finish in the browser. Dropping the task
    /// cancels the login: the GPUI task drops the engine's login, which
    /// stops its task and with it the local server waiting for Google.
    LoggingIn {
        _task: Task<()>,
    },
    /// `channel` is `None` if the name couldn't be fetched (e.g. offline).
    Connected {
        channel: Option<String>,
    },
}

impl From<YouTubeAccount> for Login {
    fn from(account: YouTubeAccount) -> Self {
        match account {
            YouTubeAccount::NotSetUp | YouTubeAccount::LoggedOut => {
                Login::NotConnected { error: None }
            }
            YouTubeAccount::LoggedIn { channel } => Login::Connected { channel },
        }
    }
}

pub struct YouTubePanel {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    login: Login,
    emoji: Emoji,
    /// Showing the client id/secret form.
    editing: bool,
    form_error: Option<String>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl YouTubePanel {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = config.read(cx).settings().youtube;
        let client_id =
            cx.new(|cx| InputState::new(window, cx).placeholder("….apps.googleusercontent.com"));
        // Masked: shown as dots, like a password.
        let client_secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("GOCSPX-…")
                .masked(true)
        });
        let set_up = settings.oauth_app().is_ok();
        let emoji = match &settings.emojis {
            None => Emoji::None,
            Some(path) => match emoji_import::count(path) {
                Ok(count) => Emoji::Loaded(count),
                Err(e) => Emoji::Problem(format!("{e:#}")),
            },
        };
        // New settings (from here or the settings window): redraw.
        let subscriptions = vec![cx.observe(&config, |_, _, cx| cx.notify())];
        let mut panel = Self {
            engine,
            config,
            login: Login::Checking,
            emoji,
            editing: !set_up,
            form_error: None,
            client_id,
            client_secret,
            _subscriptions: subscriptions,
        };
        if set_up {
            panel.check_login(cx);
        }
        panel
    }

    /// The current settings, a copy: they may change any time.
    fn settings(&self, cx: &App) -> YouTubeSettings {
        self.config.read(cx).settings().youtube
    }

    /// Asks the engine whose login is stored (with the client in the
    /// settings, including one saved a moment before).
    fn check_login(&mut self, cx: &mut Context<Self>) {
        self.login = Login::Checking;
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let login = match engine.youtube_account().await {
                Ok(account) => Login::from(account),
                Err(e) => Login::NotConnected {
                    error: Some(format!("{e:#}")),
                },
            };
            let _ = this.update(cx, |panel, cx| {
                panel.login = login;
                cx.notify();
            });
        })
        .detach();
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = match engine.youtube_login().await {
                Ok(login) => {
                    // First the consent page, in the browser...
                    let url = login.url().to_string();
                    let _ = this.update(cx, |_, cx| cx.open_url(&url));
                    // ...then however the login ends there. (The engine
                    // restarts the YouTube sources with it.)
                    login.finish().await
                }
                Err(e) => Err(e),
            };
            // Replacing `LoggingIn` drops this very task: fine, it's done.
            let _ = this.update_in(cx, |panel, window, cx| {
                if let Ok(YouTubeAccount::LoggedIn { .. }) = result {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::success(
                            "Connected to YouTube",
                        ),
                        cx,
                    );
                }
                panel.login = match result {
                    Ok(account) => Login::from(account),
                    Err(e) => Login::NotConnected {
                        error: Some(format!("{e:#}")),
                    },
                };
                cx.notify();
            });
        });
        self.login = Login::LoggingIn { _task: task };
        cx.notify();
    }

    /// Dropping the waiting login (with `LoggingIn`) cancels it.
    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        self.login = Login::NotConnected { error: None };
        cx.notify();
    }

    fn log_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.login = Login::NotConnected { error: None };
        cx.notify();
        // The YouTube sources notice the missing login (the engine restarts
        // them) and turn red.
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Err(e) = engine.youtube_logout().await {
                let _ = this.update_in(cx, |_, window, cx| {
                    report(window, cx, format!("Couldn't log out: {e:#}"));
                });
            }
        })
        .detach();
    }

    fn edit_client(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.settings(cx).client_id.unwrap_or_default();
        self.client_id
            .update(cx, |input, cx| input.set_value(id, window, cx));
        self.client_secret
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.editing = true;
        self.form_error = None;
        cx.notify();
    }

    /// Changes the YouTube settings (`change` edits them in the engine,
    /// which restarts the YouTube sources with them) and saves them.
    fn change_settings(
        &mut self,
        change: impl FnOnce(&mut YouTubeSettings) + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |settings| change(&mut settings.youtube),
            cx,
        );
    }

    fn choose_emoji_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose the emoji export".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = paths.await;
            let _ = this.update_in(cx, |panel, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        panel.import_emoji(path, window, cx);
                    }
                }
                Ok(Ok(None)) | Err(_) => {} // cancelled
                // On Linux the dialog goes through xdg-desktop-portal,
                // which needs a portal backend installed.
                Ok(Err(e)) => report(
                    window,
                    cx,
                    format!(
                        "Couldn't open the file dialog ({e}). You can also drag the file \
                         onto the YouTube section."
                    ),
                ),
            });
        })
        .detach();
    }

    fn import_emoji(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let before = std::mem::replace(&mut self.emoji, Emoji::Importing);
        cx.notify();
        let dir = self.config.read(cx).settings_dir();
        // File work off the UI thread, so the window never stutters.
        let task = cx
            .background_executor()
            .spawn(async move { emoji_import::import(&path, &dir) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |panel, window, cx| {
                match result {
                    Ok((copy, count)) => {
                        panel.emoji = Emoji::Loaded(count);
                        panel.use_emojis(copy, window, cx);
                        window.push_notification(
                            gpui_kit::component::notification::Notification::success(format!(
                                "{count} custom emoji loaded"
                            )),
                            cx,
                        );
                    }
                    Err(e) => {
                        // A bad file changes nothing: keep what was there.
                        panel.emoji = before;
                        report(window, cx, format!("That file can't be used: {e:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Hands the imported export to the engine, which loads it into the
    /// YouTube sources (restarting them), also when it replaced the
    /// previous export in the same file.
    fn use_emojis(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Err(e) = engine.use_youtube_emojis(path).await {
                let _ = this.update_in(cx, |_, window, cx| {
                    report(window, cx, format!("Couldn't use the emoji: {e:#}"));
                });
            }
        })
        .detach();
    }

    fn remove_emoji(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Our own copy in the settings folder; the original is untouched.
        // (Running sources have the emoji loaded already.)
        if let Some(path) = self.settings(cx).emojis {
            let _ = std::fs::remove_file(path);
        }
        self.change_settings(|settings| settings.emojis = None, window, cx);
        self.emoji = Emoji::None;
        cx.notify();
    }

    fn save_client(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.client_id.read(cx).value().trim().to_string();
        let secret = self.client_secret.read(cx).value().trim().to_string();
        if id.is_empty() || secret.is_empty() {
            self.form_error = Some("Both the client id and the client secret are needed.".into());
            cx.notify();
            return;
        }
        if !id.ends_with(".apps.googleusercontent.com") {
            self.form_error = Some(
                "That doesn't look like a client id (it ends in .apps.googleusercontent.com)."
                    .into(),
            );
            cx.notify();
            return;
        }
        self.editing = false;
        self.form_error = None;
        self.change_settings(
            |settings| {
                settings.client_id = Some(id);
                settings.client_secret = Some(secret);
            },
            window,
            cx,
        );
        // Asked after the change was sent: the engine answers with the new
        // client.
        self.check_login(cx);
    }

    /// Only offer "Cancel" if there's a working client to go back to.
    fn can_cancel_editing(&self, cx: &App) -> bool {
        self.settings(cx).oauth_app().is_ok()
    }

    // ---- drawing ----

    fn render_client_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let set_up = self.can_cancel_editing(cx);
        let label = |text: &'static str| {
            div()
                .w(px(110.))
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        v_flex()
            .gap_2()
            .when(!set_up, |this| {
                this.child(
                    div()
                        .text_color(cx.theme().warning)
                        .child("Not set up yet: YouTube needs your own (free) Google project."),
                )
            })
            .child(
                Button::new("youtube-guide")
                    .label("Open the setup guide")
                    .icon(IconName::ExternalLink)
                    .small()
                    .ghost()
                    .on_click(|_, _, cx| cx.open_url(SETUP_GUIDE)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(label("Client ID"))
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.client_id).id("youtube-client-id")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(label("Client secret"))
                    .child(
                        div().flex_1().child(
                            Input::new(&self.client_secret)
                                .id("youtube-client-secret")
                                .mask_toggle(),
                        ),
                    ),
            )
            .children(
                self.form_error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(set_up, |this| {
                        this.child(Button::new("youtube-cancel-edit").label("Cancel").on_click(
                            cx.listener(|panel, _, _, cx| {
                                panel.editing = false;
                                panel.form_error = None;
                                cx.notify();
                            }),
                        ))
                    })
                    .child(
                        Button::new("youtube-save")
                            .label("Save")
                            .primary()
                            .on_click(
                                cx.listener(|panel, _, window, cx| panel.save_client(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_login(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (health, text): (Health, String) = match &self.login {
            Login::Checking => (Health::Warning, "Checking the login…".into()),
            Login::NotConnected { .. } => (Health::Off, "Not connected".into()),
            Login::LoggingIn { .. } => (
                Health::Warning,
                "Waiting for the browser: finish the login there…".into(),
            ),
            Login::Connected {
                channel: Some(name),
            } => (Health::Ok, format!("Connected as \"{name}\"")),
            Login::Connected { channel: None } => (Health::Ok, "Connected".into()),
        };

        let mut buttons = h_flex().gap_2();
        buttons = match &self.login {
            Login::NotConnected { .. } => buttons.child(
                Button::new("youtube-connect")
                    .label("Connect YouTube")
                    .primary()
                    .on_click(cx.listener(|panel, _, window, cx| panel.connect(window, cx))),
            ),
            Login::LoggingIn { .. } => buttons.child(
                Button::new("youtube-cancel-login")
                    .label("Cancel")
                    .on_click(cx.listener(|panel, _, _, cx| panel.cancel_login(cx))),
            ),
            Login::Connected { .. } => buttons.child(
                Button::new("youtube-logout")
                    .label("Log out")
                    .on_click(cx.listener(|panel, _, window, cx| panel.log_out(window, cx))),
            ),
            Login::Checking => buttons,
        };
        if !matches!(self.login, Login::LoggingIn { .. }) {
            buttons = buttons.child(
                Button::new("youtube-change-client")
                    .label("Change client")
                    .ghost()
                    .on_click(cx.listener(|panel, _, window, cx| panel.edit_client(window, cx))),
            );
        }

        let error = match &self.login {
            Login::NotConnected { error: Some(e) } => Some(e.clone()),
            _ => None,
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(light(health, cx))
                    .child(div().flex_1().child(text))
                    .child(buttons),
            )
            .children(error.map(|e| div().text_sm().text_color(muted).child(e)))
            .into_any_element()
    }
}

impl YouTubePanel {
    fn render_emoji(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Learned ones need no file: say so, rather than "none".
        let none = if self.settings(cx).learn_emojis {
            "none imported (learned from chat)"
        } else {
            "none loaded"
        };
        let theme = cx.theme();
        let (text, color) = match &self.emoji {
            Emoji::None => (none.to_string(), theme.muted_foreground),
            Emoji::Loaded(count) => (format!("{count} loaded"), theme.foreground),
            Emoji::Problem(e) => (e.clone(), theme.danger),
            Emoji::Importing => ("loading…".to_string(), theme.muted_foreground),
        };
        let has_file = matches!(self.emoji, Emoji::Loaded(_) | Emoji::Problem(_));
        h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Custom emoji"),
            )
            .child(div().flex_1().text_color(color).child(text))
            .child(
                Button::new("youtube-emoji-choose")
                    .label(if has_file {
                        "Replace…"
                    } else {
                        "Choose file…"
                    })
                    .small()
                    .on_click(
                        cx.listener(|panel, _, window, cx| panel.choose_emoji_file(window, cx)),
                    ),
            )
            .when(has_file, |this| {
                this.child(
                    Button::new("youtube-emoji-remove")
                        .label("Remove")
                        .small()
                        .ghost()
                        .on_click(
                            cx.listener(|panel, _, window, cx| panel.remove_emoji(window, cx)),
                        ),
                )
            })
            .child(
                Button::new("youtube-emoji-help")
                    .label("How do I export them?")
                    .icon(IconName::ExternalLink)
                    .small()
                    .ghost()
                    .on_click(|_, _, cx| cx.open_url(EMOJI_GUIDE)),
            )
    }
}

impl Render for YouTubePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let drop_highlight = cx.theme().muted;
        v_flex()
            .id("youtube-panel")
            .gap_2()
            // Dropping an emoji export anywhere on the section loads it: a
            // fallback for systems where the file dialog doesn't open.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_highlight))
            .on_drop(cx.listener(|panel, paths: &ExternalPaths, window, cx| {
                if let Some(path) = paths.paths().first() {
                    panel.import_emoji(path.clone(), window, cx);
                }
            }))
            .child(div().font_semibold().child("YouTube"))
            .child(if self.editing {
                self.render_client_form(cx)
            } else {
                v_flex()
                    .gap_2()
                    .child(self.render_login(cx))
                    .child(self.render_emoji(cx))
                    .into_any_element()
            })
    }
}
