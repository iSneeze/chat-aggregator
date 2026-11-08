# Chat events API

Live chat from Twitch, YouTube and Picarto (and StreamElements and Streamlabs tips), merged into one stream of JSON events,
for programs that want to react to chat: games, bots, sound boards, custom
overlays.

- **Endpoint:** `ws://127.0.0.1:7878/api/v1/ws` (WebSocket, port
  configurable)
- **Off by default:** switch it on in the app under **⚙ Settings →
  Connection → JSON API** (headless: `api = true` in `[server]`, or
  `--api`). Even when it's on, a web site only gets in once the streamer
  allows it (see [Security](#security)).
- **Direction:** the server pushes one event per text frame; anything the
  client sends is ignored.
- **Formal spec:** [asyncapi.yaml](asyncapi.yaml) (AsyncAPI 3.0) with the
  payload schema [schema/chat-event.json](schema/chat-event.json) (JSON
  Schema draft 7, generated from the code).

Try it without writing code (the dev shell includes `websocat`):

```sh
cargo run -p chat-engine --example run -- --demo --api
websocat ws://127.0.0.1:7878/api/v1/ws
```

## Connecting

| query            | effect |
|------------------|--------|
| _(none)_         | live events only, from the moment you connect |
| `?history=true`  | first the most recent messages (default: up to 20), then live |

History is off by default on purpose: a game that reacts to `!jump` must not
replay old commands every time it reconnects. Use `?history=true` if you
*display* chat.

The server pings idle connections every 30 seconds (WebSocket libraries
answer automatically) and sends a Close frame with code `1001` when it shuts
down or the API is switched off (the reason says which). While it's
switched off, connecting fails with HTTP `403 Forbidden`. Reconnect when the
connection drops: nothing is lost while you're connected, but events that
happen while you're disconnected are not queued.

## Events

Every event is a JSON object with a `type` field.

### `message`

A chat message or a platform event (donation, sub, raid, ...).

```json
{
  "type": "message",
  "id": "LCC.ExampleMessageId0001",
  "platform": "youtube",
  "author": {
    "id": "UCxxxxxxxxxxxxxxxxxxxxxx",
    "name": "Viewer42",
    "color": null,
    "badges": ["member"],
    "avatar_url": "https://yt3.ggpht.com/…"
  },
  "text": ":_hypeWave:",
  "emotes": [{ "code": ":_hypeWave:", "url": "https://yt3.ggpht.com/…" }],
  "timestamp": "2026-09-25T18:40:44.516Z",
  "kind": { "type": "emote_only" }
}
```

| field       | meaning |
|-------------|---------|
| `id`        | platform message id; `delete` events refer to it |
| `platform`  | `"twitch"`, `"youtube"`, `"picarto"`, `"streamelements"`, `"streamlabs"` (tips: every message is a `donation`), later `"rplay"` |
| `author`    | `id` (platform user id), `name`, `color` (`#rrggbb`, Twitch and Picarto, else `null`), `badges` (e.g. `broadcaster`, `moderator`, `subscriber`, `member`, `vip`), `avatar_url` (YouTube and Picarto, else `null`) |
| `text`      | what the user typed; **may be empty** (e.g. a raid). Event descriptions are in `kind` |
| `emotes`    | emotes used in `text`, each listed once; in `text` they appear exactly as `code` |
| `timestamp` | when it was sent, RFC 3339 in UTC; fractional seconds only as precise as needed (`18:41:02Z`, `18:40:44.5Z`, `18:40:44.516Z`), so parse it rather than slicing it |
| `kind`      | what kind of message, see below |

`kind.type` and its extra fields:

| `kind.type`  | fields | examples |
|--------------|--------|----------|
| `text`       | | normal chat |
| `emote_only` | | only emotes (and spaces) |
| `donation`   | `amount`, `tier`, `value`, `currency` | Twitch bits, YouTube Super Chat, StreamElements or Streamlabs tip: `"500 bits"`, `"€5.00"`, `"10"` |
| `special`    | `image_url`, `amount`, `info`, `tier`, `value`, `currency` (each may be `null`) | YouTube Super Sticker (`info` = sticker description), YouTube gift |
| `membership` | `info`, `months` | new sub/member, resub, milestone; `months` = how long they've been a member (resub, milestone), `null` for a new one |
| `gift`       | `count` | gifted subs (Twitch) or memberships (YouTube) |
| `notice`     | `info` | raid, announcement, watch streak, new chatter, … (Twitch events without a kind of their own) |

`amount` is a display string as the platform formats it (currency and all);
it's meant for showing. StreamElements doesn't send the currency: a tip
shows in the currency the streamer set in the app (`"€10.00"`), or as a
plain number (`"10"`, `"2.50"`) if they set none. Streamlabs does: its tips
show in the currency Streamlabs reports.

For arithmetic there's `value`, the amount as a number in the platform's
unit, and `currency`, its ISO code (`null` for none):

| platform | `value` | `currency` |
|----------|---------|------------|
| `twitch` | the bits (`500`) | `null` |
| `youtube` | the Super Chat's or Sticker's amount (`5.0` for €5.00); a gift's jewels | the viewer's currency (`"EUR"`); `null` for jewels |
| `streamelements` | the tip's amount (`10.0`) | the streamer's tip currency, if they set it |
| `streamlabs` | the tip's amount (`10.0`) | the currency Streamlabs reports (`"EUR"`) |

Nothing is converted: compare amounts only within one currency (or one
platform). `value` and `currency` may be `null` (test messages, unknown
amounts).

`tier` says how big the amount is, from 1 (the smallest) to 7, or `null`
where there's none (YouTube jewel gifts). **One scale for every platform**,
YouTube's Super Chat tiers, so "tier 4 or more" means about the same
everywhere:

| platform | tiers |
|----------|-------|
| `youtube` | YouTube's Super Chat/Sticker tier, as YouTube reports it, whatever the currency: 1 ($1+, blue), 2 ($2+, cyan), 3 ($5+, teal), 4 ($10+, yellow), 5 ($20+, orange), 6 ($50+, magenta), 7 ($100+, red). Its top band may report more than 7 |
| `twitch`  | YouTube's dollar steps × 100 bits: 1 (1–199), 2 (200+), 3 (500+), 4 (1,000+), 5 (2,000+), 6 (5,000+), 7 (10,000+) |
| `streamelements` | where the streamer set their tiers to start, in their tip currency; by default YouTube's steps: 1 (below 2), 2 (2+), 3 (5+), 4 (10+), 5 (20+), 6 (50+), 7 (100+) |
| `streamlabs` | the same tiers as `streamelements`; a tip in another currency than the streamer's tip currency is 1 (nothing is converted, so its worth is unknown) |

(Until 2026-11, Twitch bits had their own scale, 1–5 by Twitch's cheer
colours.)

### `delete`, `clear_user`, `clear_all`

Moderation: remove messages you already showed.

```json
{ "type": "delete",     "platform": "twitch",  "message_id": "7c4e9a61-…" }
{ "type": "clear_user", "platform": "youtube", "user_id": "UCxxxxxxxx" }
{ "type": "clear_all",  "platform": "twitch" }
```

- `delete`: one message, matched by `platform` + `id`.
- `clear_user`: a user was banned or timed out; remove every message whose
  `platform` + `author.id` match.
- `clear_all`: remove every message of that platform.

Always match on the platform too: ids of different platforms are unrelated.

### `lagged`

```json
{ "type": "lagged", "missed": 12 }
```

Your client read too slowly and fell behind the server's buffer (256
events); `missed` events were dropped for you. Live events continue right
after it. Only happens if your program blocks while handling events.

## Compatibility rules

- **Ignore `type`s and fields you don't know.** New event types, kind types
  and fields may be added within `v1`.
- Renaming or removing anything means a new version: `/api/v2/...`.

## Security

The server only listens on `127.0.0.1`, so other machines can't connect.
On the same machine, the API accepts:

- **programs**: bots, game engines, scripts (anything that isn't a
  browser sends no `Origin` header);
- **pages served from this computer**, e.g. a browser game you run with
  `python -m http.server` (`http://localhost:<any port>`), and
  chat-aggregator's own [widgets](widgets.md);
- **OBS browser sources**, including ones set to "Local file";
- **web sites the streamer allowed**, e.g. a browser game hosted on
  `https://game.example`.

Browsers let any page open a WebSocket to `localhost`, but they always send
the page's `Origin`, and pages can't fake it. Without a check, any website
open in the streamer's browser could read their chat, including
members-only chat, and learn from it which channel is theirs. So when a web
site connects for the first time, the app asks the streamer:

> **Let https://game.example read your chat?** [Allow] [Deny]

- **While the question is open, the connection waits.** On "Allow" it goes
  through as it is: the game doesn't have to reconnect. It just takes a
  little longer the first time.
- **"Allow" is remembered** (`api_origins` in `config.toml`); the site can
  be removed again under ⚙ Settings → Connection, which also disconnects
  it.
- **"Deny" is remembered until the app restarts.** A denied site, or one
  nobody answered within 2 minutes, gets `403` with a message saying why.
- **Headless mode** has nobody to ask: only the sites in `api_origins`
  get in.
- Nobody is asked while the API is switched off.

A page opened straight from disk in a desktop browser is refused without
asking: browsers send `Origin: null` for it, and so can any website (from a
sandboxed frame), so the question couldn't say who is asking. Open it
through OBS, or serve it from localhost.

The overlay and the API also refuse any request whose `Host` isn't
`127.0.0.1`, `localhost` or `[::1]`. That stops "DNS rebinding", where a
site points its own domain at `127.0.0.1` to read the server's answers.

**Chat is untrusted input.** Every text field (`text`, `author.name`,
amounts, descriptions, emote codes) is exactly what someone typed or sent,
unescaped. If you show it in a web page or an OBS browser source, insert it
as **text** (`element.textContent = …`), never as HTML (`innerHTML`,
`insertAdjacentHTML`, template strings put into the page). An overlay that
did exactly that let chatters run code on streamers' PCs through OBS
([CVE-2024-7971](https://cyberinsider.com/malicious-twitch-chat-messages-can-trigger-code-execution-on-obs-studio/)):
OBS runs its browser sources without Chromium's sandbox. The same goes for
URLs (`url`, `avatar_url`): use them as image sources only.

## Example clients

### JavaScript (browser, Node 22+, Deno, Bun)

```js
function connect() {
  const ws = new WebSocket("ws://127.0.0.1:7878/api/v1/ws");
  ws.onmessage = (frame) => {
    const event = JSON.parse(frame.data);
    switch (event.type) {
      case "message":
        console.log(`[${event.platform}] ${event.author.name}: ${event.text}`);
        if (event.kind.type === "donation") console.log(`  donated ${event.kind.amount}!`);
        break;
      case "delete":
        console.log(`remove message ${event.message_id}`);
        break;
      case "lagged":
        console.warn(`missed ${event.missed} events`);
        break;
      // other types: ignore
    }
  };
  ws.onclose = () => setTimeout(connect, 2000); // reconnect
}
connect();
```

### Python (`pip install websockets`)

```python
import asyncio, json
import websockets

async def main():
    # Iterating over connect() reconnects automatically when the connection drops.
    async for ws in websockets.connect("ws://127.0.0.1:7878/api/v1/ws"):
        try:
            async for frame in ws:
                event = json.loads(frame)
                if event["type"] == "message" and event["text"].startswith("!jump"):
                    print(f"{event['author']['name']} wants to jump!")
        except websockets.ConnectionClosed:
            continue

asyncio.run(main())
```
