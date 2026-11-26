# Overlay themes

A theme decides how the chat looks on stream. chat-aggregator comes with
built-in themes you can customize right in the app; your own themes are folders
you can edit with any text editor, share, and switch between in the app.

## Built-in themes

- **Default**: a card per message, with badges, the event line (amounts,
  subs, raids) and the text.
- **Minimal**: one line per message, `Name: text`, outlined so it reads
  over any game. A small bar at the start shows the platform (purple
  Twitch, red YouTube); names are white for viewers, green for members and
  subscribers, blue with a wrench for moderators, yellow for the channel
  owner. Donations show their amount in the platform's tier colour, subs
  and gifts in green, all in the same line.
- **Bubbles**: rounded speech bubbles with a floating nameplate pill perched
  on the top edge, an optional curved speech tail, avatars beside the bubble,
  and per-chatter colors or alternating conversation alignment.
- **Cozy**: warm pastel stationery cards with a stitched inner border, hard
  offset shadow, subtle scrapbook tilt, and a persistent decorative charm
  (sparkle, heart, star, blossom, leaf, or moon) per chatter.
- **Retro**: classic 16-bit JRPG dialogue boxes with a speaker nameplate tab,
  framed portraits, pixel/handheld/CRT/Win95 frame styles, optional scanlines,
  and a blinking advance cursor on the newest message.
- **Neon**: cybernetic HUD cards with 45° chamfered corners, dual-color neon
  accents, configurable glow intensity, and *Chamfered*, *Brackets*, *Neon
  Tube*, and *Cyber Slash* frame styles.
- **Ornate**: gilded baroque and dark-fantasy cards with double hairline
  borders, symmetric inline-SVG corner flourishes, *Filigree*, *Tarot*,
  *Cathedral*, and *Velvet* frame styles, optional centered title plaques, and
  persistent per-chatter arcane crests.
- **Aero**: Frutiger Aero and early-2000s glossy glass panels with
  self-contained specular sheen gradients, 3D glass platform orbs, subtle
  seeded bokeh bubbles, and *Aero Glass*, *Aqua Pill*, *Wii Tile*, and *Y2K
  Chrome* frame styles.
- **Comic**: bold graphic-print panels with thick ink outlines, hard offset
  shadows, Ben-Day halftone screentone dots, breakout speaker ribbons, and
  *Phantom*, *Manga*, *Pulp*, and *Zine* panel styles.

Pick them in the app's theme list, or `theme = "minimal"` / `theme = "bubbles"`
/ `theme = "cozy"` / `theme = "retro"` / `theme = "neon"` / `theme = "ornate"`
/ `theme = "aero"` / `theme = "comic"` under `[overlay]` in a profile's file (`profiles/<name>.toml`). Each profile has
its own theme. They're built into the app and improve with its updates,
so they can't be edited in place; open **Customize…** and use **New theme…**
(or **Copy as custom…** on any built-in theme) to create an editable folder
starting from any built-in theme's files.

## Customizing themes without CSS

Click **Customize…** next to the theme picker to open the **Themes** window:

- **1-click presets** let you switch palettes right away (e.g. *Midnight*,
  *Twitch Neon*, *Sakura*, *Matcha*, *Warm Latte*, *Floating* on Default;
  *Classic*, *High Contrast*, *Soft Ivory*, *Terminal* on Minimal; *Cotton
  Candy*, *Lavender Milk*, *Sky Cloud*, *Mint Cream*, *Midnight Pop*, *Rainbow
  Chat* on Bubbles; *Strawberry Milk*, *Matcha Cream*, *Peach Tea*,
  *Blueberry Milk*, *Honey Journal*, *Evening Cocoa* on Cozy; *Classic JRPG*,
  *Win95*, *Handheld*, *Parchment*, *Arcade Dark*, *Amber CRT*, *8-Bit Crimson*
  on Retro; *Cyberpunk*, *Synthwave*, *Matrix*, *Tactical*, *Vaporwave*,
  *Hyper Crimson* on Neon; *Vampire Velvet*, *Obsidian Gold*, *Celestial
  Tarot*, *Cathedral Rose*, *Cursed Emerald*, *Ivory Grimoire* on Ornate;
  *Frutiger Sky*, *Aqua Reef*, *Wii Pearl*, *Citrus Gel*, *Y2K Chrome*,
  *Aurora Dusk* on Aero; or *Phantom Thief*, *Cursed Apple*, *Noir Patron*,
  *Shonen Ink*, *Noir Manga*, *Risograph* on Comic).
