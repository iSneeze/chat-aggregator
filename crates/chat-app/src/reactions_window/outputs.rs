//! Output connection cards (Warudo & OBS) in the Reactions window: URL,
//! queue wait, OBS keyring password, and save/check actions.

use chat_engine::{Health, MAX_QUEUE_SECS, ObsSettings, ReactionSettings, Target, WarudoSettings};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::ReactionsWindow;
use super::form::number;
use crate::app_config::edit_settings;
use crate::ui::{card, labelled, light};

/// The connection settings' fields, for both programs.
pub(super) struct OutputInputs {
    pub(super) warudo_url: Entity<InputState>,
    pub(super) warudo_queue: Entity<InputState>,
    pub(super) obs_url: Entity<InputState>,
    pub(super) obs_queue: Entity<InputState>,
    pub(super) obs_password: Entity<InputState>,
}

impl OutputInputs {
    pub(super) fn new(settings: &ReactionSettings, window: &mut Window, cx: &mut App) -> Self {
        let mut field = |value: String, placeholder: &'static str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
                    .default_value(value)
            })
        };
        Self {
            warudo_url: field(settings.warudo.url.clone(), "ws://127.0.0.1:19190", false),
            warudo_queue: field(settings.warudo.queue_secs.to_string(), "0", false),
            obs_url: field(settings.obs.url.clone(), "ws://127.0.0.1:4455", false),
            obs_queue: field(settings.obs.queue_secs.to_string(), "0", false),
            obs_password: field(String::new(), "from OBS: Show Connect Info", true),
        }
    }

    pub(super) fn fill(&self, settings: &ReactionSettings, window: &mut Window, cx: &mut App) {
        let fields = [
            (&self.warudo_url, settings.warudo.url.clone()),
            (&self.warudo_queue, settings.warudo.queue_secs.to_string()),
            (&self.obs_url, settings.obs.url.clone()),
            (&self.obs_queue, settings.obs.queue_secs.to_string()),
            (&self.obs_password, String::new()),
        ];
        for (input, value) in fields {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }
}

impl ReactionsWindow {
    pub(super) fn outputs_unsaved(&self, cx: &App) -> bool {
        let settings = self.config.read(cx).settings().profile.reactions;
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        // As a number: "030" is what's saved as 30.
        let queue = |input: &Entity<InputState>| number(&text(input), "").ok();
        let o = &self.outputs;
        text(&o.warudo_url) != settings.warudo.url
            || queue(&o.warudo_queue) != Some(settings.warudo.queue_secs)
            || text(&o.obs_url) != settings.obs.url
            || queue(&o.obs_queue) != Some(settings.obs.queue_secs)
            || !text(&o.obs_password).is_empty()
    }

    pub(super) fn save_output(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let o = &self.outputs;
        let (url, queue) = match target {
            Target::Warudo => (&o.warudo_url, &o.warudo_queue),
            Target::Obs => (&o.obs_url, &o.obs_queue),
        };
        let url = url.read(cx).value().trim().to_string();
        let queue = queue.read(cx).value().to_string();
        let checked = match (url.starts_with("ws://"), number(&queue, "The waiting time")) {
            (false, _) => Err("The address starts with ws://".to_string()),
            (_, Err(e)) => Err(e),
            (_, Ok(secs)) if secs > MAX_QUEUE_SECS => {
                Err(format!("Waiting can be at most {MAX_QUEUE_SECS} seconds."))
            }
            (_, Ok(secs)) => Ok(secs),
        };
        let error = match target {
            Target::Warudo => &mut self.warudo_error,
            Target::Obs => &mut self.obs_error,
        };
        let queue_secs = match checked {
            Ok(secs) => secs,
            Err(problem) => {
                *error = Some(problem);
                cx.notify();
                return;
            }
        };
        *error = None;
        cx.notify();
        let handle = window.window_handle();
        edit_settings(
            &self.engine,
            handle,
            move |s| match target {
                Target::Warudo => s.profile.reactions.warudo = WarudoSettings { url, queue_secs },
                Target::Obs => s.profile.reactions.obs = ObsSettings { url, queue_secs },
            },
            cx,
        );
    }

    pub(super) fn check_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let stored = engine.obs_password_stored().await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = &stored {
                    tracing::warn!("couldn't read the OBS password's state: {e:#}");
                }
                view.password_stored = stored.ok();
                cx.notify();
            });
        })
        .detach();
    }

    /// Stores the typed password (`store`), or removes the stored one.
    pub(super) fn set_password(
        &mut self,
        store: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let password = store.then(|| self.outputs.obs_password.read(cx).value().to_string());
        if password.as_deref() == Some("") {
            self.obs_error = Some("Type the password first.".into());
            cx.notify();
            return;
        }
        self.obs_error = None;
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.set_obs_password(password).await;
            let _ = this.update_in(cx, |view, window, cx| {
                match result {
                    Ok(()) => {
                        view.password_stored = Some(store);
                        // Out of the field once it's stored: the window
                        // keeps no copy.
                        view.outputs
                            .obs_password
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    Err(e) => view.obs_error = Some(format!("Couldn't store it: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_output(&self, target: Target, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let o = &self.outputs;
        let (url, queue, error, prefix) = match target {
            Target::Warudo => (&o.warudo_url, &o.warudo_queue, &self.warudo_error, "warudo"),
            Target::Obs => (&o.obs_url, &o.obs_queue, &self.obs_error, "obs"),
        };
        let output = self.status.outputs.iter().find(|o| o.target == target);
        let (health, summary) = match output {
            Some(output) => (Health::from(&output.state), output.summary()),
            None => (
                Health::Off,
                "not connected: no switched-on rule uses it".into(),
            ),
        };

        card(cx)
            .flex_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(light(health, cx))
                    .child(div().font_semibold().child(target.label()))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(muted)
                            .child(summary),
                    ),
            )
            .child(labelled(
                "Address",
                Input::new(url).id(format!("{prefix}-url")),
                cx,
            ))
            .child(labelled(
                "Wait",
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(64.))
                            .child(Input::new(queue).id(format!("{prefix}-queue"))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("s while it's away (0: drop)"),
                    ),
                cx,
            ))
            .when(target == Target::Obs, |card| {
                let stored = match self.password_stored {
                    Some(true) => "a password is stored",
                    Some(false) => "no password stored",
                    None => "…",
                };
                card.child(labelled(
                    "Password",
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&o.obs_password).id("obs-password")),
                        )
                        .child(
                            Button::new("obs-password-store")
                                .label("Store")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_password(true, window, cx)
                                })),
                        )
                        .child(
                            Button::new("obs-password-remove")
                                .label("Remove")
                                .small()
                                .disabled(self.password_stored != Some(true))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_password(false, window, cx)
                                })),
                        ),
                    cx,
                ))
                .child(div().text_sm().text_color(muted).child(format!(
                    "In your system's keyring, not the settings file: {stored}."
                )))
            })
            .children(
                error
                    .clone()
                    .map(|e| div().text_sm().text_color(danger).child(e)),
            )
            .child(
                Button::new(format!("{prefix}-save"))
                    .label("Save")
                    .primary()
                    .small()
                    .self_start()
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.save_output(target, window, cx)
                        }),
                    ),
            )
    }
}
