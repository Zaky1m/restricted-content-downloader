# Deployment

**English** · [العربية](deploy.ar.md)

## What you need

- A bot token from [@BotFather](https://t.me/BotFather)
- An API ID and API hash from [my.telegram.org](https://my.telegram.org)
- PostgreSQL 14 or newer (the Docker setup starts one for you)
- One or more Telegram accounts for the download pool. Use separate accounts,
  not your personal one
- A private channel or group for the error log, with the bot as an admin

The bot keeps an open connection to Telegram and receives updates over it. It
needs no public address, domain or open port.

## With Docker

```bash
git clone https://github.com/Zaky1m/restricted-content-downloader.git
cd restricted-content-downloader
cp .env.example .env
```

Fill in `.env` (see the table below), then:

```bash
docker compose up -d
docker compose run --rm bot rcd pool-login
docker compose logs -f bot
```

`pool-login` asks for the code Telegram sends to each number in `POOL_PHONES`
and stores the sessions in the database. To add one more account later run
`rcd pool-login --phone +12025550123`.

## On a server without Docker

Install Rust (stable), FFmpeg and PostgreSQL, then:

```bash
cargo build --release
cp .env.example .env    # fill in the values
./target/release/rcd check
./target/release/rcd pool-login
./target/release/rcd serve
```

To keep it running, copy [`deploy/rcd.service.example`](../deploy/rcd.service.example)
to `/etc/systemd/system/rcd.service`, adjust the user and paths, and run
`sudo systemctl enable --now rcd`. On `SIGTERM` the bot stops its tasks
cleanly, and they continue on the next start.

## Settings

| Variable | Required | Meaning |
|---|---|---|
| `API_ID`, `API_HASH` | Yes | Telegram application credentials |
| `BOT_TOKEN` | Yes | Token from @BotFather |
| `BOT_USERNAME` | Yes | Bot username without `@`, used in referral links |
| `OWNER_IDS` | Yes | Telegram user ids of the owners, comma separated |
| `ADMIN_USERNAME` | Yes | Support contact shown to users, without `@` |
| `LOG_CHANNEL_ID` | Yes | Chat that receives the error log |
| `DATABASE_URL` | Yes | PostgreSQL URL, or the separate `DB_*` variables |
| `SESSION_ENCRYPTION_KEY` | Yes | 64 hex characters, from `openssl rand -hex 32` |
| `POOL_PHONES` | No | Numbers of the download accounts for `pool-login` |
| `CHANNEL_LINK` | No | Link behind the Channel button; empty hides it |
| `START_IMAGE`, `ARCHIVE_CHANNEL_ID` | No | Welcome photo for `/start` and the channel it is stored in |
| `POSTGRES_PASSWORD` | Docker | Password of the database that compose starts |
| `DATA_DIR`, `LOCALES_DIR` | No | Default `data` and `locales` |

Keep `SESSION_ENCRYPTION_KEY` safe and never change it: the stored sessions
cannot be read without it.

## Moving from the Python version

`rcd import --sessions-dir <folder>` reads the sessions of the earlier
Pyrogram-based bot (the `.session` files and the `user_sessions` table),
encrypts them and stores them in the new tables, so users stay signed in. The
database schema is shared: point `DATABASE_URL` at the existing database, stop
the old bot and run the import once before `rcd serve`. Running it again is
safe.

## Updating

```bash
git pull
docker compose up -d --build
```

Database migrations run on start.
