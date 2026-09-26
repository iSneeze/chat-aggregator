---
title: Privacy policy
permalink: /privacy/
---

# Privacy policy

_Effective 2026-09-27._

chat-aggregator is a program that runs **on your own computer**. It has no
servers, no user accounts and no analytics. The developers never receive
any of your data. This page describes what the program does with data on
your computer.

## YouTube API Services

chat-aggregator uses **YouTube API Services**. By connecting YouTube you
agree to be bound by the
[YouTube Terms of Service](https://www.youtube.com/t/terms). Google's
handling of data is described in the
[Google Privacy Policy](https://policies.google.com/privacy).

You connect YouTube with your own Google Cloud project and log in with
Google. chat-aggregator asks only for the permission **"View your YouTube
account"** (`youtube.readonly`), which is read-only: it can't post, delete
or change anything on your channel.

## What data is accessed

From YouTube, with your permission:

- your channel's name, to confirm which account you logged in with;
- your live broadcasts (their ids, status and scheduled start time), to
  find the stream to follow;
- the live chat of your broadcasts, or of a video whose id you enter:
  messages, and for each message the author's public display name, channel
  id, profile picture link and badges, plus the details of Super Chats,
  Super Stickers, memberships and gifts.

From Twitch: the public chat of the channels you enter, read anonymously
without any login.

## What is stored, where, and for how long

- **Your YouTube login** (a refresh token) is stored in your operating
  system's credential store (Keychain on macOS, Credential Manager on
  Windows, Secret Service on Linux). If none is available, it's stored in a
  file in chat-aggregator's settings folder that only your user account can
  read. It stays there until you log out or revoke access (see below).
- **Your settings** (for example your Google project's client id and
  secret, an optional API key, channel names, overlay theme, a copy of your
  custom emoji list) are stored in chat-aggregator's settings folder on your
  computer.
- **Chat messages are not saved.** They're kept in memory while the program
  runs (the most recent ones, 20 by default, so a reloaded overlay isn't
  empty) and are gone when you close it.

## What is shared, and with whom

- **Nothing is sent to the developers or any third party.**
- chat-aggregator shows the chat in an overlay served on your own computer
  (`127.0.0.1`); other computers can't reach it. Programs running on your
  computer can read the same chat through its local API (and so can web
  pages open in your browser, because browsers allow pages to connect to
  your own computer). This only ever includes public chat.
- The program connects only to the platforms themselves: Google/YouTube (for
  login and chat) and Twitch (for chat). Emote and profile images in the
  overlay are loaded from Twitch's and YouTube's image servers by your
  streaming software.
- Your YouTube API usage is recorded by Google in **your own** Google Cloud
  project.

## Removing access and data

- Log out in chat-aggregator (**Log out** in the app's YouTube section,
  `--youtube-logout` in the command-line version): this deletes the stored
  login from your computer.
- Revoke chat-aggregator's access at any time in your Google Account, under
  third-party connections:
  [myaccount.google.com/connections](https://myaccount.google.com/connections).
- To remove everything, delete chat-aggregator's settings folder (for
  example `~/.config/chat-aggregator` on Linux).

## This website

This website is hosted on GitHub Pages and sets no cookies of its own.
GitHub may log visits as described in the
[GitHub Privacy Statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).

## Changes and contact

Changes to this policy are published on this page; its full history is
visible in the
[source repository](https://github.com/iSneeze/chat-aggregator/commits/master/docs/privacy.md).
Questions: open an issue at
[github.com/iSneeze/chat-aggregator/issues](https://github.com/iSneeze/chat-aggregator/issues).
