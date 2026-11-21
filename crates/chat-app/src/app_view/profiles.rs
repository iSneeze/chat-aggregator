//! The profile menu in the main window's header: switch to another
//! profile, make one (empty, or a copy of the active one), rename the
//! active one, delete another. The engine does the work
//! (`EngineHandle::switch_profile`, …); this asks the questions.
//!
//! A child module of `app_view`, so it can use `AppView`'s private fields:
//! in Rust, privacy ends at the module, and a child module counts as
//! inside its parent.

use chat_engine::{SourceState, normalize_profile_name};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::*;

use super::AppView;
use super::themes::{newest_label, theme_label};
use crate::ui::report;

/// The name field under the header, while it's open.
pub(super) struct ProfileForm {
    action: ProfileAction,
    input: Entity<InputState>,
    /// Why the name was refused.
    pub(super) error: Option<String>,
    /// Enter in the field submits it. Kept here: the subscription ends
    /// with the form.
    _enter: Subscription,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ProfileAction {
    /// A new profile: empty, or a copy of the active one.
    New { copy: bool },
    /// The active profile, renamed: its name now.
    Rename(String),
}

impl AppView {
    /// The header's profile button and its menu; nothing without profiles
    /// (an engine without a settings folder: tests).
    pub(super) fn render_profile_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.status.profiles.is_empty() {
            return None;
        }
        let active = self.status.profile.clone();
        let profiles = self.status.profiles.clone();
        let view = cx.entity().downgrade();
        let button = Button::new("profile-menu")
            .label(format!("Profile: {active}"))
            .small()
            .ghost()
            .dropdown_caret(true)
            .tooltip("Switch, add, rename or delete profiles");
        // The menu is built each time it opens (an `Fn`): what it uses is
        // cloned into it, not borrowed from the view.
        Some(
            button.dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                profile_menu(menu, &view, &active, &profiles, window, cx)
            }),
        )
    }

    /// The name form, while one is open.
    pub(super) fn render_profile_form(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.profile_form.as_ref()?;
        let (label, submit) = match &form.action {
            ProfileAction::New { copy: false } => ("New profile".to_string(), "Create"),
            ProfileAction::New { copy: true } => {
                (format!("Copy of \"{}\"", self.status.profile), "Create")
            }
            ProfileAction::Rename(name) => (format!("Rename \"{name}\""), "Rename"),
        };
        let row = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_color(cx.theme().muted_foreground).child(label))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&form.input).id("profile-name")),
            )
            .child(
                Button::new("profile-submit")
                    .label(submit)
                    .primary()
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.submit_profile_form(window, cx)),
                    ),
            )
            .child(
                Button::new("profile-cancel")
                    .label("Cancel")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.profile_form = None;
                        cx.notify();
                    })),
            );
        Some(
            v_flex().gap_1().child(row).children(
                form.error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            ),
        )
    }

    pub(super) fn open_profile_form(
        &mut self,
        action: ProfileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = match &action {
            ProfileAction::Rename(name) => name.clone(),
            ProfileAction::New { .. } => String::new(),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("name, e.g. Indie")
                .default_value(name)
        });
        let enter = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_profile_form(window, cx);
            }
        });
        self.profile_form = Some(ProfileForm {
            action,
            input,
            error: None,
            _enter: enter,
        });
        cx.notify();
    }

    /// Makes or renames the profile. A new one is switched to right away:
    /// it's made to be set up.
    pub(super) fn submit_profile_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.profile_form else {
            return;
        };
        let name = form.input.read(cx).value().to_string();
        // Checked here too, for an answer without a round trip; the engine
        // checks again (and knows which names are taken).
        if let Err(problem) = normalize_profile_name(&name) {
            form.error = Some(problem);
            cx.notify();
            return;
        }
        let action = form.action.clone();
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match &action {
                ProfileAction::New { copy } => engine.create_profile(&name, *copy).await.map(Some),
                ProfileAction::Rename(from) => {
                    engine.rename_profile(from, &name).await.map(|_| None)
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(created) => {
                        this.profile_form = None;
                        if let Some(name) = created {
                            this.ask_switch(name, window, cx);
                        }
                    }
                    Err(e) => {
                        if let Some(form) = &mut this.profile_form {
                            form.error = Some(format!("{e:#}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Switches to profile `name`, asking first if that would stop chat
    /// that's coming in, or drop unsaved changes in the Reactions window.
    pub(super) fn ask_switch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if name == self.status.profile {
            return;
        }
        // The test window's source isn't any profile's: it keeps running.
        let live = self.status.sources.iter().any(|source| {
            source.config.is_some() && matches!(source.state, SourceState::Running(_))
        });
        let unsaved = self.has_unsaved_side_windows(cx);
        if !live && !unsaved {
            self.switch_profile(name, window, cx);
            return;
        }
        let mut what = Vec::new();
        if live {
            what.push(
                "Its sources take over from the running ones, and the overlay's chat starts over.",
            );
        }
        if unsaved {
            what.push("Unsaved changes in the Reactions window are lost.");
        }
        self.open_confirm_dialog(
            window,
            cx,
            format!("Switch to \"{name}\"?"),
            what.join(" "),
            "Switch",
            move |this, window, cx| this.switch_profile(name.clone(), window, cx),
        );
    }

    fn switch_profile(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        // Edits still waiting for the typing pause in the other windows
        // were made in the profile being left: in before the switch.
        // (`edit_settings` sends a change right away; the switch below is
        // sent once its task runs, so after them.)
        self.flush_side_windows(cx);
        self.profile_form = None;
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Err(e) = engine.switch_profile(&name).await {
                let message = format!("Couldn't switch to the profile \"{name}\": {e:#}");
                let _ = this.update_in(cx, |_, window, cx| report(window, cx, message));
            }
        })
        .detach();
    }

    fn ask_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_confirm_dialog(
            window,
            cx,
            format!("Delete the profile \"{name}\"?"),
            "Its sources and settings are gone for good, and so are tokens and widget keys no \
             other profile uses.",
            "Delete",
            move |this, window, cx| this.delete_profile(name.clone(), window, cx),
        );
    }

    fn delete_profile(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.delete_profile(&name).await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => window.push_notification(
                    Notification::info(format!("Profile \"{name}\" deleted")),
                    cx,
                ),
                Err(e) => report(
                    window,
                    cx,
                    format!("Couldn't delete the profile \"{name}\": {e:#}"),
                ),
            });
        })
        .detach();
    }

    /// Another profile became the active one: the overlay pickers show its
    /// look. (The other windows load it themselves, see
    /// `AppConfig::switches`.)
    pub(super) fn profile_switched(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let overlay = self.config.read(cx).settings().profile.overlay;
        let theme = theme_label(overlay.theme.as_deref());
        let newest = newest_label(overlay.newest);
        self.theme_select.update(cx, |select, cx| {
            select.set_selected_value(&theme, window, cx);
        });
        self.newest_select.update(cx, |select, cx| {
            select.set_selected_value(&newest, window, cx);
        });
        self.new_theme = None;
        self.theme_error = None;
        self.rescan_themes(window, cx);
    }
}

