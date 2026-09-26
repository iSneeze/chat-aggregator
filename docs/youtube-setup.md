# Connecting YouTube

To read your YouTube live chat, chat-aggregator logs in to your YouTube
account. Because of how YouTube's API is billed, you need your **own**
(free) Google Cloud project for that. This takes about 10 minutes, once.

## Why your own project?

YouTube counts API usage ("quota") per Google Cloud project, not per user.
A project gets **10,000 units per day** for free. Reading live chat costs
about **1,500–1,800 units per hour of streaming**: YouTube currently cuts
the chat connection every ~10 seconds (a known bug on Google's side, with an
open ticket), and every reconnect costs 5 units. So one project covers
roughly **5–6 hours of streaming per day**, and a shared project for all
users couldn't even cover one channel. With your own project, your quota is
yours alone.

Checking whether you're live while you're offline is cheap (1 unit every
30 seconds to 5 minutes), so leaving the app running all day is fine.

## 1. Create a project

1. Open the [Google Cloud Console](https://console.cloud.google.com/) and
   sign in with any Google account (it doesn't have to be the YouTube one).
2. Project picker at the top → **New project** → name it, e.g.
   `chat-aggregator` → **Create**. Select the new project.

No billing account or payment method is needed.

## 2. Enable the YouTube Data API

**APIs & Services → Library** → search **YouTube Data API v3** → **Enable**.

## 3. Set up the consent screen

This is the page Google shows when you log in. Google's menus move around
from time to time; the section is called **Google Auth Platform** (older
name: *OAuth consent screen*).

1. **Get started**: app name, e.g. `chat-aggregator`, and your email as the
   support address. **Audience**: choose **External**. Add your contact
   email, agree to Google's policy and **Create**.
2. **Branding**: to be allowed to publish (next step), Google needs a
   homepage, a privacy policy and their domain. Use chat-aggregator's:

   | field | value |
   |-------|-------|
   | Application home page | `https://isneeze.github.io/chat-aggregator/` |
   | Application privacy policy link | `https://isneeze.github.io/chat-aggregator/privacy/` |
   | Application terms of service link | leave empty |
   | Authorized domains | `isneeze.github.io` |

   **Don't upload a logo**: with a logo, Google requires a full app
   verification. Click **Save** at the bottom.
3. **Audience**: click **Publish app** and confirm, so the status becomes
   **In production**.

   Why: while an app is in *Testing*, Google makes logins expire after
   **7 days**, and you'd have to log in again every week. Publishing does
   **not** require Google's verification for an app only you use: Google
   just shows a warning when you log in (step 5), and allows up to 100
   accounts.

   Afterwards Google shows a yellow banner: *"Your app requires
   verification … please submit your app for review."* That's because
   reading YouTube counts as a *sensitive* permission. **Ignore it; don't
   submit.** Verification is meant for apps used by many strangers (it asks
   for a demo video and domain ownership).

**Just trying it out?** You can skip branding and publishing: stay in
*Testing* and add the Google account that owns your YouTube channel under
**Audience → Test users** (without that, Google refuses the login). You'll
have to log in again every 7 days.

You don't need to add anything under **Data Access** (scopes):
chat-aggregator asks for its one permission, read-only access to your
YouTube account, when you log in.

## 4. Create the OAuth client

1. **Clients → Create client**.
2. Application type: **Desktop app**. Name: anything, e.g.
   `chat-aggregator desktop`.
3. **Create**. The next dialog shows the **Client ID** and the **Client
   secret**: copy both, or **Download JSON**. **This is the only time Google
   shows the secret**; later you only see its last four characters. Lost
   it? Open the client and add a new secret (then delete the old one).

It must be the *Desktop app* type: other types don't accept the local login
address the app uses (`http://127.0.0.1:<port>`).

The client secret of a desktop app isn't a real secret in Google's eyes
(any installed program could be taken apart to find it), but don't post it
publicly either.

## 5. Connect in the app

1. Start chat-aggregator. The **YouTube** section of the main window says
   *Not set up yet*: paste the **Client ID** and **Client secret** and click
   **Save**. (They're saved in `config.toml` in chat-aggregator's settings
   folder, readable only by your user account.)
2. Click **Connect YouTube**. Your browser opens Google's login page:
   1. Pick the Google account that owns your **YouTube channel**.
   2. Google warns **"Google hasn't verified this app"**. That's expected
      for your own app: **Advanced → Go to chat-aggregator (unsafe)**.
   3. Allow **"View your YouTube account"** (read-only access).
   4. The tab says *Connected to YouTube*; close it.
3. The app shows **Connected as "*your channel*"**.

The login is stored in your system's keyring (Keychain on macOS,
Credential Manager on Windows, Secret Service such as gnome-keyring or
KWallet on Linux; without one, a file only you can read in the settings
folder). You only do this once. **Log out** in the YouTube section (e.g. to
switch channels) removes it.

## 6. Add your broadcasts as a source

Under **Add a source**, choose **YouTube (your broadcasts)** → **Add**.

chat-aggregator finds your current or next broadcast by itself (including
unlisted and members-only streams), waits cheaply until it starts, and
attaches to its chat. After the stream it goes back to waiting for the next
one. The source's light: 🟢 working (or waiting for your broadcast), 🟡
recovering by itself, 🔴 no chat until something changes (e.g. log in
again, or the daily quota runs out; the text next to it says which), ⚪
switched off.

To follow someone else's stream instead: **YouTube (a video)** with the
video's id (the part after `watch?v=` in its URL). It uses your login too.

### Optional: an API key

Only needed to read a video's chat *without* logging in. Same project:
**APIs & Services → Credentials → Create credentials → API key**. Then
edit the key: under **API restrictions** choose **Restrict key** and tick
only **YouTube Data API v3**, so the key is useless for anything else. In
the app, paste it into **⚙ Settings → Connection → YouTube API key**. It
uses the same project's quota.

## Without the app (headless)

The same settings work from the terminal. Create a commented config file
once:

```sh
cargo run -p chat-engine --example run -- --init-config
```

It goes into chat-aggregator's settings folder
(`~/.config/chat-aggregator/` on Linux, `~/Library/Application
Support/chat-aggregator/` on macOS, `%APPDATA%\chat-aggregator\` on
Windows). Fill in the `[youtube]` section and add your broadcasts:

```toml
[youtube]
client_id = "1234567890-abc….apps.googleusercontent.com"
client_secret = "GOCSPX-…"

[[sources]]
type = "youtube"
```

(The environment variables `YOUTUBE_CLIENT_ID` and `YOUTUBE_CLIENT_SECRET`
override the file.) Then log in once, and start:

```sh
cargo run -p chat-engine --example run -- --youtube-login    # as in step 5
cargo run -p chat-engine --example run                       # everything in config.toml
cargo run -p chat-engine --example run -- --youtube-logout   # to log out
```

The terminal shows the same status lights per source.

## Quota: keeping an eye on it

- Current usage: **APIs & Services → Enabled APIs & services → YouTube
  Data API v3 → Quotas & System Limits**. The daily quota resets at
  **midnight Pacific time** (09:00 in Central Europe).
- When it's used up, the YouTube source turns red with *"YouTube quota used
  up for today; resuming at …"* and continues by itself after the reset.
- If you stream more than ~5 hours a day, request more quota for free via
  YouTube's [quota extension and compliance
  audit](https://developers.google.com/youtube/v3/guides/quota_and_compliance_audits).
  Don't create several projects to add up quota: that's against YouTube's
  API terms.

## Troubleshooting

| message | cause and fix |
|---------|---------------|
| Google shows `redirect_uri_mismatch` or `invalid_request` | the client isn't of type **Desktop app** (step 4) |
| `invalid_client` | client ID or secret mistyped, or from another project: **Change client** in the YouTube section |
| `access_denied` / "app is being tested" | the app is still in *Testing* and your account isn't a test user (step 3) |
| `YouTube login required: Google rejected the stored login` | the login expired or was revoked. If it happens weekly, the app is still in *Testing* (step 3). **Connect YouTube** again. |
| `this Google account has no YouTube channel` | you logged in with a different Google account than the channel's: **Log out**, connect again with the right one |
| `YouTube quota used up for today; resuming at …` | the daily quota is used up; the source continues by itself after midnight Pacific time |
| `YOUTUBE_CLIENT_ID must be set` (headless) | the `[youtube]` section of `config.toml` is missing |
