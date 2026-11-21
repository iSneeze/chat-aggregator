//! Web sites asking to connect to the JSON API, and coordination with other
//! modal dialogs in the main window so an API prompt (or its cancellation)
//! never clobbers a profile confirmation dialog.

use gpui_kit::component::WindowExt;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::*;

use super::AppView;
use crate::ui::confirm_with_cancel;

/// Which modal dialog the main window currently owns.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum ActiveDialog {
    #[default]
    None,
    /// Waiting for the streamer to Allow or Deny this web site origin.
    ApiApproval(String),
    /// A profile switch/delete confirmation dialog is currently open.
    Confirm,
}

impl AppView {
    /// Opens a yes/no confirmation dialog coordinated with `active_dialog`.
    /// When the dialog closes, any waiting API approval prompt is checked.
    pub(super) fn open_confirm_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        title: String,
        description: impl Into<SharedString>,
        ok: &'static str,
        on_ok: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.active_dialog = ActiveDialog::Confirm;
        let view_ok = cx.entity().downgrade();
        let view_cancel = view_ok.clone();
        confirm_with_cancel(
            window,
            cx,
            title,
            description,
            ok,
            move |window, cx| {
                let _ = view_ok.update(cx, |this, cx| {
                    this.active_dialog = ActiveDialog::None;
                    on_ok(this, window, cx);
                    this.ask_about_api_requests(window, cx);
                });
            },
            move |window, cx| {
                let _ = view_cancel.update(cx, |this, cx| {
                    if this.active_dialog == ActiveDialog::Confirm {
                        this.active_dialog = ActiveDialog::None;
                        this.ask_about_api_requests(window, cx);
                    }
                });
            },
        );
    }

    /// Asks about the first web site waiting to use the JSON API (one
    /// question at a time), and takes the question back once that site
    /// stopped waiting: answered, gave up, or the API was switched off.
    pub(super) fn ask_about_api_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let requests = &self.status.api_requests;
        // A site answered here has left the status by now, or soon will.
        self.answered.retain(|origin| requests.contains(origin));
        if let ActiveDialog::ApiApproval(origin) = &self.active_dialog
            && !requests.contains(origin)
        {
            self.active_dialog = ActiveDialog::None;
            window.close_dialog(cx);
        }
        // Never open over an existing API prompt or profile confirmation dialog.
        if self.active_dialog != ActiveDialog::None {
            return;
        }
        let Some(origin) = requests
            .iter()
            .find(|origin| !self.answered.contains(origin))
            .cloned()
        else {
            return;
        };
        self.active_dialog = ActiveDialog::ApiApproval(origin.clone());
        // Streamers often keep the app in the background: bring it up, or
        // the game waits for an answer nobody sees.
        window.activate_window();
        let view = cx.entity().downgrade();
        // The builder may run again (e.g. on every redraw), so it can only
        // hand out clones of what it captured: it's an `Fn`, not `FnOnce`.
        window.open_alert_dialog(cx, move |alert, _, _| {
            let answer = |allow: bool| {
                let (view, origin) = (view.clone(), origin.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            view.answer_api_request(&origin, allow, cx);
                        });
                    }
                    true // close the question
                }
            };
            alert
                .title(format!("Let {origin} read your chat?"))
                .description(
                    "It wants to use chat-aggregator's JSON API, like a browser game does. \
                     It would see every chat message, including members-only chat, and could \
                     tell which channel is yours. Allow only sites you trust; you can remove \
                     it later in ⚙ Settings → Connection.",
                )
                // First: `show_cancel`, `on_ok` and `on_cancel` are stored in
                // the button props, which this call replaces.
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Allow")
                        .cancel_text("Deny"),
                )
                .show_cancel(true)
                // Only the two buttons answer: no Escape, no close button.
                .keyboard(false)
                .close_button(false)
                .on_ok(answer(true))
                .on_cancel(answer(false))
        });
    }

    /// The streamer's answer about `origin`: the engine lets it in (or not)
    /// right away; "Allow" also goes into the settings (saved), "Deny" lasts
    /// until restart.
    fn answer_api_request(&mut self, origin: &str, allow: bool, cx: &mut Context<Self>) {
        if matches!(&self.active_dialog, ActiveDialog::ApiApproval(o) if o == origin) {
            self.active_dialog = ActiveDialog::None;
        }
        self.answered.push(origin.to_string());
        let (engine, origin) = (self.engine.clone(), origin.to_string());
        cx.spawn(async move |_, _| {
            if let Err(e) = engine.answer_api_request(&origin, allow).await {
                tracing::warn!("couldn't pass on the answer: {e:#}");
            }
        })
        .detach();
        cx.notify();
    }
}