- **Match image…** reads a screenshot of your OBS scene or character artwork
  and suggests three contrast-checked color palettes you can apply in one click.
- **Colors, typography, motion, and elements** update connected OBS overlays
  **live without reloading** (and are saved per profile and per theme under
  `[overlay.theme_options.<theme>]`). Under **Font**, you can pick one of the
  theme's built-in font stacks, search your installed system fonts, or click
  **Import…** to copy a `.ttf`, `.otf`, `.woff2`, or `.woff` file into the
  app's `fonts/` folder and serve it straight to OBS.
- **Preview** opens a local browser preview (`/preview?theme=…`) with sample
  messages, switchable light/dark/checkerboard backdrops, width presets, and a
  live animation drip mode so you can style your chat before going live.

## Where themes live

In the **themes** folder next to chat-aggregator's settings (the app's
**Open themes folder** button takes you there):

```text
themes/
├── my-theme/
│   ├── overlay.css     the look: colours, fonts, sizes, animations
│   ├── theme.toml      optional: customizable options and presets in the app
│   ├── message.html    optional: the structure of one message
│   └── bg.png, …       optional: images and fonts the CSS uses
└── dark-card/
    └── overlay.css
```

The folder name is the theme's name. Every file is optional: anything a
theme doesn't have comes from the built-in theme (except `theme.toml`: a
custom folder without one simply shows no option controls). Most themes only
need `overlay.css`. (A theme folder somewhere else works too: put its full path
in the profile's file, e.g. `theme = "/home/me/obs/my-theme"`.)

## Exposing options in a custom theme (`theme.toml`)

Any custom theme folder can include a `theme.toml` file to get native controls
in the **Themes** window (and live updates in OBS without reloading). Each
`[[options]]` entry supports four types — `color`, `number`, `bool`, and
`choice` — mapped onto CSS custom properties (`css_var` / `css_values`) and/or
modifier classes on `<main class="chat">` (`class`, `class_when_false`,
`class_values`):

```toml
[[options]]
name = "accent_color"
label = "Accent"
group = "Colors"
type = "color"
default = "#9146ff"
css_var = "--chat-accent"

[[options]]
name = "font_size"
label = "Text size"
group = "Typography"
type = "number"
default = 20
min = 12
max = 42
step = 1
unit = "px"
css_var = "--chat-font-size"

[[options]]
name = "show_avatars"
label = "Avatars"
group = "Elements"
type = "bool"
default = true
class_when_false = "chat--hide-avatars"

[[options]]
name = "corners"
label = "Corners"
group = "Typography"
type = "choice"
default = "Rounded"
options = ["Sharp", "Rounded", "Pill"]
css_var = "--chat-radius"
[options.css_values]
Sharp = "4px"
Rounded = "12px"
Pill = "20px"

# Optional 1-click presets shown above the form:
[[presets]]
name = "Sakura"
[presets.values]
accent_color = "#ff85b3"
corners = "Pill"
```

Option names must be 1–40 ASCII characters starting with a letter; `css_var`
must match `--lowercase-name`; class names must be lowercase and cannot use
the reserved `chat` or `chat--newest-top` classes.

## Making a theme

1. In the app: open **Customize…**, click **New theme…** (or **Copy as
   custom…** on a built-in theme), choose which built-in theme to start from,
   give it a name, and click **Create**. This copies that built-in theme's
   `overlay.css`, `message.html`, and `theme.toml` into a new folder.
2. Click **Open folder**, open the new folder, and edit `overlay.css` (and
   optionally `theme.toml` or `message.html`).
3. Click **Use as active** and **Reload** in the app: OBS shows the change
   right away.

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

## What the overlay shows

Some messages may be ones you'd rather not repeat in the chat box: tips
that StreamElements' or Streamlabs' own alert box already shows, Super Chats, raid
notices. These are left out before they reach the overlay, so a theme
doesn't have to hide them:

- **In the app:** ⚙ Settings → Overlay, **Platforms shown** and **Messages
  shown**. A switch turned off keeps that platform or kind of message out
  of overlays; it applies to new messages right away. Chat reactions and
  the JSON API still get everything, so a hidden tip still fires its
  reaction.
- **Per browser source:** add `?hide=` and the names, separated by commas,
  to the overlay URL. That wins over the app's setting, so one scene can
  have a chat box without tips and another one with everything:

```text
http://127.0.0.1:7878/?hide=streamelements
http://127.0.0.1:7878/?hide=donation,special,notice&newest=top
http://127.0.0.1:7878/?hide=        everything, whatever the app hides
```

The names: platforms `twitch`, `youtube`, `picarto`, `rplay`,
`streamelements`, `streamlabs`;
kinds of message `text`, `emote_only`, `donation` (bits, Super Chats,
tips), `special` (stickers, gifts), `membership`, `gift` (gifted subs),
`notice` (raids, announcements). Names it doesn't know are skipped.

## A theme per browser source

Normally every overlay shows the theme picked in the app. To give one OBS
scene its own theme, add `?theme=` and the theme's name to its overlay URL:

```text
http://127.0.0.1:7878/?theme=my-theme
http://127.0.0.1:7878/?theme=my-theme&newest=top
http://127.0.0.1:7878/?theme=cozy        a built-in theme (default, minimal, bubbles, cozy, retro, neon, ornate, aero, comic)
http://127.0.0.1:7878/?theme=cozy&preset=Matcha%20Cream&max_messages=10
```

That overlay keeps its theme whatever the app is set to. You can also pin
specific `theme.toml` options or a preset directly in the URL (clicking
**Copy URL** in the **Themes** window builds the shortest `?theme=…&preset=…`
URL for your current settings automatically). A URL with option or preset
parameters stays locked to those values even if you later change the theme's
settings in the app for another OBS scene; a plain `?theme=<name>` URL without
option parameters continues to update live with the app. **Reload overlays** in
the app still reloads it (with its own theme and parameters), so editing its
files works as usual. A name with a space goes into the URL as `%20`
(`?theme=my%20theme`). If the name doesn't match a theme folder, the overlay
shows the app's theme instead, and the log says why.

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

## Colours by amount

Paid messages (Super Chats, Super Stickers, bits, tips) get a class for
how big the amount is: `msg--tier-1` to `msg--tier-7`. It's one scale for
every platform, YouTube's Super Chat tiers:

| platform | tiers |
|----------|-------|
| YouTube | YouTube's own tier, whatever the currency: 1 ($1+), 2 ($2+), 3 ($5+), 4 ($10+), 5 ($20+), 6 ($50+), 7 ($100+) |
| Twitch  | bits, YouTube's dollar steps × 100: 1 (below 200), 2 (200+), 3 (500+), 4 (1,000+), 5 (2,000+), 6 (5,000+), 7 (10,000+) |
| StreamElements | tips, from the amounts set in ⚙ Settings → Paid messages (by default YouTube's steps) |
| Streamlabs | the same; a tip in another currency than the one set there is 1 |

The colours can still be each platform's own. YouTube's chat shows its
tiers as blue, cyan, teal, yellow, orange, magenta and red; Twitch's cheer
colours are five, so two tiers share some of them:

```css
.msg--youtube.msg--tier-1 { --paid-color: #1e88e5; }
.msg--youtube.msg--tier-4 { --paid-color: #ffca28; }
/* Twitch: grey, purple (2, 3), teal (4, 5), blue, red */
.msg--twitch.msg--tier-2,
.msg--twitch.msg--tier-3  { --paid-color: #9c3ee8; }
```

The Minimal theme colours plain `.msg--tier-1` … `.msg--tier-7` the
YouTube way, for every platform, and gives Twitch its own with
`.msg--twitch.msg--tier-N`: two classes beat one, so those win.

Messages without a tier (YouTube jewel gifts, the app's test messages)
have no tier class, so give `--paid-color` a default too.

## A look per chatter

Every chatter can get something of their own (a colour, a shape, a
little marker next to the name) that stays the same every time they write,
across streams and name changes. The `seed` filter turns their id into a
number:

{% raw %}
```html
<span class="marker marker--shape-{{ author.id | seed(6, 'shape') }}"
      style="--marker-hue: {{ author.id | seed(360, 'hue') }}"></span>
```
{% endraw %}

`value | seed(n, 'salt')` gives a number from 0 to n−1: `seed(6, …)` picks
one of six shapes (style `.marker--shape-0` to `.marker--shape-5`),
`seed(360, …)` a hue for `hsl(var(--marker-hue) 80% 60%)` or
`filter: hue-rotate(…)`. Each salt (`'shape'`, `'hue'`, any word) gives an
independent number, so shape and colour don't go hand in hand. The numbers
never change between versions of chat-aggregator.

Good to know: it's the same person on Twitch and YouTube, but two ids, so
two looks. And two chatters can land on the same combination; the more
choices you combine (shapes × colours × sizes …), the rarer that gets.

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
