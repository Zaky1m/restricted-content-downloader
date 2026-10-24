# Translating

All user-facing text lives in `locales/<code>.json`. English (`en.json`) is the
reference: every other file must have exactly the same keys, and a test fails
when one is missing or extra.

## Fix a line

Open the file of the language, find the key and change the text. Keep the
placeholders such as `{count}` and the HTML tags (`<b>`, `<code>`) unchanged;
only their position may move.

## Add a language

1. Copy `locales/en.json` to `locales/<code>.json` (a language code such as `fr`
   or `pt-br`).
2. Translate the values, never the keys.
3. Run `cargo test --release`.

The bot loads the folder at start and with the owner's reload command, so no
code change is needed.

## Text rules

- English text that is shown as is (command names, usernames, links) stays as it is
- Keep the short, direct tone of the English text
- Right-to-left languages need no special markup
- `units` holds the size and time units (`{n} MB`, `{n}s`). Write them in the
  language's own script so a right-to-left line does not mix in Latin letters,
  and keep `{n}`

## Without a pull request

Open the translation issue form and paste the translated text or the line to fix.
