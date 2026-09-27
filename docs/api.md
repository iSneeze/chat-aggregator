# Chat events API

Live chat from Twitch and YouTube, merged into one stream of JSON events,
for programs that want to react to chat: games, bots, sound boards, custom
overlays.

- **Endpoint:** `ws://127.0.0.1:7878/api/v1/ws` (WebSocket, port
  configurable)
- **Off by default:** switch it on in the app under **⚙ Settings →
  Connection → JSON API** (headless: `api = true` in `[server]`, or
  `--api`). Any web page open in the browser could connect to it, so it
  only listens when you want it to.
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
| `platform`  | `"twitch"`, `"youtube"` (later `"rplay"`) |
| `author`    | `id` (platform user id), `name`, `color` (`#rrggbb`, Twitch only, else `null`), `badges` (e.g. `moderator`, `subscriber`, `member`, `vip`), `avatar_url` (YouTube only, else `null`) |
| `text`      | what the user typed; **may be empty** (e.g. a raid). Event descriptions are in `kind` |
| `emotes`    | emotes used in `text`, each listed once; in `text` they appear exactly as `code` |
| `timestamp` | when it was sent, RFC 3339 in UTC |
| `kind`      | what kind of message, see below |

`kind.type` and its extra fields:

| `kind.type`  | fields | examples |
|--------------|--------|----------|
| `text`       | | normal chat |
| `emote_only` | | only emotes (and spaces) |
| `donation`   | `amount` | Twitch bits, YouTube Super Chat: `"100 bits"`, `"€5.00"` |
| `special`    | `image_url`, `amount`, `info` (each may be `null`) | YouTube Super Sticker (`info` = sticker description), YouTube gift |
| `membership` | `info`, `months` | new sub/member, resub, milestone; `months` = how long they've been a member (resub, milestone), `null` for a new one |
| `gift`       | `count` | gifted subs (Twitch) or memberships (YouTube) |
| `notice`     | `info` | raid, announcement |

`amount` is a display string as the platform formats it (currency and all);
it's meant for showing, not for arithmetic.

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
Any program on the same machine can, and so can websites open in the
streamer's browser (browsers allow pages to connect to `ws://localhost`).
That's fine for this read-only feed of public chat, and it means browser
games can use it too. It's off by default for that reason.

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
