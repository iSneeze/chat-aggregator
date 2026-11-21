//! The "Add a source" card at the bottom of the main window: platform picker,
//! dynamic text/token fields, YouTube channel selector, validation, and submit.

use chat_engine::{SourceConfig, YouTubeSettings};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme, IconName, StyledExt, h_flex};
use gpui_kit::*;

use super::AppView;
use crate::ui::{card, choice, report, segmented};

/// What the add form creates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NewSource {
    Twitch,
    YouTubeOwn,
    YouTubeVideo,
    Picarto,
    Rplay,
    StreamElements,
    Streamlabs,
    Demo,
}

impl NewSource {
    pub(super) const ALL: [NewSource; 8] = [
        NewSource::Twitch,
        NewSource::YouTubeOwn,
        NewSource::YouTubeVideo,
        NewSource::Picarto,
        NewSource::Rplay,
        NewSource::StreamElements,
        NewSource::Streamlabs,
        NewSource::Demo,
    ];

    fn label(self) -> &'static str {
        match self {
            NewSource::Twitch => "Twitch",
            NewSource::YouTubeOwn => "YouTube (your broadcasts)",
            NewSource::YouTubeVideo => "YouTube (a video)",
            NewSource::Picarto => "Picarto",
            NewSource::Rplay => "RPlay",
            NewSource::StreamElements => "StreamElements tips",
            NewSource::Streamlabs => "Streamlabs tips",
            NewSource::Demo => "Demo",
        }
    }

    /// Placeholder for the text field, if this kind needs one.
    fn needs_text(self) -> Option<&'static str> {
        match self {
            NewSource::Twitch => Some("channel name"),
            NewSource::YouTubeVideo => Some("video id (the part after watch?v=)"),
            NewSource::Picarto => Some("channel name or picarto.tv link"),
            // Like Streamlabs: the widget link says whose channel it is,
            // the name is for the app's list (one per persona).
            NewSource::Rplay => Some("a name of your choice, e.g. your RPlay name"),
            NewSource::StreamElements => Some("Account ID (StreamElements: your avatar → channel)"),
            // Only for the app's list: the token doesn't say whose account
            // it is, and a streamer may have one per persona.
            NewSource::Streamlabs => Some("a name for this account, e.g. your username or a label"),
            NewSource::YouTubeOwn | NewSource::Demo => None,
        }
    }

    /// Whether this kind needs a secret too (in a second, masked field):
    /// it goes to the secret store, not into the source's settings. The
    /// field's placeholder, and what to say when it's left empty.
    fn needs_token(self) -> Option<(&'static str, &'static str)> {
        match self {
            NewSource::StreamElements => Some((
                "Overlay Token (same page)",
                "Paste the Overlay Token too (same dashboard page).",
            )),
            NewSource::Streamlabs => Some((
                "Socket API Token",
                "Paste the Socket API Token too (Streamlabs dashboard: Settings → API Settings → \
                 API Tokens → Your Socket API Token).",
            )),
            // A link, not a token: that's what RPlay's studio shows. It
            // holds a key that can do much more than read chat, so it gets
            // the masked field too (streamers' screens are often on stream).
            NewSource::Rplay => Some((
                "widget link (RPlay studio → Dashboard Widget → Stream Chat)",
                "Paste a widget link too: in your RPlay studio, open Dashboard Widget, click \
                 Generate Widget URLs, then Show next to Stream Chat, and copy that link.",
            )),
            _ => None,
        }
    }

    /// `account`: the YouTube channel whose broadcasts to read (its id).
    fn config(self, text: &str, account: Option<String>) -> SourceConfig {
        let text = text.trim().to_string();
        match self {
            NewSource::Twitch => SourceConfig::Twitch { channel: text },
            NewSource::YouTubeOwn => SourceConfig::YouTube {
                video_id: None,
                account,
            },
            NewSource::YouTubeVideo => SourceConfig::YouTube {
                video_id: Some(text),
                account: None,
            },
            NewSource::Picarto => SourceConfig::Picarto { channel: text },
            NewSource::Rplay => SourceConfig::Rplay { label: text },
            NewSource::StreamElements => SourceConfig::StreamElements { account: text },
            NewSource::Streamlabs => SourceConfig::Streamlabs { label: text },
            NewSource::Demo => SourceConfig::Demo,
        }
    }
}

