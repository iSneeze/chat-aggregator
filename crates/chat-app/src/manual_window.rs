//! The test window: compose chat events by hand and send them through a
//! manual source, e.g. `!jump` for a game integration, or a fake donation
//! to see how the overlay shows it.
//!
//! The manual source exists exactly as long as this window: it's added
//! when the window opens and removed when the view is released (window
//! closed).

use chat_core::tiers::{self, Amount, TipSettings};
use chat_core::{Author, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use chat_engine::{EngineHandle, ManualInput, SourceId};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::*;

use crate::ui::{labelled, report};

/// What the window sends. The fields shown below the choice depend on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Message,
    Donation,
    Membership,
    Gift,
    Notice,
}

impl Kind {
    const ALL: [Kind; 5] = [
        Kind::Message,
        Kind::Donation,
        Kind::Membership,
        Kind::Gift,
        Kind::Notice,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Message => "Message",
            Kind::Donation => "Donation",
            Kind::Membership => "Membership",
            Kind::Gift => "Gift",
            Kind::Notice => "Notice",
        }
    }
}

/// Who the test author is. Sent as the badges the platform itself would
/// send, so themes treat test messages like real ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Viewer,
    Member,
    Moderator,
    Owner,
}

impl Role {
    const ALL: [Role; 4] = [Role::Viewer, Role::Member, Role::Moderator, Role::Owner];

    fn label(self) -> &'static str {
        match self {
            Role::Viewer => "Viewer",
            Role::Member => "Member",
            Role::Moderator => "Mod",
            Role::Owner => "Owner",
        }
    }

    /// The badge names chat-twitch and chat-youtube produce for this role.
    fn badges(self, platform: ChatPlatform) -> Vec<String> {
        // A tip page knows no roles.
        if matches!(
            platform,
            ChatPlatform::StreamElements | ChatPlatform::Streamlabs
        ) {
            return Vec::new();
        }
        let youtube = platform == ChatPlatform::YouTube;
        let badge = match self {
            Role::Viewer => return Vec::new(),
            Role::Member if youtube => "member",
            Role::Member => "subscriber",
            Role::Moderator => "moderator",
            Role::Owner if youtube => "owner",
            Role::Owner => "broadcaster",
        };
        vec![badge.into()]
    }
}

/// The platforms test messages can come from.
const PLATFORMS: [(ChatPlatform, &str); 4] = [
    (ChatPlatform::Twitch, "Twitch"),
    (ChatPlatform::YouTube, "YouTube"),
    (ChatPlatform::StreamElements, "StreamElements"),
    (ChatPlatform::Streamlabs, "Streamlabs"),
];

/// A test donation of `amount` on `platform`, as that platform's source
/// would send it. The tier comes from the amount, on the scale every
/// platform shares (`chat_core::tiers`).
fn donation(
    platform: ChatPlatform,
    amount: Amount,
    tips: &TipSettings,
) -> Result<MessageKind, String> {
    Ok(match platform {
        ChatPlatform::Twitch => {
            let bits = amount.as_f64();
            if bits.fract() != 0.0 {
                return Err("bits are a whole number".into());
            }
            // Whole and at most 1e13 (see `Amount::of`): the cast is exact.
            let bits = bits as u64;
            MessageKind::Donation {
                amount: format!("{bits} bits"),
                tier: Some(tiers::bits_tier(bits)),
                value: Some(bits as f64),
                currency: None,
            }
        }
        // In the tip currency if one is set, so amount rules can be tried
        // with Super Chats too; otherwise in dollars, YouTube's tier unit.
        ChatPlatform::YouTube => {
            let currency = tips.currency().unwrap_or("USD");
            MessageKind::Donation {
                amount: tiers::display_amount(amount, Some(currency)),
                tier: Some(tiers::youtube_tier(amount)),
                value: Some(amount.as_f64()),
                currency: Some(currency.to_owned()),
            }
        }
        // The tip services: exactly what their sources make of a tip.
        _ => tips.donation(amount, None),
    })
}

