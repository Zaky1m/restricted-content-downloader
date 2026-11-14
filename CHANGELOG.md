# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - Unreleased

### Changed
- grammers 0.12 (Telegram layer 233): posts that use newer Telegram objects are read instead of failing a whole batch chunk
- A dropped Telegram connection is retried on the same update stream instead of restarting the bot; pool accounts keep watching their channels
- The "task resumed" notice is sent once per task, and a batch dropped after repeated restarts tells its owner and pays back its unsent posts
- A download whose file reference expired reads the post again and continues
- A batch post that could not be delivered counts as failed, not skipped, so it is retried and paid back; a post with nothing to send is skipped
- A `?single` link downloads that one album item, not the whole album
- A large file question left unanswered for a day ends the album or batch instead of asking again for the next file; the rest of a batch is paid back
- A single download whose linked account was logged out says so and unlinks it, instead of retrying and failing without a reason

### Added
- A localized JSON broadcast can carry a `_button`: a link button with a label per language and a color
- Download a single post, a range of posts, or clone a whole channel, with progress, resume after a restart and per-user queues
- Linked user accounts and a pool of download accounts, stored encrypted in PostgreSQL
- Premium plans paid with Telegram Stars, gifts, referral points and a referral check for new users
- Channel monitoring that forwards new posts, with type and duration filters
- Upload destinations: private chat, Saved Messages or a channel; whole files up to 4000 MiB through a Premium account
- Owner and admin panel, broadcasts, usage statistics and a log channel
- 30 languages that reload without a restart
- `rcd import` to bring over the sessions of a Pyrogram-based version of this bot, `rcd pool-login` to sign in download accounts, `rcd check` to validate the setup
