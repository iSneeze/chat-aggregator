# Widgets

A widget is a web page that shows chat its own way, as an OBS browser
source: emotes rising up the screen, a counter, a mini game. It gets the
chat from chat-aggregator's [JSON API](api.md), and chat-aggregator serves
it, so its settings can be picked in the app and travel in its URL.

Three widgets come with the app:

- **Live Emoji**: every emote and emoji sent in chat floats up from the
  bottom of the screen and fades away, in a lively "party" mood or a calm
  one.
- **Danmaku**: chat flies across the screen from right to left, as on
  Niconico or Bilibili, without names (unless you switch them on). Every
  comment takes the same time to cross the screen, so long ones move
  faster, and each one takes a lane where it never runs into the one in
  front. Paid messages and member events are pinned at the top (or the
  bottom) for a few seconds instead, the amount in the tier's colour.
  Raids and announcements are left out, and a message a moderator deletes
  disappears from the screen at once. When chat is faster than the lanes
  can carry, comments wait a few seconds for a free lane and are dropped
  after that; paid messages always get their turn.
- **Timer**: a countdown or a count-up timer that keeps exact time, also
  when OBS reloads it. You control it from chat (`!timer pause`), and
  donations and subs can make a countdown longer, as for a subathon. See
  [The Timer](#the-timer) below.

## Using a widget

1. Switch on the JSON API: widgets get chat through it (the app's Widgets
   window has a button for it, or **⚙ Settings → Connection → JSON API**).
2. Open **Widgets** in the app's main window and pick a widget.
3. Pick its settings. They're kept in the app, and switches and choices
   apply at once; typed values, and numbers changed with − and + (or the
   ↑ ↓ keys), once you stop.
4. **Copy URL**, and paste it into an OBS browser source's **URL** field
   (not "Local file"). Size the source like your canvas: widgets have a
   transparent background.

The URL carries the settings (`…/widgets/live-emoji?mode=calm&size=72`):
after changing them, copy the URL again into the browser source. **Reset**
puts every setting back to the widget's own default (it asks first). The
main window shows how many widgets are connected right now.

What the overlay leaves out (**⚙ Settings → Overlay**, `?hide=`) doesn't
apply to widgets: they get all of chat from the JSON API, and each widget
decides what it shows. Danmaku's **Paid messages: off**, for one, leaves
out tips that your tip service's alert box shows already.

### Importing a widget

**Import…** in the Widgets window picks a widget's `.html` file and copies
it into the `widgets` folder in chat-aggregator's settings folder. The app
checks it first and says why if it can't be used (no widget block, a
setting that makes no sense, a script loaded from elsewhere).

- A widget is named after its title: "Confetti Rain" becomes
  `confetti-rain`, in its URL and as its file name. Importing a newer
  version of it (same title) asks, then replaces the old one; the values
  picked for it stay where they still fit.
- **Open folder** shows the widgets folder. Widgets copied into it by hand
  show up too, and a file you edit there applies the next time OBS loads
  it (right-click the browser source → Refresh).
- **Remove** deletes the widget's file and forgets its settings.

**Only import widgets from people you trust.** A widget runs its own code
in OBS. chat-aggregator lets it read chat and nothing else on your
computer, but like any web page it can load pictures and fonts from the
web, and so could pass on what it shows to whoever runs those servers.

## The Timer

A clean, big countdown (or count-up) for Starting Soon screens, breaks or
a subathon. Pick the time in its **Hours**, **Minutes** and **Seconds**,
and if you like a **Label** above it ("Subathon"), a **Colour** as a hex
code (`#ffca28`), a **Font**, and a text to show **When it ends**
("Starting now!").

**It doesn't drift.** It remembers when it started, not how many seconds
it has counted, and works out the time from your computer's clock every
time it updates. Timers that count seconds one by one fall behind a
little with every tick; this one shows the right time after hours too.

**It keeps running through reloads.** OBS reloads browser sources now and
then (when you change one, or restart OBS). The timer remembers where it
was and carries on. A new time, direction or command word starts a new
timer; a new size, colour or label doesn't. Two browser sources with the
same URL show the same timer. With **Keep running through reloads**
switched off, it starts over every time the page loads: together with
OBS's "Refresh browser when scene becomes active" (in the browser
source's properties), that's a Starting Soon countdown that starts the
moment you show the scene.

