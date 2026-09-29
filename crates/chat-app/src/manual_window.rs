//! The test window: compose chat events by hand and send them through a
//! manual source, e.g. `!jump` for a game integration, or a fake donation
//! to see how the overlay shows it.
//!
//! The manual source exists exactly as long as this window: it's added
//! when the window opens and removed when the view is released (window
//! closed).

use chat_core::{Author, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use chat_engine::{EngineHandle, ManualInput, SourceId};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::*;

use crate::app_view::report;

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

/// The donation tiers the window offers, labelled with where each one
/// starts on the platform's scale (see `MessageKind::Special`'s `tier`):
/// tier 1 is the first entry.
fn tier_steps(platform: ChatPlatform) -> &'static [&'static str] {
    match platform {
        ChatPlatform::YouTube => &["$1+", "$2+", "$5+", "$10+", "$20+", "$50+", "$100+"],
        _ => &["1+", "100+", "1,000+", "5,000+", "10,000+"],
    }
}

pub struct ManualWindow {
    engine: EngineHandle,
    /// Set once the engine has added the manual source.
    source: Option<(SourceId, ManualInput)>,
    platform: ChatPlatform,
    kind: Kind,
    role: Role,
    /// Donations only: `None` tries a theme's look without a tier.
    tier: Option<u32>,
    author: Entity<InputState>,
    text: Entity<InputState>,
    amount: Entity<InputState>,
    count: Entity<InputState>,
    info: Entity<InputState>,
    error: Option<String>,
    sent: u64,
    last_id: Option<String>,
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
        let amount = input(
            "amount as shown, e.g. €5.00 or 100 bits",
            "€5.00",
            window,
            cx,
        );
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
            tier: None,
            author,
            text,
            amount,
            count,
            info,
            error: None,
            sent: 0,
            last_id: None,
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
            Kind::Donation => MessageKind::Donation {
                amount: non_empty(value(&self.amount), "the amount")?,
                tier: self.tier,
            },
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
                avatar_url: None,
            },
            // Donations and memberships can come with a message too.
            text: if matches!(self.kind, Kind::Gift | Kind::Notice) {
                String::new()
            } else {
                text
            },
            emotes: Vec::new(),
            timestamp: chrono::Utc::now(),
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
        self.last_id = Some(message.id.clone());
        cx.notify();
        self.send_event(ChatEvent::Message(message), window, cx);
    }

    /// Deletes the last sent message, to try out moderation in the overlay.
    fn delete_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(message_id) = self.last_id.take() {
            let event = ChatEvent::Delete {
                platform: self.platform,
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

    /// Donation tier: none, or one per step of the platform's scale.
    fn tier_field(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let steps = tier_steps(self.platform);
        let radios = std::iter::once("none")
            .chain(steps.iter().copied())
            .enumerate()
            .map(|(i, label)| Radio::new(i).label(label));
        let tiers = RadioGroup::horizontal("manual-tier")
            .children(radios)
            // Index 0 is "none", index n is tier n.
            .selected_index(Some(self.tier.map_or(0, |t| t as usize)))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.tier = (*index > 0).then_some(*index as u32);
                cx.notify();
            }));
        labelled("Tier", tiers, cx)
    }
}

impl Render for ManualWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let platforms = RadioGroup::horizontal("manual-platform")
            .selected_index(Some(match self.platform {
                ChatPlatform::YouTube => 1,
                _ => 0,
            }))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.platform = if *index == 1 {
                    ChatPlatform::YouTube
                } else {
                    ChatPlatform::Twitch
                };
                // Twitch has fewer tiers: keep the choice on the new scale.
                let top = tier_steps(this.platform).len() as u32;
                this.tier = this.tier.map(|t| t.min(top));
                cx.notify();
            }))
            .child(Radio::new(0).label("Twitch"))
            .child(Radio::new(1).label("YouTube"));

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
                .child(self.field("Amount", &self.amount, "manual-amount", cx))
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
                            .ghost()
                            .disabled(self.last_id.is_none())
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

