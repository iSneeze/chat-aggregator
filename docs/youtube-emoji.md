# YouTube channel emoji

YouTube's API sends custom channel emoji only as text, like `:_hype:`,
without the picture. To show them as images, chat-aggregator needs a list
of the codes and their image links. A small browser script reads that list
from YouTube's emoji picker; you do this once per channel, and again when
the channel adds new emoji.

(Normal emoji like 😀 always work; this is only about channel emoji.)

## 1. Install the script

1. Install a userscript manager in your browser:
   [Violentmonkey](https://violentmonkey.github.io/) or
   [Tampermonkey](https://www.tampermonkey.net/).
2. Open
   [yt-emoji-export.user.js](https://github.com/iSneeze/chat-aggregator/raw/master/scripts/yt-emoji-export.user.js)
   and confirm the installation. The manager keeps it up to date.

## 2. Export the emoji

1. Open the live chat of the channel whose emoji you want, ideally as a
   pop-out: on a live stream or premiere, chat menu **⋮ → Pop-out chat**.
   Channel emoji only appear in their own channel's chat.
2. Open the chat's **emoji picker** (the smiley in the message field). A
   small panel with an emoji counter appears in the top-left corner.
3. Click **Sweep**: the script scrolls through the picker and collects every
   emoji. The counter goes up.
4. Click **Download**. You get `youtube-emojis-<channel id>.json`.

Emoji you can't use yourself (members-only) are collected too, as long as
the picker shows them.

## 3. Load them into chat-aggregator

In the app's **YouTube** section, click **Choose file…** next to "Custom
emoji" (or drag the file onto the section). chat-aggregator checks the
file, copies it into its settings folder and shows how many emoji it
found; the original download can go. YouTube chat reconnects once to pick
them up.

Without the app: set `emojis = "/path/to/file.json"` in the `[youtube]`
section of `config.toml`.
