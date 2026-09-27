---
title: chat-aggregator
---

# chat-aggregator

**Twitch and YouTube live chat in one overlay for your stream.**

chat-aggregator runs on your own computer while you stream. It reads the
live chat of your channels on Twitch and YouTube, merges it into a single
chat, and shows it as a styled overlay you add to OBS (or any streaming
software) as a browser source. Other programs on your computer, like games
or bots, can receive the same chat as a live JSON feed.

- One chat for all platforms, with emotes, badges, donations, subs and
  memberships.
- Moderation carries over: messages deleted on a platform disappear from the
  overlay too.
- Fully stylable with your own HTML template and CSS.
- A small desktop app to set it up: add channels, connect YouTube, pick a
  theme, send test messages.
- Runs locally: no account with us, no server of ours in between.
- Built with streamer safety in mind: chat is always shown as text, never
  as code, and the overlay refuses to run any script but its own.
  (Keep OBS itself up to date too: its built-in browser gets security fixes
  with it.)

## Documentation

- [Connecting YouTube](youtube-setup.md): the one-time setup of your own
  Google Cloud project and login.
- [Overlay themes](themes.md): making the chat look the way you want.
- [YouTube channel emoji](youtube-emoji.md): showing custom emoji as images.
- [Chat events API](api.md): the JSON WebSocket feed for your own programs
  ([AsyncAPI spec](asyncapi.yaml)).
- [Architecture](architecture.md): for developers, how the program is
  built and why.
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
by YouTube, Google or Twitch.
