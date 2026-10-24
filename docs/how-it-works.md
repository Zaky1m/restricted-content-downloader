# How it works

**English** · [العربية](how-it-works.ar.md)

## Which account reads the source

A bot cannot read a channel it is not a member of, and Telegram stops bots and
most clients from forwarding posts in a channel that restricts saving. RCD
reads posts through a normal Telegram account instead and sends the media
again as a new message.

- Public sources are read by a pool of download accounts that the owner
  signs in with `rcd pool-login`. The bot picks one for each task
- Private sources need an account that is already a member. The user links
  their own account with `/login`, and the bot uses it only for that user's
  tasks

Every session is stored in PostgreSQL, encrypted with a key from the
environment. A copy of the database alone does not give access to any account.

## Tasks and the queue

Each link, range or clone becomes a task with a number. One status message per
task shows its place in the queue, then its progress, then the result.

- A free user has one open task, and free users share four running slots
- A Premium user can have five open tasks, which run one after another
- After a restart every open task continues from the last post it delivered
- A download that stops moving for 90 seconds is restarted
- `/cancel` lists open tasks and stops the one you choose

## Delivery

When a post can be copied, the bot copies it. Otherwise it downloads the file,
uploads it again with the caption and thumbnail settings of the user, and
deletes the local copy. `/forcedownload` always takes the second path.

Bots can upload up to 2 GB. For a larger file the bot asks the user to choose:

- Parts: the file is split into numbered parts of up to 1990 MiB
- Whole: a linked Premium account uploads the whole file, up to 4000 MiB,
  to the user's channel

Before downloading, the bot checks free disk space and keeps 5 GiB in reserve
for the rest of the server.

## Watching a channel

`/monitor` delivers every post published after it is turned on, once each and
in order. Public channels are followed live through a pool account. Private
channels are checked every hour with the user's linked account. A channel that
stays unreachable for 24 checks in a row is dropped and the user is told which
one.

## Filters

A range, clone or monitor can keep only some types (`-sv`) or skip them
(`-rm`): videos, photos, audio, files, text and other. `--min` and `--max`
limit video and audio length in seconds.

## Waiting for an answer

When the bot waits for a choice (a phone code, a filter, a confirmation), the
wait is a database row with an expiry between 10 minutes and 24 hours, so a
restart does not lose it and a button cannot be used twice.