**Chat commands.** Only you (as the channel's owner) can use them, unless
you set **Chat commands** to `mods` (you and your moderators) or `off`:

| command | does |
|---------|------|
| `!timer pause` | stops it |
| `!timer start` | runs it again (`!timer resume` works too) |
| `!timer reset` | back to the time you set; a paused timer stays paused |
| `!timer add 5m` | adds time: `90s`, `1h`, `1h30m`; a plain number is minutes (`!timer add 5`) |
| `!timer remove 5m` | takes time away |
| `!timer desc Day 2!` | a new label above the time, as typed (up to 60 characters) |
| `!timer desc` | back to the label from the settings |

So for a subathon: `!timer pause` and `!timer reset` while you get ready,
`!timer start` when it begins. A label set from chat stays through
reloads and `!timer reset`, until a new label in the app takes its place.
The **Command word** can be changed
(`!subathon`), to control two timers apart. Commands show up in the chat
overlay like any other message.

**Donations and subs** (only for a countdown, off until you switch on
**Donations add time**): every 1.00 donated adds **Time per 1.00
donated** (60 seconds: a minute per dollar).

- 100 bits and 20 RPlay coins count as 1.00.
- Super Chats, stickers and tips in **Your currency** (USD unless you set
  another) count at their amount. In another currency they count by their
  size on YouTube's Super Chat scale: $1, 2, 5, 10, 20, 50 or 100
  (chat-aggregator doesn't convert currencies). This works best with a
  currency about a dollar's size (USD, EUR, GBP …).
- **Time per sub** (0 unless you set it) is added for every new sub, resub
  or membership, and for each gifted one.

A "+5:00" floats up into the timer when time is added. Once a countdown
has ended, donations don't bring it back (`!timer add` does). Donations
made while the browser source isn't loaded (OBS closed, or the source
set to shut down when hidden) aren't counted.

Commands and donations need the JSON API switched on; without it, it's a
plain timer.

## Writing a widget

A widget is **one HTML file**: its scripts and styles inside it, pictures
and fonts from the web or as `data:` URLs. Somewhere in it, a JSON block
describes the widget and its settings:

```html
<script type="application/json" id="widget">
{
  "title": "Chat Counter",
  "description": "Counts the messages since the stream started.",
  "settings": [
    { "name": "size", "label": "Text size", "type": "number",
      "default": 48, "min": 12, "max": 200, "step": 2, "unit": "px" },
    { "name": "bots", "label": "Count bots", "type": "bool", "default": false,
      "help": "Messages from accounts with \"bot\" in their name." },
    { "name": "label", "label": "Label", "type": "text", "default": "messages" },
    { "name": "corner", "label": "Corner", "type": "choice",
      "default": "top-left", "options": ["top-left", "top-right", "bottom-left", "bottom-right"] }
  ]
}
</script>
```

| field         | meaning |
|---------------|---------|
| `title`       | the widget's name in the app; also its name in the URL (letters and digits, the rest as `-`) |
| `description` | optional: a sentence or two shown above the settings |
| `settings`    | optional: what the app's form offers, in this order |

Each setting has a `name` (its key in the URL query: letters, digits, `-`
and `_`), a `label` for the form, an optional `help` sentence, a `type` and
a `default`:

| `type`   | the URL carries | also needs |
|----------|-----------------|------------|
| `number` | `size=72`       | `min`, `max`; optional `step`, `unit` |
| `bool`   | `bots=1` or `bots=0` | |
| `text`   | `label=chat%20lines` (any text, URL-encoded) | |
| `choice` | `corner=top-right` | `options`: the choices, `default` among them |

A number's `step` is how far the app's − and + buttons move it, onto the
steps counted from `min` (with `"min": 12, "step": 2`: 12, 14, 16 …); 1 if
there's none. Typed values only have to be from `min` to `max`. `unit` is
shown after the number.

The URL only carries values that differ from the defaults, so the page
reads its defaults from the same block and the two can't disagree:

```js
const params = new URLSearchParams(location.search);

function settingsFrom(widget, query) {
  const values = {};
  for (const s of widget.settings) {
    const raw = query.get(s.name);
    let value = s.default;
    if (raw !== null) {
      if (s.type === "number") {
        const v = parseFloat(raw);
        if (Number.isFinite(v)) value = Math.min(s.max, Math.max(s.min, v));
      } else if (s.type === "bool") {
        if (raw === "1" || raw === "true") value = true;
        else if (raw === "0" || raw === "false") value = false;
      } else if (s.type === "choice") {
        if (s.options.includes(raw)) value = raw;
      } else {
        value = raw;
      }
    }
    values[s.name] = value;
  }
  return values;
}

const SETTINGS = settingsFrom(
  JSON.parse(document.getElementById("widget").textContent), params);
```

Connect to the API at the address the page came from (the port is the
streamer's choice), and reconnect when the connection drops:

```js
function connect() {
  const ws = new WebSocket(`ws://${location.host}/api/v1/ws`);
  ws.onmessage = (frame) => {
    const event = JSON.parse(frame.data);
    if (event.type === "message") {
      // event.author.name, event.text, event.emotes, event.kind: see the API docs
    }
  };
  ws.onclose = () => setTimeout(connect, 2000);
}
connect();
```

The events are described in the [API docs](api.md).

### The rules a widget runs under

chat-aggregator serves a widget with a Content-Security-Policy, as it does
its own overlay:

- **Only the page's own inline scripts run.** A `<script src="…">` makes
  the file fail the check, and inline event handlers (`onclick="…"`) are
  blocked. Put the code in `<script>` elements in the file.
- **Connections go back to chat-aggregator only** (`fetch`, WebSocket).
- **Styles** may be inline or come from `https://` addresses; **pictures,
  fonts, sounds and videos** from `https://` addresses or as `data:` URLs.
- At most 5 MB.

**Chat is untrusted**: chatters choose their names and messages. Never put
chat into the page as HTML (`innerHTML`); use `textContent`, or draw it on
a canvas. Emote pictures come as URLs in `emotes`.

### Trying it out

Import the file, then open its URL (from **Copy URL**) in a browser while
chat-aggregator runs with the JSON API on; a **Demo** source sends sample
messages. After an edit, change the file in the widgets folder (or import
it again) and reload the page. Fields the block has that this version of
chat-aggregator doesn't know are ignored, so a widget can carry extra
information for later versions.
