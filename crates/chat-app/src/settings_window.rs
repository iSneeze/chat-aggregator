//! The settings window (⚙): the app's look, the overlay's replay history,
//! burst pacing and what it shows, the tip currency and tiers, the port,
//! the JSON API and the web sites allowed to use it, the YouTube API key
//! and emoji learning, the settings folder.
//!
//! gpui-kit's settings fields report every keystroke. Applying each one
//! would go wrong: typing "25" over "20" passes through "2", which would
//! throw away 18 messages of replay history, and every letter of an API key
//! would restart YouTube. So edits go into a draft that is applied (engine
//! and `config.toml`) once typing has paused for a moment: a "debounce".
//! The look is the exception: it applies on the spot.

use std::time::Duration;

use chat_core::tiers::{Amount, TipSettings};
use chat_engine::{Appearance, DEFAULT_HISTORY, DEFAULT_PORT, EngineHandle, Hide, ServerSettings};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, IconName, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::app_view::report;
use crate::appearance;
use crate::paid;

/// How long typing must pause before edits apply.
const APPLY_AFTER: Duration = Duration::from_millis(800);

/// The dropdown's entries: the value saved in `config.toml`, and its label.
const APPEARANCES: [(Appearance, &str, &str); 4] = [
    (Appearance::System, "system", "System (light or dark)"),
    (Appearance::Light, "light", "Light"),
    (Appearance::Dark, "dark", "Dark"),
    (Appearance::HighContrast, "high-contrast", "High contrast"),
];

/// The overlay's platform switches: label, and what switching it off hides.
const SHOWN_PLATFORMS: [(&str, &[Hide]); 5] = [
    ("Twitch", &[Hide::Twitch]),
    ("YouTube", &[Hide::YouTube]),
    ("Picarto", &[Hide::Picarto]),
    ("StreamElements tips", &[Hide::StreamElements]),
    ("Streamlabs tips", &[Hide::Streamlabs]),
];

/// The overlay's message switches: label, description, and what switching
/// it off hides. "Chat" covers emote-only messages too: hiding chat but
/// showing a lone emote would be odd.
const SHOWN_KINDS: [(&str, &str, &[Hide]); 6] = [
    (
        "Chat",
        "Normal chat messages, also emote-only ones.",
        &[Hide::Text, Hide::EmoteOnly],
    ),
    ("Donations", "Bits, Super Chats, tips.", &[Hide::Donation]),
    (
        "Stickers and gifts",
        "YouTube Super Stickers and gifts.",
        &[Hide::Special],
    ),
    (
        "Memberships",
        "New subscribers and members, resubs, milestones.",
        &[Hide::Membership],
    ),
    ("Gifted subs", "Gifted subs and memberships.", &[Hide::Gift]),
    (
        "Notices",
        "Raids, announcements and other platform events.",
        &[Hide::Notice],
    ),
];

/// What this window edits, not yet applied. Only these settings: the
/// theme, the chat direction and the allowed sites change in other places
/// too, so this window never writes them (and so can't undo a change made
/// there while it was open).
#[derive(Clone, PartialEq)]
struct Draft {
    history: usize,
    stagger_ms: u64,
    stagger_max_ms: u64,
    port: u16,
    api: bool,
    api_key: String,
    learn_emojis: bool,
    /// What the overlay leaves out, sorted (so the file doesn't change
    /// with the order switches were flipped in).
    hide: Vec<Hide>,
    /// The tip currency and where tip tiers 2–7 start, as typed: checked
    /// together ([`Draft::tips`]), and only applied when they make sense.
    tip_currency: String,
    tip_tiers: [String; 6],
}

// The engine's settings by their full name: `Settings` alone is gpui-kit's
// settings page component here.
impl Draft {
    fn new(settings: &chat_engine::Settings) -> Self {
        let server = &settings.server;
        Self {
            history: server.history,
            stagger_ms: server.stagger_ms,
            stagger_max_ms: server.stagger_max_ms,
            port: server.port,
            api: server.api,
            api_key: settings.youtube.api_key.clone().unwrap_or_default(),
            learn_emojis: settings.youtube.learn_emojis,
            hide: server.hide.clone(),
            tip_currency: settings.tips.currency.clone(),
            tip_tiers: settings.tips.tiers.map(|amount| amount.to_string()),
        }
    }

