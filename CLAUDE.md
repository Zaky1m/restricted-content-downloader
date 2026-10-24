# CLAUDE.md

Guidance for AI agents and contributors working on this repository.

## What this is

A Telegram bot that copies posts from channels and groups to a user, or to a
channel the user picks. One Rust binary (`rcd`) talks MTProto with grammers,
keeps all durable state in PostgreSQL and uses linked user accounts and a pool
of download accounts for sources the bot cannot read itself.

## Read before you change anything

1. `docs/SPEC.md`: product behaviour. This is the source of truth.
2. `docs/DECISIONS.md`: why things are the way they are. Append to it when you
   make a non-obvious choice.
3. `CLAUDE.local.md`, if present: deployment notes for one specific server. It
   is never committed.

## Commands

```bash
cargo fmt --all
cargo clippy --all-targets --release -- -D warnings
cargo test --release
cargo run --release -- check        # validates .env, database and locales
cargo run --release -- serve        # needs a filled .env (see .env.example)
```

## Code rules

- Stable Rust, edition 2024. No `unsafe`.
- No `unwrap()` or `expect()` outside tests and startup config parsing. Return
  errors with context (`anyhow::Context`) in binaries and typed errors
  (`thiserror`) inside library modules.
- Comments are English, short, and say why, not what.
- Name things by domain meaning, never `utils`, `helper`, or `manager`.
- One concern per file. Prefer plain functions that are easy to unit-test.
- All durable state lives in PostgreSQL. In-memory caches only for data that
  can be rebuilt from the database.
- Money and reward actions are recorded atomically and idempotently before
  they take effect.
- Updates are pushed over the open Telegram connection. Never poll.
- Logs, errors and CLI output are English. User-facing text comes from the
  i18n files.
- No secrets in code, tests, fixtures, docs or commit messages.

## Commits

Small commits, one reason each, imperative mood. Run fmt, clippy and tests
before every commit.