/// A form row: the label in a fixed-width column, then the control.
/// (`E` is named rather than `impl IntoElement` so the result can say it
/// holds an `E` but borrows nothing: `use<E>`.)
fn labelled<E: IntoElement>(
    label: &'static str,
    control: E,
    cx: &App,
) -> impl IntoElement + use<E> {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(90.))
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().child(control))
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
    use std::net::Ipv4Addr;
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
        let engine = runtime
            .block_on(Engine::start(EngineConfig {
                bind: (Ipv4Addr::LOCALHOST, 0).into(),
                ..EngineConfig::default()
            }))
            .unwrap();
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

    fn pick_kind(window: &mut gpui_kit::Window, kind: Kind, cx: &mut gpui_kit::App) {
        let index = Kind::ALL.iter().position(|k| *k == kind).unwrap();
        window.within("manual-kind").click(index, cx);
    }

    #[gpui_kit::test]
    fn each_kind_shows_its_fields(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            // Every kind: author and role. Message (selected at start): text.
            assert!(window.within("manual-role").try_find(0).is_some());
            assert!(window.try_find("manual-text").is_some());
            assert!(window.try_find("manual-amount").is_none());
            // Donation: amount, tier and text.
            pick_kind(window, Kind::Donation, cx);
            assert!(window.try_find("manual-amount").is_some());
            // (A radio group's id is a scope for its radios, not an element.)
            assert!(window.within("manual-tier").try_find(0).is_some());
            assert!(window.try_find("manual-text").is_some());
            // Gift: only the count.
            pick_kind(window, Kind::Gift, cx);
            assert!(window.try_find("manual-count").is_some());
            assert!(window.try_find("manual-text").is_none());
            assert!(window.try_find("manual-amount").is_none());
        })
        .unwrap();
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
    fn tier_follows_the_platforms_scale(cx: &mut TestAppContext) {
        let ui = open(cx);
        let tier = |cx: &mut TestAppContext| {
            ui.view.read_with(cx, |view, cx| match view.compose(cx) {
                Ok(m) => match m.kind {
                    MessageKind::Donation { tier, .. } => tier,
                    other => panic!("expected a donation, got {other:?}"),
                },
                Err(e) => panic!("{e}"),
            })
        };
        cx.update_window(ui.window.into(), |_, window, cx| {
            pick_kind(window, Kind::Donation, cx);
            window.within("manual-platform").click(1, cx); // YouTube
        })
        .unwrap();
        assert_eq!(tier(cx), None, "no tier until one is picked");

        cx.update_window(ui.window.into(), |_, window, cx| {
            window.within("manual-tier").click(7, cx); // $100+
        })
        .unwrap();
        assert_eq!(tier(cx), Some(7));

        // Twitch's scale ends at 5: the choice moves to its top tier.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.within("manual-platform").click(0, cx);
        })
        .unwrap();
        assert_eq!(tier(cx), Some(5));

        cx.update_window(ui.window.into(), |_, window, cx| {
            window.within("manual-tier").click(0, cx); // none
        })
        .unwrap();
        assert_eq!(tier(cx), None);
    }

    #[gpui_kit::test]
    fn form_problems_are_explained(cx: &mut TestAppContext) {
        let ui = open(cx);
        let empty = ui.view.read_with(cx, |view, cx| view.compose(cx));
        assert!(empty.is_err_and(|e| e.contains("message")));

        cx.update_window(ui.window.into(), |_, window, cx| {
            pick_kind(window, Kind::Gift, cx);
            window.click("manual-count", cx);
            window.input("x", cx); // "5x"
        })
        .unwrap();
        let gift = ui.view.read_with(cx, |view, cx| view.compose(cx));
        assert!(gift.is_err_and(|e| e.contains("whole number")));
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

        cx.update_window(ui.window.into(), |_, window, cx| {
            pick_kind(window, Kind::Donation, cx);
            window.within("manual-tier").click(2, cx); // Twitch: 100+ bits
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
                    matches!(m.kind, MessageKind::Donation { amount, tier: Some(2) } if amount == "€5.00")
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