    /// The tip currency as typed (in capitals), if it's one.
    fn tip_currency(&self) -> Result<String, String> {
        let currency = self.tip_currency.trim().to_ascii_uppercase();
        TipSettings::check_currency(&currency)?;
        Ok(currency)
    }

    /// Where tip tiers 2–7 start, as typed, if they make sense. A
    /// half-typed table ("1" on the way to "100") stays here instead of
    /// reaching the engine, which would refuse it anyway.
    fn tip_tiers(&self) -> Result<[Amount; 6], String> {
        let mut tiers = [Amount::default(); 6];
        for (i, (text, tier)) in self.tip_tiers.iter().zip(&mut tiers).enumerate() {
            *tier = text.parse().map_err(|e| format!("tier {}: {e}", i + 2))?;
        }
        TipSettings::check_tiers(&tiers)?;
        Ok(tiers)
    }

    /// The tip settings as typed, or the first thing wrong with them.
    fn tips(&self) -> Result<TipSettings, String> {
        Ok(TipSettings {
            currency: self.tip_currency()?,
            tiers: self.tip_tiers()?,
        })
    }

    /// Writes the draft into `settings`, leaving everything else as it is.
    fn write_to(self, settings: &mut chat_engine::Settings) {
        // Only what makes sense: the rest waits until it's fixed (the page
        // says what's wrong). First, while `self` is still whole: the lines
        // below move fields out of it.
        // Each on its own: a half-typed tier doesn't hold back the currency.
        if let Ok(currency) = self.tip_currency() {
            settings.tips.currency = currency;
        }
        if let Ok(tiers) = self.tip_tiers() {
            settings.tips.tiers = tiers;
        }
        let server = &mut settings.server;
        server.history = self.history;
        server.stagger_ms = self.stagger_ms;
        server.stagger_max_ms = self.stagger_max_ms;
        server.port = self.port;
        server.api = self.api;
        server.hide = self.hide;
        settings.youtube.api_key = Some(self.api_key).filter(|key| !key.is_empty());
        settings.youtube.learn_emojis = self.learn_emojis;
    }
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
    tip_currency: Entity<InputState>,
    /// Where tip tiers 2–7 start (tier 1 is everything below tier 2).
    tip_tiers: [Entity<InputState>; 6],
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = Draft::new(&config.read(cx).settings());
        // Our own input (not gpui-kit's settings text field) to mask it.
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional")
                .masked(true)
                .default_value(draft.api_key.clone())
        });
        let tip_currency = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. EUR")
                .default_value(draft.tip_currency.clone())
        });
        // `from_fn` builds the array from its index: one field per tier.
        let tip_tiers: [Entity<InputState>; 6] = std::array::from_fn(|i| {
            cx.new(|cx| InputState::new(window, cx).default_value(draft.tip_tiers[i].clone()))
        });
        let mut subscriptions = vec![
            cx.subscribe(&api_key, |this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let key = input.read(cx).value().trim().to_string();
                    this.change(|draft| draft.api_key = key, cx);
                }
            }),
            cx.subscribe(&tip_currency, |this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let code = input.read(cx).value().to_string();
                    this.change(|draft| draft.tip_currency = code, cx);
                }
            }),
            // The allowed sites change from the main window too ("Allow"
            // in its question): redraw the list when they do.
            cx.observe(&config, |_, _, cx| cx.notify()),
            // Closing the window within the pause mustn't lose the last
            // edit: apply it now. (Problems then only go to the log: there's
            // no window left to show them in.)
            cx.on_release(|this, cx| {
                if this.dirty {
                    this.apply(cx);
                }
            }),
        ];
        for (i, input) in tip_tiers.iter().enumerate() {
            // `move`: each closure keeps its own `i`.
            subscriptions.push(cx.subscribe(input, move |this, input, event, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.change(|draft| draft.tip_tiers[i] = text, cx);
                }
            }));
        }
        Self {
            engine,
            config,
            window: window.window_handle(),
            draft,
            pending: None,
            dirty: false,
            api_key,
            tip_currency,
            tip_tiers,
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
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPLY_AFTER).await;
            let _ = this.update(cx, |this, cx| this.apply(cx));
        }));
        cx.notify();
    }

    /// Hands the draft to the engine, which applies only what changed (e.g.
    /// YouTube restarts only for a new API key), and saves it. Takes `App`
    /// rather than `Context<Self>`: it's also called while the view is being
    /// released, when only `App` is available. (A `Context` works too: it
    /// derefs to `App`.)
    fn apply(&mut self, cx: &mut App) {
        self.dirty = false;
        let draft = self.draft.clone();
        edit_settings(
            &self.engine,
            self.window,
            move |settings| draft.write_to(settings),
            cx,
        );
    }

    fn set_api(&mut self, on: bool, cx: &mut Context<Self>) {
        self.apply_now(|draft| draft.api = on, cx);
    }

    fn set_learn_emojis(&mut self, on: bool, cx: &mut Context<Self>) {
        self.apply_now(|draft| draft.learn_emojis = on, cx);
    }

    /// Shows (`on`) or hides the overlay's messages of `items`.
    fn set_shown(&mut self, items: &[Hide], on: bool, cx: &mut Context<Self>) {
        self.apply_now(
            |draft| {
                draft.hide.retain(|hide| !items.contains(hide));
                if !on {
                    draft.hide.extend_from_slice(items);
                    draft.hide.sort();
                }
            },
            cx,
        );
    }

    /// For switches: applies right away (a switch has no half-typed states
    /// to wait out). Edits still waiting for the pause go along with it.
    fn apply_now(&mut self, edit: impl FnOnce(&mut Draft), cx: &mut Context<Self>) {
        edit(&mut self.draft);
        // Dropping the pending timer cancels it; `apply` does its work now.
        self.pending = None;
        self.apply(cx);
        cx.notify();
    }

    /// Forgets an allowed web site: its API connections end now, and it's
    /// asked about again the next time it connects.
    fn remove_api_origin(&mut self, origin: &str, cx: &mut Context<Self>) {
        let origin = origin.to_string();
        edit_settings(
            &self.engine,
            self.window,
            move |settings| settings.server.api_origins.retain(|o| *o != origin),
            cx,
        );
    }

    /// Puts the tip tiers back to YouTube's steps.
    fn reset_tip_tiers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let steps = TipSettings::default()
            .tiers
            .map(|amount| amount.to_string());
        for (input, step) in self.tip_tiers.iter().zip(&steps) {
            input.update(cx, |input, cx| input.set_value(step.clone(), window, cx));
        }
        self.change(|draft| draft.tip_tiers = steps, cx);
    }

    /// Changes the look right away, and saves it.
    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        edit_settings(
            &self.engine,
            self.window,
            move |settings| settings.app.appearance = appearance,
            cx,
        );
        // Shown at once rather than after the engine's answer. The settings
        // fields call this while their window is busy dispatching the
        // click; `defer` runs it right after, when the window is free to be
        // updated.
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                appearance::apply(appearance, Some(window), cx);
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
                        .map(|view| view.read(cx).config.read(cx).settings().app.appearance)
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
                move |cx| read.upgrade().is_some_and(|view| view.read(cx).draft.api),
                move |on, cx| {
                    if let Some(view) = write.upgrade() {
                        view.update(cx, |this, cx| this.set_api(on, cx));
                    }
                },
            )
        };
        let learn_emojis = {
            let (read, write) = (view.clone(), view.clone());
            SettingField::switch(
                move |cx| {
                    read.upgrade()
                        .is_some_and(|view| view.read(cx).draft.learn_emojis)
                },
                move |on, cx| {
                    if let Some(view) = write.upgrade() {
                        view.update(cx, |this, cx| this.set_learn_emojis(on, cx));
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
        // The allowed sites, each with a button to remove it. Read from the
        // config (not the draft) on every draw: "Allow" in the main window
        // adds to it while this window is open.
        let allowed_sites = {
            let view = view.clone();
            SettingField::render(move |_, _, cx| {
                let origins = view
                    .upgrade()
                    .map(|v| v.read(cx).config.read(cx).settings().server.api_origins)
                    .unwrap_or_default();
                let none = origins.is_empty();
                let rows = origins.into_iter().enumerate().map(|(ix, origin)| {
                    let view = view.clone();
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().child(origin.clone()))
                        .child(
                            Button::new(("settings-remove-origin", ix))
                                .label("Remove")
                                .small()
                                .on_click(move |_, _, cx| {
                                    if let Some(view) = view.upgrade() {
                                        view.update(cx, |this, cx| {
                                            this.remove_api_origin(&origin, cx);
                                        });
                                    }
                                }),
                        )
                });
                v_flex()
                    .gap_1()
                    .items_end()
                    .when(none, |list| {
                        list.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("None yet"),
                        )
                    })
                    .children(rows)
            })
        };
        let api_key = {
            let input = self.api_key.clone();
            SettingField::render(move |_, _, _| {
                div()
                    .w(px(240.))
                    .child(Input::new(&input).id("settings-api-key").mask_toggle())
            })
        };

        let tip_currency = {
            let input = self.tip_currency.clone();
            SettingField::render(move |_, _, _| {
                div()
                    .w(px(100.))
                    .child(Input::new(&input).id("settings-tip-currency"))
            })
        };
        let tier_table = {
            let view = view.clone();
            let inputs = self.tip_tiers.clone();
            SettingItem::render(move |_, _, cx| tier_table(&view, &inputs, cx))
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
            SettingPage::new("Overlay").groups([
                SettingGroup::new()
                    .description(
                        "Changes reach overlays when they connect or reload; \
                         \"Reload overlays\" in the main window applies them to all.",
                    )
                    .items([
                        SettingItem::new(
                            "Replay history",
                            number(&view, 0., 200., 1., |d| &mut d.history)
                                .default_value(DEFAULT_HISTORY as f64),
                        )
                        .description("Messages a (re)loaded overlay shows right away."),
                        SettingItem::new(
                            "Burst spacing (ms)",
                            number(&view, 0., 1000., 50., |d| &mut d.stagger_ms)
                                .default_value(ServerSettings::default().stagger_ms as f64),
                        )
                        .description(
                            "When many messages arrive at once, show them this far apart \
                             (0: off).",
                        ),
                        SettingItem::new(
                            "Longest delay (ms)",
                            number(&view, 0., 5000., 250., |d| &mut d.stagger_max_ms)
                                .default_value(ServerSettings::default().stagger_max_ms as f64),
                        )
                        .description("A message is never held back longer than this."),
                    ]),
                SettingGroup::new()
                    .title("Platforms shown")
                    .description(
                        "Switched off: that platform's messages stay out of the overlay. \
                         Chat reactions and the JSON API still get them. Applies to new \
                         messages right away; an overlay URL with ?hide=… chooses for \
                         itself.",
                    )
                    .items(SHOWN_PLATFORMS.iter().map(|(label, items)| {
                        SettingItem::new(*label, shown_switch(&view, items).default_value(true))
                    })),
                SettingGroup::new()
                    .title("Messages shown")
                    .description("The same, by kind of message, on every platform.")
                    .items(SHOWN_KINDS.iter().map(|(label, description, items)| {
                        SettingItem::new(*label, shown_switch(&view, items).default_value(true))
                            .description(*description)
                    })),
            ]),
            SettingPage::new("Paid messages").groups([
                SettingGroup::new().title("Tips").item(
                    SettingItem::new("Tip currency", tip_currency).description(
                        "Your tip page's currency (StreamElements, Streamlabs), as 3 letters \
                         (EUR, USD). Nothing is converted: StreamElements tips show in it, \
                         and amount rules count Super Chats in it too. A Streamlabs tip in \
                         another currency gets tier 1. Empty: StreamElements tips show as \
                         plain numbers.",
                    ),
                ),
                SettingGroup::new()
                    .title("Tiers")
                    .description(
                        "Every paid message gets a tier from 1 to 7, on one scale for all \
                         platforms: YouTube's Super Chat tiers. Twitch counts 100 bits as a \
                         dollar; tips start where you say, in your tip currency. Tiers \
                         colour donations in the overlay, and are what a reaction's \
                         \"tier 4 or more\" means.",
                    )
                    .item(tier_table),
            ]),
            SettingPage::new("Connection").group(
                SettingGroup::new().items([
                    SettingItem::new("JSON API", json_api.default_value(false)).description(
                        format!(
                            "Chat for your own programs (games, bots) at {api_url}. Off \
                             unless you need it. Programs, pages on this computer and OBS \
                             browser sources can always connect; a web site is asked about \
                             first."
                        ),
                    ),
                    SettingItem::new("Allowed web sites", allowed_sites).description(
                        "Sites (e.g. browser games) you allowed to read your chat through \
                         the JSON API. Removing one disconnects it; it's asked about again \
                         next time.",
                    ),
                    SettingItem::new(
                        "Port",
                        number(&view, 1024., 65535., 1., |d| &mut d.port)
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
                    SettingItem::new("Learn YouTube emoji", learn_emojis.default_value(true))
                        .description(
                            "Shows channel emoji as pictures once someone uses them, \
                             read from YouTube's own chat page (no login). Remembered \
                             per channel in the settings folder.",
                        ),
                ]),
            ),
        ]
    }
}

/// The tier table: tips editable (tiers 2–7), Twitch and YouTube for
/// comparison, and what's wrong with the tips if anything is.
fn tier_table(
    view: &WeakEntity<SettingsWindow>,
    inputs: &[Entity<InputState>; 6],
    cx: &mut App,
) -> impl IntoElement + use<> {
    let muted = cx.theme().muted_foreground;
    let draft = view.upgrade().map(|view| view.read(cx).draft.clone());
    let tips = draft.as_ref().map(Draft::tips);
    // The tips' column header says the currency, as typed.
    let currency = draft
        .as_ref()
        .map(|d| d.tip_currency.trim().to_ascii_uppercase())
        .filter(|code| !code.is_empty());
    let cell = |width: f32| div().w(px(width)).flex_none();
    let header = h_flex()
        .gap_3()
        .text_sm()
        .text_color(muted)
        .child(cell(40.).child("Tier"))
        .child(cell(150.).child(match &currency {
            Some(code) => format!("Tips ({code})"),
            None => "Tips".into(),
        }))
        .child(cell(120.).child("Twitch"))
        .child(cell(80.).child("YouTube"));
    let rows = paid::all().map(|tier| {
        let tips_cell = match tier.checked_sub(2) {
            // Tier 1 follows from tier 2.
            None => {
                let below = match &tips {
                    Some(Ok(tips)) => paid::tips_start(1, tips),
                    _ => "below tier 2".into(),
                };
                div().text_sm().child(below)
            }
            Some(i) => h_flex()
                .gap_1()
                .items_center()
                .child(
                    div().w(px(100.)).child(
                        Input::new(&inputs[i as usize])
                            .id(("settings-tip-tier", tier as usize))
                            .small(),
                    ),
                )
                .child(div().text_sm().child("+")),
        };
        h_flex()
            .gap_3()
            .items_center()
            .child(cell(40.).child(tier.to_string()))
            .child(cell(150.).child(tips_cell))
            .child(cell(120.).text_sm().child(paid::bits_start(tier)))
            .child(cell(80.).text_sm().child(paid::youtube_start(tier)))
    });
    let problem = match &tips {
        Some(Err(problem)) => Some(format!("Not applied yet: {problem}.")),
        _ => None,
    };
    let reset = {
        let view = view.clone();
        Button::new("settings-tip-tiers-reset")
            .label("YouTube's steps")
            .small()
            .on_click(move |_, window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |this, cx| this.reset_tip_tiers(window, cx));
                }
            })
    };
    v_flex()
        .w_full()
        .gap_2()
        .child(header)
        .children(rows)
        .when_some(problem, |table, problem| {
            table.child(div().text_sm().text_color(cx.theme().danger).child(problem))
        })
        .child(h_flex().child(reset))
}