impl AppView {
    /// The channel a new "your broadcasts" source reads: the one picked,
    /// or the first connected. Always named in the source, so it stays
    /// with that channel even if the first one is logged out later. `None`
    /// before any channel is connected.
    fn new_youtube_account(&self, youtube: &YouTubeSettings) -> Option<String> {
        youtube
            .account_for(self.new_account.as_deref())
            .or(youtube.accounts.first())
            .map(|account| account.channel_id.clone())
    }

    pub(super) fn add_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        let youtube = self.config.read(cx).settings().youtube;
        let config = self
            .new_source
            .config(&text, self.new_youtube_account(&youtube));
        if let Err(problem) = config.validate() {
            self.form_error = Some(problem);
            cx.notify();
            return;
        }
        let needs_token = self.new_source.needs_token();
        let token = self.token.read(cx).value().trim().to_string();
        if let Some((_, missing)) = needs_token
            && token.is_empty()
        {
            self.form_error = Some(missing.into());
            cx.notify();
            return;
        }
        // What can be checked without connecting (an RPlay link without a
        // key in it): said here, before anything is stored.
        if needs_token.is_some()
            && let Err(problem) = config.prepare_token(&token)
        {
            self.form_error = Some(problem);
            cx.notify();
            return;
        }
        self.form_error = None;
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.token
            .update(cx, |input, cx| input.set_value("", window, cx));

        // The engine saves it (and every other change to the sources); a
        // token goes to the secret store first.
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let added = match needs_token {
                Some(_) => engine.add_source_with_token(config, &token).await,
                None => engine.add_source(config).await,
            };
            if let Err(e) = added {
                let message = format!("Couldn't add the source: {e:#}");
                let _ = this.update_in(cx, |_, window, cx| report(window, cx, message));
            }
        })
        .detach();
    }

    pub(super) fn render_add_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kinds = segmented(
            cx,
            NewSource::ALL.into_iter().enumerate().map(|(i, kind)| {
                choice(("kind", i), kind.label(), kind == self.new_source).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.new_source = kind;
                        this.form_error = None;
                        if let Some(placeholder) = kind.needs_text() {
                            this.input.update(cx, |input, cx| {
                                input.set_placeholder(placeholder, window, cx)
                            });
                        }
                        if let Some((placeholder, _)) = kind.needs_token() {
                            this.token.update(cx, |input, cx| {
                                input.set_placeholder(placeholder, window, cx)
                            });
                        }
                        cx.notify();
                    },
                ))
            }),
        );

        let mut row = h_flex().gap_2().items_center();
        // Your broadcasts on which channel, if more than one is connected.
        let youtube = self.config.read(cx).settings().youtube;
        if self.new_source == NewSource::YouTubeOwn && youtube.accounts.len() >= 2 {
            let chosen = self.new_youtube_account(&youtube);
            let channels = segmented(
                cx,
                youtube.accounts.iter().enumerate().map(|(i, account)| {
                    let selected = chosen.as_ref() == Some(&account.channel_id);
                    let id = account.channel_id.clone();
                    choice(("youtube-account", i), account.name.clone(), selected).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.new_account = Some(id.clone());
                            cx.notify();
                        }),
                    )
                }),
            );
            row = row
                .child(
                    div()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground)
                        .child("Channel"),
                )
                .child(channels);
        }
        if self.new_source.needs_text().is_some() {
            row = row.child(
                div()
                    .flex_1()
                    .child(Input::new(&self.input).id("new-source-text")),
            );
        }
        if self.new_source.needs_token().is_some() {
            row = row.child(
                div()
                    .flex_1()
                    .child(Input::new(&self.token).id("new-source-token")),
            );
        }
        row = row.child(
            Button::new("add")
                .icon(IconName::Plus)
                .label("Add")
                .primary()
                .on_click(cx.listener(|this, _, window, cx| this.add_source(window, cx))),
        );

        // The title beside the kinds rather than above them: a row less.
        // The track's `self_start` keeps the title level with its first
        // line when it wraps in a narrow window.
        let heading = h_flex()
            .gap_3()
            .items_center()
            .child(div().flex_none().font_semibold().child("Add a source"))
            .child(kinds);

        card(cx).child(heading).child(row).children(
            self.form_error
                .clone()
                .map(|error| div().text_sm().text_color(cx.theme().danger).child(error)),
        )
    }
}
