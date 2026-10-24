# Contributing

Thanks for helping. Please read `docs/SPEC.md` first: it is the source of truth
for product behaviour, and changes to it need a discussion in an issue.

## Build and test

```bash
cargo fmt --all
cargo clippy --all-targets --release -- -D warnings
cargo test --release
```

Tests that use PostgreSQL run when `TEST_DATABASE_URL` is set to a disposable
database (each test creates and drops its own schema). Locale files must keep
exactly the keys of `locales/en.json`; a test checks it.

## Rules

- Stable Rust, edition 2024, no `unsafe`
- No `unwrap()` or `expect()` outside tests and startup configuration parsing
- Comments are in English, short, and explain why rather than what
- User-facing text lives in the i18n files, never in code
- No secrets, personal data or deployment details in code, tests, docs or
  commit messages

## Commits

Small commits with one reason each, in the imperative mood ("Add the
large file question"). Run the three commands above before every commit.