/// A switch for the overlay's messages of `items`: on while none of them is
/// hidden.
fn shown_switch(view: &WeakEntity<SettingsWindow>, items: &'static [Hide]) -> SettingField<bool> {
    let (read, write) = (view.clone(), view.clone());
    SettingField::switch(
        move |cx| {
            read.upgrade().is_none_or(|view| {
                let hidden = &view.read(cx).draft.hide;
                !items.iter().any(|item| hidden.contains(item))
            })
        },
        move |on, cx| {
            if let Some(view) = write.upgrade() {
                view.update(cx, |this, cx| this.set_shown(items, on, cx));
            }
        },
    )
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
    use std::path::PathBuf;

    use chat_core::ChatEvent;
    use chat_engine::{Appearance, ConfigFile, Engine, EngineConfig, Hide};
    use gpui_kit::component::{Root, Theme};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use chat_core::tiers::{Amount, TipSettings};

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
        // New settings wake GPUI tasks from the engine's tokio threads
        // (see `manual_window::tests::run_until`).
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let path = std::env::temp_dir()
            .join(format!("chat-app-settings-{}-{name}", std::process::id()))
            .join("config.toml");
        let settings_dir = path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&settings_dir);
        let mut config = EngineConfig::default();
        config.settings.server.port = 0; // the OS picks a free one
        config.save_to = Some(path.clone());
        let engine = runtime.block_on(Engine::start(config)).unwrap();
        let handle = engine.handle();
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = AppConfig::new(&handle, settings_dir, cx);
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
                this.change(|draft| draft.history = value, cx);
            });
        }

        fn engines(&self) -> chat_engine::Settings {
            self.engine.handle().settings().borrow().clone()
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

    /// Lets GPUI tasks run until `done`: the engine answers from tokio's
    /// threads in real time (see `manual_window::tests::run_until`).
    fn run_until(cx: &mut TestAppContext, mut done: impl FnMut() -> bool) {
        cx.executor().allow_parking();
        for _ in 0..500 {
            cx.run_until_parked();
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out");
    }

    /// Types `text` into the tip field `input` (a tier, or the currency),
    /// replacing what's there, as the field's change event does.
    fn type_tip(ui: &Ui, cx: &mut TestAppContext, input: Option<usize>, text: &str) {
        let view = ui.view().clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            let field = view.read_with(cx, |this, _| match input {
                Some(i) => this.tip_tiers[i].clone(),
                None => this.tip_currency.clone(),
            });
            field.update(cx, |field, cx| field.set_value(text, window, cx));
        })
        .unwrap();
        // `set_value` doesn't report a change (only typing does): say it.
        let text = text.to_string();
        ui.view().update(cx, |this, cx| {
            this.change(
                |draft| match input {
                    Some(i) => draft.tip_tiers[i] = text,
                    None => draft.tip_currency = text,
                },
                cx,
            );
        });
    }

    #[gpui_kit::test]
    fn tip_settings_apply_once_they_make_sense(cx: &mut TestAppContext) {
        let ui = open(cx, "tips");
        // Tier 4 from 12: on the way there, "1" is below tier 3's 5.
        type_tip(&ui, cx, Some(2), "1");
        type_tip(&ui, cx, None, "eur");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, || ui.saved().is_some());
        let saved = ui.saved().unwrap().tips;
        assert_eq!(saved.currency, "EUR", "the currency, in capitals");
        assert_eq!(
            saved.tiers,
            TipSettings::default().tiers,
            "the half-typed tier waits"
        );
        let problem = ui.view().read_with(cx, |this, _| this.draft.tips());
        assert!(problem.is_err_and(|e| e.contains("above")));

        type_tip(&ui, cx, Some(2), "12");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, || {
            ui.saved()
                .is_some_and(|f| f.tips.tiers[2] == Amount::whole(12))
        });
        assert_eq!(ui.engines().tips.tier(Amount::whole(12)), 4);
        assert_eq!(ui.engines().tips.tier(Amount::of(11.99).unwrap()), 3);
    }

    #[gpui_kit::test]
    fn the_tier_table_resets_to_youtubes_steps(cx: &mut TestAppContext) {
        let ui = open(cx, "tips-reset");
        type_tip(&ui, cx, Some(5), "500");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, || {
            ui.saved()
                .is_some_and(|f| f.tips.tiers[5] == Amount::whole(500))
        });
        // What the table's button does (the table is on another page than
        // the one the window opens on).
        let view = ui.view().clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            view.update(cx, |this, cx| this.reset_tip_tiers(window, cx));
        })
        .unwrap();
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, || ui.saved().is_some_and(|f| f.tips.is_default()));
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
        run_until(cx, || ui.saved().is_some_and(|f| f.server.history == 25));
        assert_eq!(ui.engines().server.history, 25);
        assert_eq!(ui.replayed(), 20, "the \"2\" in between never applied");
        ui.fill_history(10);
        assert_eq!(ui.replayed(), 25, "the engine uses the new size");
    }

    #[gpui_kit::test]
    fn closing_the_window_applies_what_is_pending(cx: &mut TestAppContext) {
        let mut ui = open(cx, "close");
        ui.view().update(cx, |this, cx| {
            this.change(|draft| draft.stagger_ms = 0, cx);
        });
        // Nothing may hold the view any more for it to be released.
        ui.view = None;
        cx.update_window(ui.window.into(), |_, window, _| window.remove_window())
            .unwrap();
        run_until(cx, || ui.saved().is_some_and(|f| f.server.stagger_ms == 0));
        assert!(ui.engines().server.stagger().is_off());
    }

    /// The API key goes through `apply_youtube` like the YouTube section's
    /// settings, also when the window closes within the pause (and isn't
    /// there any more to show problems in).
    #[gpui_kit::test]
    fn a_pending_api_key_is_saved_when_the_window_closes(cx: &mut TestAppContext) {
        let mut ui = open(cx, "api-key");
        ui.view().update(cx, |this, cx| {
            this.change(|draft| draft.api_key = "test-key".into(), cx);
        });
        ui.view = None;
        cx.update_window(ui.window.into(), |_, window, _| window.remove_window())
            .unwrap();
        run_until(cx, || {
            ui.saved()
                .is_some_and(|f| f.youtube.api_key.as_deref() == Some("test-key"))
        });
        assert_eq!(ui.engines().youtube.api_key.as_deref(), Some("test-key"));
    }

    #[gpui_kit::test]
    fn the_json_api_switches_at_once_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, "api");
        assert!(!ui.engines().server.api, "off by default");
        // No pause to wait out: the switch applies on the spot.
        ui.view().update(cx, |this, cx| this.set_api(true, cx));
        run_until(cx, || ui.saved().is_some_and(|f| f.server.api));
        assert!(ui.engines().server.api);

        ui.view().update(cx, |this, cx| this.set_api(false, cx));
        run_until(cx, || ui.saved().is_some_and(|f| !f.server.api));
        assert!(!ui.engines().server.api);
    }

    #[gpui_kit::test]
    fn emoji_learning_switches_at_once_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, "learn-emojis");
        assert!(ui.engines().youtube.learn_emojis, "on by default");
        ui.view()
            .update(cx, |this, cx| this.set_learn_emojis(false, cx));
        run_until(cx, || ui.saved().is_some_and(|f| !f.youtube.learn_emojis));
        assert!(!ui.engines().youtube.learn_emojis);
    }

    #[gpui_kit::test]
    fn what_the_overlay_shows_applies_at_once_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, "hide");
        assert!(ui.engines().server.hide.is_empty(), "everything by default");
        ui.view().update(cx, |this, cx| {
            this.set_shown(&[Hide::StreamElements], false, cx)
        });
        run_until(cx, || {
            ui.saved()
                .is_some_and(|f| f.server.hide == [Hide::StreamElements])
        });
        // "Chat" hides emote-only messages too; kept sorted.
        ui.view().update(cx, |this, cx| {
            this.set_shown(&[Hide::Text, Hide::EmoteOnly], false, cx)
        });
        let all = [Hide::StreamElements, Hide::Text, Hide::EmoteOnly];
        run_until(cx, || ui.saved().is_some_and(|f| f.server.hide == all));
        assert_eq!(ui.engines().server.hide, all);

        // Back on.
        ui.view().update(cx, |this, cx| {
            this.set_shown(&[Hide::StreamElements], true, cx)
        });
        let chat = [Hide::Text, Hide::EmoteOnly];
        run_until(cx, || ui.saved().is_some_and(|f| f.server.hide == chat));
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
        run_until(cx, || {
            ui.saved()
                .is_some_and(|f| f.app.appearance == Appearance::HighContrast)
        });
    }

    #[gpui_kit::test]
    fn removing_an_allowed_site_is_saved(cx: &mut TestAppContext) {
        const GAME: &str = "https://game.example";
        let ui = open(cx, "origins");
        // "Allow" in the main window's question.
        let handle = ui.engine.handle();
        ui._runtime
            .block_on(handle.answer_api_request(GAME, true))
            .unwrap();
        assert_eq!(ui.engines().server.api_origins, [GAME]);

        ui.view()
            .update(cx, |this, cx| this.remove_api_origin(GAME, cx));
        run_until(cx, || {
            ui.saved().is_some_and(|f| f.server.api_origins.is_empty())
        });
        assert!(ui.engines().server.api_origins.is_empty());
    }

    /// Regression test: errors reported in this window must reach the
    /// screen (`Root` draws them only when `gpui_kit::init` ran first).
    #[gpui_kit::test]
    fn reported_errors_are_shown(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt;
        let ui = open(cx, "report");
        cx.update_window(ui.window.into(), |_, window, cx| {
            crate::app_view::report(window, cx, "something went wrong".into());
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("notification").is_some());
        })
        .unwrap();
    }
}