/// What the amount is in, next to its field.
fn unit(platform: ChatPlatform, tips: &TipSettings) -> String {
    match platform {
        ChatPlatform::Twitch => "bits".into(),
        ChatPlatform::YouTube => tips.currency().unwrap_or("USD").into(),
        _ => tips
            .currency()
            .map_or("in your tip currency (none set)".into(), str::to_owned),
    }
}

pub struct ManualWindow {
    engine: EngineHandle,
    /// Set once the engine has added the manual source.
    source: Option<(SourceId, ManualInput)>,
    platform: ChatPlatform,
    kind: Kind,
    role: Role,
    /// Donations only: the tier from the amount, or none (to try a theme's
    /// look without a tier, like a YouTube jewel gift's).
    tiered: bool,
    author: Entity<InputState>,
    text: Entity<InputState>,
    amount: Entity<InputState>,
    count: Entity<InputState>,
    info: Entity<InputState>,
    error: Option<String>,
    sent: u64,
    /// Platform and id of the last sent message, so "Delete last" targets the
    /// right platform even after switching the platform radio button.
    last_sent: Option<(ChatPlatform, String)>,
    _subscriptions: Vec<Subscription>,
}

impl ManualWindow {
    pub fn new(engine: EngineHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str,
                     value: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let author = input("author name", "Tester", window, cx);
        let text = input("message, e.g. !jump", "", window, cx);
        let amount = input("a number, e.g. 5 or 2.50", "5", window, cx);
        let count = input("number of gifts", "5", window, cx);
        let info = input("description, e.g. subscribed for 3 months", "", window, cx);

        let subscriptions = vec![
            // Enter in the message field sends, for quick `!jump !jump !up`.
            cx.subscribe_in(&text, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.send(window, cx);
                }
            }),
            // The window closed (view released): remove the manual source.
            cx.on_release(|this, cx| {
                if let Some((id, _)) = this.source.take() {
                    let engine = this.engine.clone();
                    cx.spawn(async move |_| {
                        let _ = engine.remove_source(id).await;
                    })
                    .detach();
                }
            }),
        ];

        let add = engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = add.add_manual_source().await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(source) => {
                    view.source = Some(source);
                    cx.notify();
                }
                Err(e) => report(window, cx, format!("Couldn't add the test source: {e:#}")),
            });
        })
        .detach();

        Self {
            engine,
            source: None,
            platform: ChatPlatform::Twitch,
            kind: Kind::Message,
            role: Role::Viewer,
            tiered: true,
            author,
            text,
            amount,
            count,
            info,
            error: None,
            sent: 0,
            last_sent: None,
            _subscriptions: subscriptions,
        }
    }

    /// The event the form describes right now, or what's wrong with it.
    fn compose(&self, cx: &App) -> Result<ChatMessage, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let text = value(&self.text);
        let kind = match self.kind {
            Kind::Message if text.is_empty() => return Err("type a message".into()),
            Kind::Message => MessageKind::Text,
            Kind::Donation => {
                let amount: Amount = value(&self.amount)
                    .parse()
                    .map_err(|e| format!("the amount: {e}"))?;
                let tips = self.engine.settings().borrow().profile.tips.clone();
                let mut kind = donation(self.platform, amount, &tips)?;
                // "none": a theme's look without a tier (like a YouTube
                // jewel gift's).
                if !self.tiered
                    && let MessageKind::Donation { tier, .. } = &mut kind
                {
                    *tier = None;
                }
                kind
            }
            Kind::Membership => MessageKind::MembershipJoin {
                info: non_empty(value(&self.info), "a description")?,
                months: None,
            },
            Kind::Gift => MessageKind::MembershipGift {
                count: value(&self.count)
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("the number of gifts must be a whole number above 0")?,
            },
            Kind::Notice => MessageKind::SystemNotice {
                info: non_empty(value(&self.info), "a description")?,
            },
        };
        let name = non_empty(value(&self.author), "an author name")?;
        Ok(ChatMessage {
            id: format!("manual-{}", self.sent + 1),
            platform: self.platform,
            author: Author {
                id: format!("manual-{}", name.to_lowercase()),
                name,
                color: None,
                badges: self.role.badges(self.platform),
                badge_urls: std::collections::BTreeMap::new(),
                avatar_url: None,
            },
            // Donations and memberships can come with a message too.
            text: if matches!(self.kind, Kind::Gift | Kind::Notice) {
                String::new()
            } else {
                text
            },
            emotes: Vec::new(),
            timestamp: jiff::Timestamp::now(),
            kind,
        })
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.source.is_none() {
            return; // not added yet; the button is disabled meanwhile
        }
        let message = match self.compose(cx) {
            Ok(message) => message,
            Err(problem) => {
                self.error = Some(problem);
                cx.notify();
                return;
            }
        };
        self.error = None;
        self.sent += 1;
        self.last_sent = Some((message.platform, message.id.clone()));
        cx.notify();
        self.send_event(ChatEvent::Message(message), window, cx);
    }

    /// Deletes the last sent message, to try out moderation in the overlay.
    fn delete_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((platform, message_id)) = self.last_sent.take() {
            let event = ChatEvent::Delete {
                platform,
                message_id,
            };
            self.send_event(event, window, cx);
            cx.notify();
        }
    }

    fn send_event(&self, event: ChatEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, input)) = self.source.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            if let Err(e) = input.send(event).await {
                let _ = this.update_in(cx, |_, window, cx| report(window, cx, format!("{e:#}")));
            }
        })
        .detach();
    }

    fn field(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        id: &'static str,
        cx: &App,
    ) -> impl IntoElement + use<> {
        labelled(label, Input::new(input).id(id), cx)
    }

    fn role_field(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let roles = Role::ALL
            .into_iter()
            .enumerate()
            .map(|(i, role)| Radio::new(i).label(role.label()));
        let group = RadioGroup::horizontal("manual-role")
            .children(roles)
            .selected_index(Role::ALL.iter().position(|r| *r == self.role))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.role = Role::ALL[*index];
                cx.notify();
            }));
        labelled("Role", group, cx)
    }

    /// The amount, with what it's in next to it.
    fn amount_field(&self, cx: &App) -> impl IntoElement + use<> {
        let tips = self.engine.settings().borrow().profile.tips.clone();
        let control = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.amount).id("manual-amount")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(unit(self.platform, &tips)),
            );
        labelled("Amount", control, cx)
    }

    /// Donation tier: from the amount (shown), or none.
    fn tier_field(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let from_amount = match self.compose(cx).map(|m| m.kind) {
            Ok(MessageKind::Donation {
                tier: Some(tier), ..
            }) => {
                format!("from the amount: tier {tier}")
            }
            _ => "from the amount".into(),
        };
        let tiers = RadioGroup::horizontal("manual-tier")
            .child(Radio::new(0).label(from_amount))
            .child(Radio::new(1).label("none"))
            .selected_index(Some(if self.tiered { 0 } else { 1 }))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.tiered = *index == 0;
                cx.notify();
            }));
        labelled("Tier", tiers, cx)
    }
}