/// The menu's entries: every profile (the active one ticked) to switch to,
/// then what can be done with them.
fn profile_menu(
    mut menu: PopupMenu,
    view: &WeakEntity<AppView>,
    active: &str,
    profiles: &[String],
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for name in profiles {
        let (view, target) = (view.clone(), name.clone());
        menu = menu.item(
            PopupMenuItem::new(name.clone())
                .checked(name == active)
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.ask_switch(target.clone(), window, cx));
                }),
        );
    }
    let form = |label: String, action: ProfileAction| {
        let view = view.clone();
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            let action = action.clone();
            let _ = view.update(cx, |this, cx| this.open_profile_form(action, window, cx));
        })
    };
    menu = menu
        .separator()
        .item(form(
            "New profile…".into(),
            ProfileAction::New { copy: false },
        ))
        .item(form(
            format!("Copy \"{active}\"…"),
            ProfileAction::New { copy: true },
        ))
        .item(form(
            format!("Rename \"{active}\"…"),
            ProfileAction::Rename(active.to_string()),
        ));
    // The active profile can't be deleted: the others can.
    let others: Vec<String> = profiles.iter().filter(|p| *p != active).cloned().collect();
    if !others.is_empty() {
        let view = view.clone();
        menu = menu.submenu("Delete", window, cx, move |mut menu, _, _| {
            for name in &others {
                let (view, target) = (view.clone(), name.clone());
                menu = menu.item(PopupMenuItem::new(format!("{name}…")).on_click(
                    move |_, window, cx| {
                        let _ =
                            view.update(cx, |this, cx| this.ask_delete(target.clone(), window, cx));
                    },
                ));
            }
            menu
        });
    }
    menu
}
