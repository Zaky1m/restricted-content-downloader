# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - Unreleased

### Added
- Download a single post, a range of posts, or clone a whole channel, with progress, resume after a restart and per-user queues
- Linked user accounts and a pool of download accounts, stored encrypted in PostgreSQL
- Premium plans paid with Telegram Stars, gifts, referral points and a referral check for new users
- Channel monitoring that forwards new posts, with type and duration filters
- Upload destinations: private chat, Saved Messages or a channel; whole files up to 4000 MiB through a Premium account
- Owner and admin panel, broadcasts, usage statistics and a log channel
- 30 languages that reload without a restart
- `rcd import` to bring over the sessions of a Pyrogram-based version of this bot, `rcd pool-login` to sign in download accounts, `rcd check` to validate the setup
