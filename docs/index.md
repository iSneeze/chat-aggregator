---
title: chat-aggregator
---

# chat-aggregator

**All your stream chats in one place.** chat-aggregator is a free desktop
app for streamers. It collects the live chat from Twitch, YouTube and
Picarto, plus tips from StreamElements and Streamlabs, and shows them as
one chat on your stream.

If you stream to several platforms at once, your viewers sit in several
chats that can't see each other. With chat-aggregator everyone watching
sees the same chat, and you have one chat to read instead of three.

**[Download and first steps](install.md)** (Windows and Linux)

## What it does

- **One chat on your stream.** Messages from every platform appear in one
  chat box in OBS (or any streaming software that can show a web page),
  with emotes, badges, donations, subs and memberships.
- **Your look.** Two built-in themes, or style the chat yourself with HTML
  and CSS.
- **Moderation carries over.** A message deleted on a platform disappears
  from your stream too.
- **Chat makes things happen.** `!bonk` throws an item at your model in
  Warudo; a big Super Chat puts a voice filter on your mic in OBS for 30
  seconds.
- **Widgets.** Other ways to show chat, like every emote floating up the
  screen or chat flying across it danmaku-style (Live Emoji and Danmaku
  come with the app), or widgets others made.
- **A feed for your own tools.** Games and bots can receive the merged
  chat as it happens.
- **Stays on your computer.** No account with us, and no server of ours in
  between.
- **Built with streamer safety in mind.** Chat is always shown as text,
  never run as code, and the overlay and widgets refuse every script but
  their own. (Keep OBS up to date too: its built-in browser gets its
  security fixes that way.)

## Guides

- [Download and first steps](install.md): getting the app, adding your
  channels, putting the chat into OBS.
- [Connecting YouTube](youtube-setup.md): the one-time setup of your own
  Google Cloud project and login.
- [Overlay themes](themes.md): making the chat look the way you want.
- [Widgets](widgets.md): chat shown other ways in OBS (emotes rising up
  the screen, danmaku), and how to write your own.
- [YouTube channel emoji](youtube-emoji.md): how channel emoji become
  pictures, and loading them all at once.
- [Chat events API](api.md): the JSON WebSocket feed for your own programs
  ([AsyncAPI spec](asyncapi.yaml)).
- [Building from source](development.md) and
  [Architecture](architecture.md): for developers.
- [Privacy policy](privacy.md)
- Source code: [github.com/iSneeze/chat-aggregator](https://github.com/iSneeze/chat-aggregator) (open source, [MIT license](https://github.com/iSneeze/chat-aggregator/blob/master/LICENSE))

## YouTube

chat-aggregator uses **YouTube API Services** to find your live broadcasts
and read their chat. By connecting YouTube you agree to be bound by the
[YouTube Terms of Service](https://www.youtube.com/t/terms); Google's
handling of your data is described in the
[Google Privacy Policy](https://policies.google.com/privacy). What
chat-aggregator itself does with the data is in our
[privacy policy](privacy.md).

chat-aggregator is an independent project, not affiliated with or endorsed
by Twitch, YouTube, Google, Picarto, StreamElements or Streamlabs.
