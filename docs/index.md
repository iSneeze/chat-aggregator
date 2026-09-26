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
- Runs locally: no account with us, no server of ours in between.

## Documentation

- [Connecting YouTube](youtube-setup.md): the one-time setup of your own
  Google Cloud project and login.
- [Chat events API](api.md): the JSON WebSocket feed for your own programs
  ([AsyncAPI spec](asyncapi.yaml)).
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
