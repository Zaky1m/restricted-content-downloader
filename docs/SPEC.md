# Product specification

What the bot does. Where this file and the code disagree, this file is the
target and the difference is a bug.

## What it does

A Telegram bot that copies posts from channels and groups to the user, or to a
channel the user picks. A private or copy-restricted source needs a Telegram
account linked by the user that is already a member of the source.

## Users and plans

| | Free | Premium |
|---|---|---|
| Daily downloads | 25, plus gift and referral points | unlimited |
| Open tasks per user | 1 | 5, run one after another |
| Tasks running at once | 4 free users share the slots, the rest wait in one queue | not limited by other users |
| `/clone`, `/monitor` | no | yes |
| Upload size | 2 GiB, larger files are split into parts of up to 1990 MiB | linked premium Telegram account can upload up to 4000 MiB to the upload channel |

Premium plans are paid with Telegram Stars: monthly 30 days for 200 Stars,
yearly 365 days for 2000 Stars. Days are added to the time left. An admin can
also grant premium for 1, 3, 7, 30, 90, 365 days or a custom length.

Points are spent in this order: daily points, gift points (soonest expiry
first, no expiry last), referral points.
Referral: the inviter gets 200 points and the new user 50 (400 and 200 when
the inviter is premium). A referral link is `?start=<inviter user id>`.

Referral check: a new user who arrives through a referral link first answers
a simple addition question in their language. A correct answer deletes the
question and the answers, credits the referral, and shows the start message.
After three wrong answers the user is told the referral did not count, then
sees the start message and is registered without a referral. The question
waits at most 24 hours.

## Commands

| Command | Behavior |
|---|---|
| `/start` | Registers the user, shows the menu. Payloads: a user id (referral), `gift_<id>` (claim a gift), `ul_<id>` (confirm a chat link) |
| `/help` | Usage help per feature |
| `/profile` | Plan, points, expiry, upload channel, linked account |
| `/premium` | Plans and the Stars invoice |
| `/language` | Choose one of 30 languages |
| `/login` | Link a Telegram account by sharing the contact or typing an international number; the code is entered on digit buttons; the two-step password message is deleted after it is read; the step expires after 10 minutes |
| `/logout` | Removes the linked account |
| `/dl` or a link | Downloads one post: albums, comment links and paid media included |
| `/bdl` | Range download: first link, last link of the same channel, then content filters; both ends included |
| `/clone` | Copies a whole source channel after the filters are chosen (premium) |
| `/monitor` | Watches a channel for new posts and delivers them (premium); `/monitor <channel> on\|off` still works |
| `/uploadch`, `/rmuploadch` | Sets or removes the delivery channel by chat picker, link or id |
| `/forcedownload` | Toggle: always download and upload instead of copying |
| `/thumb` | Toggle a custom thumbnail, set by sending a photo |
| `/caption` | `keep_all`, `remove_all`, or `remove_links_users` (removes mentions, links, web addresses and e-mail addresses) |
| `/cancel [id]` | Lists open tasks or cancels one |
| `/stats` | Bot counters; server load only for owners and admins |

Owners are listed in the environment (`OWNER_IDS`). Owners add and remove
admins from `/admin`; admins cannot appoint admins. The `/admin` panel covers
premium grants, gifts, forced-subscription channels, broadcast, logs, user
export and locale upload. Older admin commands remain only where the panel
does not do the same job.

## Links

Accepted: `t.me` and `telegram.me`, with or without `www.` and `/s/`, public
`name/<id>`, private `c/<channel>/<id>`, topic links with one extra number,
`?comment=<id>` for a comment on a channel post, `?single`. A private id
`c/<n>` maps to dialog id `-1000000000000 - n`. Invite links
(`+hash`, `joinchat/hash`) are accepted for joining.

## Filters

`-sv` saves only the listed types, `-rm` skips them. Types: `videos`,
`photos`, `audio`, `files`, `text`, `other`. `--min <seconds>` and
`--max <seconds>` limit duration; unknown duration always passes. `--fdl` is
ignored.

## Tasks

Every task is a stored row with a number, its data and progress. One status
message per task shows queue position, progress and the result, and is reused
after a restart. Open tasks resume from the last saved progress. A download
that stops progressing for 90 seconds is restarted. Before a download the
disk space is checked with a 5 GiB reserve left for the rest of the server.
A file above the bot's upload limit asks the user to download and split it, or
cancel; the choice is stored for 24 hours.

## Monitoring

Public sources use a download account from the pool; private sources are
scanned every hour with the user's linked account. New posts after the moment
of activation are delivered in order, once each. A source unreachable for 24
checks in a row stops its monitor and the user is told the channel name.

## Adding the bot to a chat

A registered premium user is asked to confirm; the confirmation sets the chat
as the upload channel. Anyone else is told why and the bot leaves. The request
expires after 24 hours.

## Limits

One message per second and 20 per minute; 20 button presses per minute with
blocks that double on repeat up to 24 hours. Forced subscription is checked
once a day. At most 8 linked accounts are connected at once, closed after 5
minutes idle.

## Waiting states

Stored in the database, never in memory: login 10 minutes, filter choice 10
minutes, confirmations 10 minutes, broadcast draft 10 minutes, admin steps 24
hours, large-file choice 24 hours, chat-link confirmation 24 hours. Accounts,
sessions, subscriptions, gifts, tasks, monitors and payments do not expire.

## Languages and text

30 languages from `locales/`; the first fallback is English. Fixed English
text is shown in small caps; names, commands, links, numbers, HTML and
placeholders stay as written. Lines of multi-line text start with a
direction mark for the language. Sizes, speeds and times use the units of the
user's language (`units.*`), so Arabic shows `5.64 ميجا/ثانية` and `38 ثانية`.

## Buttons

A button carries the action and the record id. Anything larger is a database
row. A one-time approval is claimed atomically before it runs.
