# Decisions

Short entries: context, decision, why.

## Rust with grammers instead of Pyrogram
The bot needs both a bot account and user accounts over MTProto. grammers gives
both in one binary with no interpreter, and updates arrive over the open
connection, so there is no polling.

## Sessions stored encrypted in PostgreSQL
A Telegram session is a login. Keeping them in the database, encrypted with
XChaCha20-Poly1305 and a key from the environment, lets a backup of the data
folder or the database leak nothing useful on its own. `rcd import` converts
sessions created by a Pyrogram-based version of this bot without logging in again.

## Waiting states live in the database
Choices a user still has to make (confirm a channel, pick how to receive a big
file) are rows with an expiry of at most 24 hours. A restart does not lose them,
and a button can only be claimed once.

## Big files are the user's choice
A bot upload stops at about 2 GB. When a file is larger the user chooses between
numbered parts and a whole upload by their own Premium account, unless that
account already uploads to their channel.

## Custom downloader
Downloads use `upload.GetFile` in 512 KiB chunks with a stall guard and DC
migration handling, because the stock helper cannot resume a stalled transfer.
A 1 MiB answer can arrive wrapped in a container that grammers refuses, which
dropped the connection, so the chunk stays at 512 KiB.

## Custom uploader
Telegram slows uploads from accounts without Premium and answers with a wait.
The stock helper sends four parts at once and gives up on the whole file at the
second wait. Our uploader retries only the part that was refused and drops to
one part in flight once the account is slowed, so a large file still arrives.

## Usernames are cached for eight hours
Resolving a username is rate-limited hard. Each account remembers the usernames
it resolved for eight hours, so a range or a monitor does not resolve the same
source again for every post.