impl Render for ManualWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let platforms = PLATFORMS
            .into_iter()
            .enumerate()
            .fold(
                RadioGroup::horizontal("manual-platform"),
                |group, (i, (_, label))| group.child(Radio::new(i).label(label)),
            )
            .selected_index(PLATFORMS.iter().position(|(p, _)| *p == self.platform))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.platform = PLATFORMS[*index].0;
                cx.notify();
            }));

        // The group numbers its radios itself (0, 1, …, inside the group's
        // id), which is also how the tests find them.
        let kinds = Kind::ALL
            .into_iter()
            .enumerate()
            .fold(RadioGroup::horizontal("manual-kind"), |group, (i, kind)| {
                group.child(Radio::new(i).label(kind.label()))
            })
            .selected_index(Kind::ALL.iter().position(|k| *k == self.kind))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.kind = Kind::ALL[*index];
                this.error = None;
                cx.notify();
            }));

        // The fields this kind needs.
        let mut fields = v_flex()
            .gap_2()
            .child(self.field("Author", &self.author, "manual-author", cx))
            .child(self.role_field(cx));
        fields = match self.kind {
            Kind::Message => fields.child(self.field("Message", &self.text, "manual-text", cx)),
            Kind::Donation => fields
                .child(self.amount_field(cx))
                .child(self.tier_field(cx))
                .child(self.field("Message", &self.text, "manual-text", cx)),
            Kind::Membership => fields
                .child(self.field("Description", &self.info, "manual-info", cx))
                .child(self.field("Message", &self.text, "manual-text", cx)),
            Kind::Gift => fields.child(self.field("Gifts", &self.count, "manual-count", cx)),
            Kind::Notice => fields.child(self.field("Description", &self.info, "manual-info", cx)),
        };

        let ready = self.source.is_some();
        v_flex()
            .size_full()
            .p_4()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_lg().font_semibold().child("Test messages"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Sent like real chat: to the overlay and the JSON API. \
                         Closing this window removes the test source.",
                    ),
            )
            .child(v_flex().gap_2().child(platforms).child(kinds))
            .child(fields)
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("manual-send")
                            .label("Send")
                            .primary()
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
                    .child(
                        Button::new("manual-delete")
                            .label("Delete last")
                            .small()
                            .disabled(self.last_sent.is_none())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.delete_last(window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} sent", self.sent)),
                    ),
            )
    }
}

