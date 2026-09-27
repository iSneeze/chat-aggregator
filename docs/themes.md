# Overlay themes

A theme decides how the chat looks on stream. chat-aggregator comes with a
built-in theme ("Default"); your own themes are folders you can edit with
any text editor, share, and switch between in the app.

## Where themes live

In the **themes** folder next to chat-aggregator's settings (the app's
**Open themes folder** button takes you there):

```text
themes/
├── cozy/
│   ├── overlay.css     the look: colours, fonts, sizes, animations
│   ├── message.html    optional: the structure of one message
│   └── bg.png, …       optional: images and fonts the CSS uses
└── minimal/
    └── overlay.css
```

The folder name is the theme's name. Every file is optional: anything a
theme doesn't have comes from the built-in theme. Most themes only need
`overlay.css`.

## Making a theme

1. In the app: **New theme…**, give it a name, **Create**. This copies the
   built-in `overlay.css` and `message.html` into a new folder and switches
   the overlay to it.
2. **Open themes folder**, open the new folder, edit `overlay.css`.
3. **Reload overlays** in the app: OBS shows the change right away.

Start with the variables at the top of `overlay.css`: fonts, colours, card
style and sizes are all set there. Two of them control the overlay's
behaviour instead of its look:

- `--max-messages`: how many messages stay on screen (default 20).
- `--delete-delay`: how long a message deleted by a moderator stays
  before it's removed, for a fade-out effect (default `0s`).

## Chat direction

Where new messages appear is a scene choice rather than a theme choice, so
it isn't set in the theme:

- **In the app:** the dropdown at the right end of the theme row, **Newest
  at bottom** (the chat grows upward, the classic look) or **Newest at
  top** (it grows downward). Overlays reload to show the change.
- **Per browser source:** add `?newest=top` or `?newest=bottom` to the
  overlay URL, e.g. `http://127.0.0.1:7878/?newest=top`. That wins over the
  app's setting, so each OBS scene keeps its layout whatever the default is.

With the newest at the top, the chat element gets the class
`chat--newest-top`; the built-in theme uses it to let messages slide in
from above instead of below (`--msg-enter-offset`). Themes made before this
option existed work too: the overlay flips the direction itself.

## Images and fonts

Put them into the theme folder and use relative paths in `overlay.css`:

```css
@font-face {
  font-family: "My Font";
  src: url(fonts/MyFont.woff2);
}
.chat { background: url(bg.png); }
```

## Changing the structure (`message.html`)

`message.html` is a [MiniJinja](https://docs.rs/minijinja) template for one
message. The comment at the top of the built-in one lists every value it
can use (author, badges, emotes, amounts, …); the classes it sets
(`msg--donation`, `msg--twitch`, `badge--moderator`, …) are what the CSS
styles. Keep the root element's `msg` class and its `data-id`,
`data-platform` and `data-author` attributes: the overlay needs them to
remove deleted messages.

## Safety

Chat text is always escaped, so whatever chatters type can never break the
page or run code in it. Two rules keep it that way:

- **Never use `| safe`** on chat values (`author.name`, `part.text`,
  `amount`, `info`, …) in `message.html`. It switches the escaping off, and
  a chatter could then put script into your overlay. OBS runs overlays
  without Chromium's sandbox, so script there can reach your computer
  through an outdated OBS (this happened with another overlay:
  [CVE-2024-7971](https://cyberinsider.com/malicious-twitch-chat-messages-can-trigger-code-execution-on-obs-studio/)).
- **Only use themes from people you trust.** A theme's `message.html` is
  code that runs in OBS. chat-aggregator's overlay only allows its own
  script (a Content-Security-Policy blocks everything else, including
  `onerror=` tricks), but treat a stranger's theme like a stranger's
  program. `overlay.css` on its own is harmless: CSS can't run code.

And keep OBS up to date: its embedded browser gets security fixes with it.
