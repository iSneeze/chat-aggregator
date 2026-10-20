# YouTube channel emoji

YouTube's API sends custom channel emoji only as text, like `:_hype:`,
without the picture. (Normal emoji like 😀 always work; this is only about
channel emoji.)

## Learned automatically

While chat-aggregator reads a YouTube chat, it also looks at YouTube's own
chat page for that stream, the way a logged-out visitor sees it, and
remembers the picture of every channel emoji someone uses. You don't have
to do anything.

- Learned emoji are kept per channel (in the `youtube-emojis` folder in
  the settings folder), so next stream they work from the first message.
- The very first time an emoji is used, that one message usually shows it
  as text: chat-aggregator learns the picture a few seconds after the
  message arrives. From then on it's a picture.
- Members-only streams: a logged-out visitor can't see their chat, so
  nothing is learned there. Emoji used in your other streams still work;
  for the rest, load them all at once (below).
- To switch it off: ⚙ Settings → Connection → **Learn YouTube emoji**
  (`learn_emojis = false` in the `[youtube]` section of `config.toml`).

This uses YouTube's chat page, not its official API, so a change on
YouTube's side can stop it working until chat-aggregator is updated. Your
chat isn't affected when that happens; only emoji not learned yet stay
text.

## Load all emoji at once (optional)

To have every emoji of a channel from the start, including ones nobody has
used yet, a small browser script reads the full list from YouTube's emoji
picker. You do this once per channel, and again when the channel adds new
emoji. Learned emoji are used on top of the list.

### 1. Install the script

1. Install a userscript manager in your browser:
   [Violentmonkey](https://violentmonkey.github.io/) or
   [Tampermonkey](https://www.tampermonkey.net/).
2. Open
   [yt-emoji-export.user.js](https://github.com/iSneeze/chat-aggregator/raw/master/scripts/yt-emoji-export.user.js)
   and confirm the installation. The manager keeps it up to date.

### 2. Export the emoji

1. Open the live chat of the channel whose emoji you want, ideally as a
   pop-out: on a live stream or premiere, chat menu **⋮ → Pop-out chat**.
   Channel emoji only appear in their own channel's chat.
2. Open the chat's **emoji picker** (the smiley in the message field). The
   script adds a **⤓** button at the end of the picker's category row.
3. Click **⤓**: the script scrolls through the picker to collect every
   emoji, then downloads `youtube-emojis-<channel id>.json`.

Emoji you can't use yourself (members-only) are collected too, as long as
the picker shows them.

### 3. Load them into chat-aggregator

In the app's **YouTube** section, click **Choose file…** next to "Custom
emoji" (or drag the file onto the section). chat-aggregator checks the
file, copies it into its settings folder and shows how many emoji it
found; the original download can go. YouTube chat reconnects once to pick
them up.

Without the app: set `emojis = "/path/to/file.json"` in the `[youtube]`
section of `config.toml`.