fn non_empty(value: String, what: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("enter {what}"))
    } else {
        Ok(value)
    }
}

/// UI tests, headless like the main window's (see `app_view::tests`).
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chat_core::{ChatEvent, MessageKind};
    use chat_engine::{Engine, EngineConfig};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use super::{Kind, ManualWindow, Role};

    struct Ui {
        window: WindowHandle<Root>,
        view: Entity<ManualWindow>,
        engine: Engine,
        runtime: tokio::runtime::Runtime,
    }

    fn open(cx: &mut TestAppContext) -> Ui {
        cx.update(gpui_kit::init);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut config = EngineConfig::default();
        config.settings.server.port = 0; // the OS picks a free one
        let engine = runtime.block_on(Engine::start(config)).unwrap();
        let handle = engine.handle();
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let manual = cx.new(|cx| ManualWindow::new(handle, window, cx));
            view = Some(manual.clone());
            Root::new(manual, window, cx)
        });
        Ui {
            window,
            view: view.expect("the window was built"),
            engine,
            runtime,
        }
    }

    /// Lets GPUI tasks run until `done`. The engine answers from tokio's
    /// threads in real time, so the test executor must be allowed to wait
    /// for them (`allow_parking`) instead of assuming nothing else happens.
    fn run_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        cx.executor().allow_parking();
        for _ in 0..500 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    /// Replaces the amount (typing would add to the "5" it starts with).
    fn set_amount(cx: &mut TestAppContext, ui: &Ui, amount: &str) {
        let view = ui.view.clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            let input = view.read(cx).amount.clone();
            input.update(cx, |input, cx| input.set_value(amount, window, cx));
        })
        .unwrap();
    }

    fn pick_kind(window: &mut gpui_kit::Window, kind: Kind, cx: &mut gpui_kit::App) {
        let index = Kind::ALL.iter().position(|k| *k == kind).unwrap();
        window.within("manual-kind").click(index, cx);
    }

    #[gpui_kit::test]
    fn role_becomes_the_platforms_badges(cx: &mut TestAppContext) {
        let ui = open(cx);
        let badges = |cx: &mut TestAppContext| {
            ui.view.read_with(cx, |view, cx| {
                view.compose(cx)
                    .map(|m| m.author.badges)
                    .unwrap_or_default()
            })
        };
        let pick = |cx: &mut TestAppContext, role: Role, platform: usize| {
            let index = Role::ALL.iter().position(|r| *r == role).unwrap();
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.within("manual-platform").click(platform, cx);
                window.within("manual-role").click(index, cx);
            })
            .unwrap();
        };
        // A message needs text before it composes.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("manual-text", cx);
            window.input("hi", cx);
        })
        .unwrap();

        let none: Vec<String> = Vec::new();
        assert_eq!(badges(cx), none, "a viewer has no badges");
        pick(cx, Role::Member, 0);
        assert_eq!(badges(cx), ["subscriber"]);
        pick(cx, Role::Member, 1);
        assert_eq!(badges(cx), ["member"]);
        pick(cx, Role::Moderator, 1);
        assert_eq!(badges(cx), ["moderator"]);
        pick(cx, Role::Owner, 0);
        assert_eq!(badges(cx), ["broadcaster"]);
        pick(cx, Role::Owner, 1);
        assert_eq!(badges(cx), ["owner"]);
    }

    #[gpui_kit::test]
    fn the_tier_follows_the_amount_on_each_platform(cx: &mut TestAppContext) {
        let ui = open(cx);
        let donation = |cx: &mut TestAppContext| {
            ui.view.read_with(cx, |view, cx| match view.compose(cx) {
                Ok(m) => match m.kind {
                    MessageKind::Donation {
                        amount,
                        tier,
                        value,
                        currency,
                    } => (amount, tier, value, currency),
                    other => panic!("expected a donation, got {other:?}"),
                },
                Err(e) => panic!("{e}"),
            })
        };
        let set = |cx: &mut TestAppContext, platform: usize, amount: &str| {
            set_amount(cx, &ui, amount);
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.within("manual-platform").click(platform, cx);
            })
            .unwrap();
        };
        cx.update_window(ui.window.into(), |_, window, cx| {
            pick_kind(window, Kind::Donation, cx);
        })
        .unwrap();

        // Twitch: bits, on YouTube's dollar steps × 100.
        set(cx, 0, "500");
        assert_eq!(
            donation(cx),
            ("500 bits".into(), Some(3), Some(500.0), None)
        );
        // YouTube without a tip currency: dollars.
        set(cx, 1, "100");
        assert_eq!(
            donation(cx),
            ("$100.00".into(), Some(7), Some(100.0), Some("USD".into()))
        );
        // StreamElements: the streamer's tiers (here the defaults).
        set(cx, 2, "69");
        assert_eq!(donation(cx), ("69".into(), Some(6), Some(69.0), None));

        cx.update_window(ui.window.into(), |_, window, cx| {
            window.within("manual-tier").click(1, cx); // none
        })
        .unwrap();
        assert_eq!(donation(cx).1, None);
    }

    #[gpui_kit::test]
    fn sends_through_the_engine_and_takes_the_source_along_when_closed(cx: &mut TestAppContext) {
        let ui = open(cx);
        let (_, mut hub) = ui.engine.hub().subscribe();
        run_until(cx, |cx| {
            ui.view.read_with(cx, |view, _| view.source.is_some())
        });
        let (id, _) = ui
            .view
            .read_with(cx, |view, _| view.source.clone().unwrap());
        assert!(ui.engine.handle().status().borrow().source(id).is_some());

        set_amount(cx, &ui, "200"); // Twitch: 200 bits, tier 2
        cx.update_window(ui.window.into(), |_, window, cx| {
            pick_kind(window, Kind::Donation, cx);
            window.click("manual-text", cx);
            window.input("keep it up", cx);
            window.click("manual-send", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let event = ui
            .runtime
            // `async`: the timer must be created inside the runtime.
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), hub.recv()).await })
            .expect("the message arrives")
            .unwrap();
        match event {
            ChatEvent::Message(m) => {
                assert_eq!(m.text, "keep it up");
                assert_eq!(m.author.name, "Tester");
                assert!(
                    matches!(m.kind, MessageKind::Donation { amount, tier: Some(2), .. } if amount == "200 bits")
                );
            }
            other => panic!("expected the donation, got {other:?}"),
        }

        // Closing the window removes the test source from the engine. The
        // view is only released when nothing holds it any more, so the
        // test lets go of its own handle first.
        drop(ui.view);
        cx.update_window(ui.window.into(), |_, window, _| window.remove_window())
            .unwrap();
        let status = ui.engine.handle().status();
        run_until(cx, |_| status.borrow().source(id).is_none());
    }
}
