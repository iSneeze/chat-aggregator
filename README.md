# chat-aggregator

*A working title: the final name is still being decided.*

**All your stream chats in one place.** chat-aggregator is a free desktop
app for streamers. It collects the live chat from Twitch, YouTube, Picarto
and RPlay, plus tips from StreamElements and Streamlabs, and shows them as
one chat on your stream.

If you stream to several platforms at once, your viewers sit in several
chats that can't see each other. With chat-aggregator everyone watching
sees the same chat, and you have one chat to read instead of three.

Website and guides: <https://isneeze.github.io/chat-aggregator/>

## What it does

- **One chat on your stream.** Messages from every platform appear in one
  chat box in OBS (or any streaming software that can show a web page),
  with emotes, badges, donations, subs and memberships.
- **Your look.** Eight built-in themes with presets, live color and layout
  customization, and palette matching from an image — or style the chat
  yourself with HTML and CSS.
- **Moderation carries over.** A message deleted on a platform disappears
  from your stream too.
- **Chat makes things happen.** `!bonk` throws an item at your model in
  Warudo; a big Super Chat puts a voice filter on your mic in OBS for 30
  seconds.
- **Widgets.** Other ways to show chat, like every emote floating up the
  screen, or chat flying across it danmaku-style. And a timer you control
  from chat, which donations can make longer.
- **A feed for your own tools.** Games and bots can receive the merged
  chat as it happens.
- **Stays on your computer.** No account with us, and no server of ours in
  between.

## How you use it

1. Start the app and add your channels. Twitch and Picarto only need the
   channel name; YouTube needs a
   [one-time setup](https://isneeze.github.io/chat-aggregator/youtube-setup)
   of about 10 minutes.
2. Copy the overlay address from the app (`http://127.0.0.1:7878/`) and
   add it in OBS as a **Browser Source**.
3. Go live. The chat shows up on its own.

No account at hand? Add the **Demo** source: it sends sample messages of
every kind, so you can try themes and widgets before your first stream.

## Get it

Builds for **Windows** (10 or newer) and **Linux** are on the
[Releases page](https://github.com/iSneeze/chat-aggregator/releases).
There's no installer yet: unpack the archive and start the program.

[Download and first steps](https://isneeze.github.io/chat-aggregator/install)
covers what to expect on each system (Windows warns about the unsigned
file), where your settings are kept, and how to report a problem.

## What's supported

| Platform | What you get | What it needs |
|----------|--------------|---------------|
| Twitch | chat, bits, subs, gift subs, raids, announcements | the channel name |
| YouTube | chat, Super Chats, Super Stickers, memberships; unlisted and members-only streams too | your own free Google project ([setup](https://isneeze.github.io/chat-aggregator/youtube-setup)) |
| Picarto | chat and emotes | the channel name |
| StreamElements | tips | your Overlay Token |
| Streamlabs | tips, from several accounts if you have them | your Socket API Token |
| RPlay | chat, emotes, coins, missions, raids | your studio's Stream Chat widget link |

## Guides

- [Download and first steps](https://isneeze.github.io/chat-aggregator/install)
- [Connecting YouTube](https://isneeze.github.io/chat-aggregator/youtube-setup)
- [Overlay themes](https://isneeze.github.io/chat-aggregator/themes):
  making the chat look the way you want
- [Widgets](https://isneeze.github.io/chat-aggregator/widgets): using
  them, and writing your own
- [YouTube channel emoji](https://isneeze.github.io/chat-aggregator/youtube-emoji)
- [Chat events API](https://isneeze.github.io/chat-aggregator/api): the
  feed for your own programs
- [Privacy policy](https://isneeze.github.io/chat-aggregator/privacy/)

## Safety and privacy

Anyone can type anything into a chat, so chat-aggregator always shows
chat as text and never runs it as code, and the overlay refuses every
script but its own. Keep OBS up to date as well: its built-in browser gets
its security fixes that way.

Everything runs on your computer. What the app stores and sends is in the
[privacy policy](https://isneeze.github.io/chat-aggregator/privacy/).

## Where it stands

An early version. It works and has been tested on live streams, but there
is no installer yet and details may still change. Found a problem? Open an
[issue](https://github.com/iSneeze/chat-aggregator/issues) and attach the
app's log file
([where to find it](https://isneeze.github.io/chat-aggregator/install#reporting-a-problem)).

## For developers

chat-aggregator is written in Rust and open source.

- [Building and running from source](docs/development.md): the
  toolchain, the headless mode, how releases are made.
- [Architecture](docs/architecture.md): how the pieces fit together, and
  why.

## License

[MIT](LICENSE) © 2026 iSneeze

chat-aggregator is an independent project, not affiliated with or endorsed
by Twitch, YouTube, Google, Picarto, RPlay, StreamElements or Streamlabs.
