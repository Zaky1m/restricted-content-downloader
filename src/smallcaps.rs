//! Fixed English text is shown in small caps. Names, commands, links, numbers,
//! HTML and placeholders stay as written. Conversion happens at display time.

use std::sync::OnceLock;

use regex::Regex;

const LATIN: &str = "abcdefghijklmnopqrstuvwxyz";
const SMALL: &str = "ᴀʙᴄᴅᴇꜰɢʜɪᴊᴋʟᴍɴᴏᴘǫʀѕᴛᴜᴠᴡxʏᴢ";

fn convert(text: &str) -> String {
    let small: Vec<char> = SMALL.chars().collect();
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match LATIN.find(c) {
            Some(i) => small[i],
            None => c,
        })
        .collect()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Converts everything outside `<code>`, `<pre>`, tags, `{placeholders}`,
/// links, `/commands` and HTML entities.
pub fn small_caps(text: &str) -> String {
    static PROTECTED: OnceLock<Regex> = OnceLock::new();
    let protected = PROTECTED.get_or_init(|| {
        Regex::new(r"<code>[\s\S]*?</code>|<pre>[\s\S]*?</pre>|<[^>]+>|\{[^{}]+\}|https?://[^\s<]+|/[a-z][a-z0-9_]*|&[A-Za-z#0-9]+;")
            .expect("static pattern")
    });
    let mut out = String::with_capacity(text.len());
    let mut plain_from = 0;
    let mut search_from = 0;
    while let Some(found) = protected.find_at(text, search_from) {
        let is_command = text[found.start()..].starts_with('/');
        let after_word = text[..found.start()]
            .chars()
            .next_back()
            .is_some_and(is_word);
        if is_command && after_word {
            // "a/b" is not a command: look again one character later.
            search_from = found.start() + 1;
            continue;
        }
        out.push_str(&convert(&text[plain_from..found.start()]));
        out.push_str(found.as_str());
        plain_from = found.end();
        search_from = found.end();
    }
    out.push_str(&convert(&text[plain_from..]));
    out
}

/// Direction mark at the start of every line of a multi-line text, so mixed
/// scripts keep the language's direction.
pub fn mark_direction(language: &str, text: &str) -> String {
    if !text.contains('\n') && !text.contains('⌯') {
        return text.to_string();
    }
    let base = language.split('-').next().unwrap_or(language);
    let mark = if matches!(base, "ar" | "fa" | "he" | "iw" | "ur") {
        '\u{200f}'
    } else {
        '\u{200e}'
    };
    text.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!(
                    "{mark}{}",
                    line.trim_start_matches(['\u{200e}', '\u{200f}'])
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_plain_text_only() {
        assert_eq!(small_caps("Hello World"), "ʜᴇʟʟᴏ ᴡᴏʀʟᴅ");
        assert_eq!(small_caps("Use /dl and /help now"), "ᴜѕᴇ /dl ᴀɴᴅ /help ɴᴏᴡ");
        assert_eq!(
            small_caps("Send <code>Link</code> to {name} at https://t.me/x &amp; Go"),
            "ѕᴇɴᴅ <code>Link</code> ᴛᴏ {name} ᴀᴛ https://t.me/x &amp; ɢᴏ"
        );
        assert_eq!(small_caps("<b>Bold</b> Text"), "<b>ʙᴏʟᴅ</b> ᴛᴇxᴛ");
        assert_eq!(small_caps("Task #12: Done"), "ᴛᴀѕᴋ #12: ᴅᴏɴᴇ");
        assert_eq!(small_caps("a\nb"), "ᴀ\nʙ");
        assert_eq!(small_caps("a/b"), "ᴀ/ʙ");
    }

    #[test]
    fn direction_marks() {
        let text = "⌯ Title\n\n✦ line \u{200e}X\nlast";
        assert_eq!(
            mark_direction("en", text),
            "\u{200e}⌯ Title\n\n\u{200e}✦ line \u{200e}X\n\u{200e}last"
        );
        assert_eq!(
            mark_direction("ar", text),
            "\u{200f}⌯ Title\n\n\u{200f}✦ line \u{200e}X\n\u{200f}last"
        );
        assert_eq!(mark_direction("fa", text), mark_direction("ar", text));
        assert_eq!(mark_direction("pt-br", text), mark_direction("en", text));
        assert_eq!(mark_direction("ar", "one"), "one");
    }
}
