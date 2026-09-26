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

Chat text is always escaped, so whatever chatters type can never break the
page or run code in it.
