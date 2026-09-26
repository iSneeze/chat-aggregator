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

Google's menus move around from time to time; the section is called
**Google Auth Platform** (older name: *OAuth consent screen*).

1. **Get started**: app name, e.g. `chat-aggregator`, and your email as the
   support address. **Audience**: choose **External**. Add your contact
   email and finish.
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
3. **Audience**: click **Publish app** so the status becomes **In
   production**.

   Why: while an app is in *Testing*, Google makes logins expire after
   **7 days**, and you'd have to log in again every week. Publishing does
   **not** require Google's verification for an app only you use; Google
   just shows a warning when you log in (see step 6).

   Afterwards Google shows a yellow banner: *"Your app requires
   verification … please submit your app for review."* **Ignore it; don't
   submit.** Verification is meant for apps used by many strangers (it asks
   for a demo video and domain ownership). Unverified, your app works fine
   for up to 100 accounts; you'll just see the warning at login.

**Just trying it out?** You can skip branding and publishing: stay in
*Testing* and add the Google account that owns your YouTube channel under
**Audience → Test users** (without that, Google refuses the login). You'll
have to log in again every 7 days.

## 4. Create the OAuth client

1. **Clients → Create client**.
2. Application type: **Desktop app**. Name: anything, e.g.
   `chat-aggregator desktop`.
3. **Create**, then copy the **Client ID** and the **Client secret** right
   away (download the JSON too): Google may not show the secret again later.

It must be the *Desktop app* type: other types don't accept the local login
address the app uses (`http://127.0.0.1:<port>`).

The client secret of a desktop app isn't a real secret in Google's eyes
(any installed program could be taken apart to find it), but don't post it
publicly either.

## 5. Tell chat-aggregator

Until the settings window exists, via environment variables:

```sh
export YOUTUBE_CLIENT_ID="1234567890-abc….apps.googleusercontent.com"
export YOUTUBE_CLIENT_SECRET="GOCSPX-…"
```

## 6. Log in

```sh
cargo run -p chat-engine --example run -- --youtube-login
```

Your browser opens Google's login page:

1. Pick the Google account that owns your **YouTube channel**.
2. Google warns **"Google hasn't verified this app"**. That's expected for
   your own unpublished-to-the-world app: **Advanced → Go to
   chat-aggregator (unsafe)**.
3. Allow **"View your YouTube account"** (read-only access).
4. The tab says *Connected to YouTube*; the terminal prints
   `logged in to YouTube as "<your channel>"`.

The login is stored in your system's keyring (Keychain on macOS,
Credential Manager on Windows, Secret Service such as gnome-keyring or
KWallet on Linux; without one, a file only you can read in the app's config
folder). You only do this once.

## 7. Go live

```sh
cargo run -p chat-engine --example run -- --youtube-own
```

chat-aggregator finds your current or next broadcast by itself (including
unlisted and members-only streams), waits cheaply until it starts, and
attaches to its chat. After the stream it goes back to waiting for the next
one.

To log out (e.g. to switch channels):
`cargo run -p chat-engine --example run -- --youtube-logout`.

## Quota: keeping an eye on it

- Current usage: **APIs & Services → YouTube Data API v3 → Quotas**. The
  daily quota resets at **midnight Pacific time** (09:00 in Central Europe).
- If you stream more than ~5 hours a day, request more quota for free via
  YouTube's [quota extension and compliance
  audit](https://developers.google.com/youtube/v3/guides/quota_and_compliance_audits).
  Don't create several projects to add up quota: that's against YouTube's
  API terms.

## Troubleshooting

| message | cause and fix |
|---------|---------------|
| `YOUTUBE_CLIENT_ID must be set` | step 5 missing in this terminal |
| Google shows `redirect_uri_mismatch` or `invalid_request` | the client isn't of type **Desktop app** (step 4) |
| `invalid_client` | client ID or secret mistyped, or from another project |
| `YouTube login required: Google rejected the stored login` | the login expired or was revoked. If it happens weekly, the app is still in *Testing* (step 3). Log in again (step 6). |
| `this Google account has no YouTube channel` | you logged in with a different Google account than the channel's |
| `quotaExceeded` / `ResourceExhausted` | today's quota is used up; it resets at midnight Pacific time |
