//! Telegram link parsing. Every accepted shape comes from the Python bot's
//! `getChatMsgID` and `parse_channel_reference`.

use std::sync::OnceLock;

use regex::Regex;

const PREFIX: &str = r"(?i)^https?://(?:www\.)?(?:telegram\.me|t\.me)";

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Chat {
    Username(String),
    /// Bot API dialog id such as -1001234567.
    Id(i64),
    /// Invite link kept as given (joinchat or +hash).
    Invite(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct MessageLink {
    pub chat: Chat,
    pub message_id: i64,
    pub comment_id: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("Empty channel reference")]
    Empty,
    #[error("Invalid private channel link")]
    InvalidPrivate,
    #[error("The link must point to a Telegram message")]
    NotAMessage,
    #[error("Unsupported channel reference")]
    Unsupported,
}

/// Bot API id of a channel from the number in a `t.me/c/<n>` link.
pub fn channel_dialog_id(n: i64) -> i64 {
    -1_000_000_000_000 - n
}

fn re(cell: &'static OnceLock<Regex>, tail: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&format!("{PREFIX}{tail}$")).expect("static pattern"))
}

fn strip_query(link: &str) -> &str {
    let no_query = link.split('?').next().unwrap_or("");
    let no_fragment = no_query.split('#').next().unwrap_or("");
    no_fragment.trim_end_matches('/')
}

pub fn is_supported_link(value: &str) -> bool {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let pattern =
        CELL.get_or_init(|| Regex::new(&format!(r"{PREFIX}(?:/|$)")).expect("static pattern"));
    pattern.is_match(value.trim())
}

fn comment_id(link: &str) -> Option<i64> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let pattern =
        CELL.get_or_init(|| Regex::new(r"(?i)[?&]comment=(\d+)").expect("static pattern"));
    pattern.captures(link).and_then(|c| c[1].parse().ok())
}

pub fn parse_message_link(link: &str) -> Result<MessageLink, LinkError> {
    static TOPIC_PRIVATE: OnceLock<Regex> = OnceLock::new();
    static PRIVATE: OnceLock<Regex> = OnceLock::new();
    static TOPIC_PUBLIC: OnceLock<Regex> = OnceLock::new();
    static PUBLIC: OnceLock<Regex> = OnceLock::new();

    let link = link.trim();
    let comment_id = comment_id(link);
    let clean = strip_query(link);

    let private = |caps: &regex::Captures, message_group: usize| {
        let channel = caps[1].parse::<i64>().map_err(|_| LinkError::NotAMessage)?;
        let message_id = caps[message_group]
            .parse::<i64>()
            .map_err(|_| LinkError::NotAMessage)?;
        Ok(MessageLink {
            chat: Chat::Id(channel_dialog_id(channel)),
            message_id,
            comment_id,
        })
    };
    let public = |caps: &regex::Captures, message_group: usize| {
        if caps[1].eq_ignore_ascii_case("c") {
            return Err(LinkError::InvalidPrivate);
        }
        let message_id = caps[message_group]
            .parse::<i64>()
            .map_err(|_| LinkError::NotAMessage)?;
        Ok(MessageLink {
            chat: Chat::Username(caps[1].to_string()),
            message_id,
            comment_id,
        })
    };

    if let Some(caps) = re(&TOPIC_PRIVATE, r"/c/(\d+)/(\d+)/(\d+)").captures(clean) {
        return private(&caps, 3);
    }
    if let Some(caps) = re(&PRIVATE, r"/c/(\d+)/(\d+)").captures(clean) {
        return private(&caps, 2);
    }
    if let Some(caps) = re(&TOPIC_PUBLIC, r"/(?:s/)?([\w\d_]+)/(\d+)/(\d+)").captures(clean) {
        return public(&caps, 3);
    }
    if let Some(caps) = re(&PUBLIC, r"/(?:s/)?([\w\d_]+)/(\d+)").captures(clean) {
        return public(&caps, 2);
    }
    Err(LinkError::NotAMessage)
}

pub fn parse_chat_reference(value: &str) -> Result<Chat, LinkError> {
    static PRIVATE: OnceLock<Regex> = OnceLock::new();
    static INVITE: OnceLock<Regex> = OnceLock::new();
    static PUBLIC: OnceLock<Regex> = OnceLock::new();

    // Emptiness is checked before trimming: whitespace alone is "unsupported".
    if value.is_empty() {
        return Err(LinkError::Empty);
    }
    let value = value.trim();
    if value.starts_with('@') {
        return Ok(Chat::Username(value.to_string()));
    }
    let digits = value.trim_start_matches('-');
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        let number = value.parse::<i64>().map_err(|_| LinkError::Unsupported)?;
        return Ok(Chat::Id(if number < 0 {
            number
        } else {
            channel_dialog_id(number)
        }));
    }
    let clean = strip_query(value);
    if let Some(caps) = re(&PRIVATE, r"/c/(\d+)(?:/\d+)*").captures(clean) {
        let number = caps[1].parse::<i64>().map_err(|_| LinkError::Unsupported)?;
        return Ok(Chat::Id(channel_dialog_id(number)));
    }
    if re(&INVITE, r"/(?:joinchat/|\+)([\w-]+)").is_match(clean) {
        return Ok(Chat::Invite(clean.to_string()));
    }
    if let Some(caps) = re(&PUBLIC, r"/(?:s/)?([\w\d_]+)(?:/\d+)*").captures(clean) {
        if caps[1].eq_ignore_ascii_case("c") {
            return Err(LinkError::InvalidPrivate);
        }
        return Ok(Chat::Username(caps[1].to_string()));
    }
    Err(LinkError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(chat: Chat, message_id: i64, comment_id: Option<i64>) -> Result<MessageLink, LinkError> {
        Ok(MessageLink {
            chat,
            message_id,
            comment_id,
        })
    }
    fn user(name: &str) -> Chat {
        Chat::Username(name.to_string())
    }

    #[test]
    fn message_links_match_the_python_bot() {
        let cases = [
            (
                "https://t.me/c/1234567/89",
                msg(Chat::Id(-1_000_001_234_567), 89, None),
            ),
            (
                "https://t.me/c/1234567/5/89",
                msg(Chat::Id(-1_000_001_234_567), 89, None),
            ),
            ("https://t.me/durov/12", msg(user("durov"), 12, None)),
            (
                "https://telegram.me/s/durov/12?single",
                msg(user("durov"), 12, None),
            ),
            ("http://www.t.me/durov/12/", msg(user("durov"), 12, None)),
            (
                "https://t.me/durov/3/12?comment=77",
                msg(user("durov"), 12, Some(77)),
            ),
            (
                "https://t.me/c/123/4#x",
                msg(Chat::Id(-1_000_000_000_123), 4, None),
            ),
            ("  https://t.me/Foo_Bar/9  ", msg(user("Foo_Bar"), 9, None)),
            ("https://T.ME/foo/9", msg(user("foo"), 9, None)),
            ("https://t.me/c/5", Err(LinkError::InvalidPrivate)),
            ("https://t.me/durov", Err(LinkError::NotAMessage)),
            ("https://example.com/a/1", Err(LinkError::NotAMessage)),
            ("https://t.me/c/12/34/56/78", Err(LinkError::NotAMessage)),
        ];
        for (link, expected) in cases {
            assert_eq!(parse_message_link(link), expected, "{link}");
        }
    }

    #[test]
    fn chat_references_match_the_python_bot() {
        let cases = [
            ("@durov", Ok(user("@durov"))),
            ("-100123", Ok(Chat::Id(-100_123))),
            ("123", Ok(Chat::Id(-1_000_000_000_123))),
            ("https://t.me/c/123/4", Ok(Chat::Id(-1_000_000_000_123))),
            (
                "https://t.me/+AbC-d_1",
                Ok(Chat::Invite("https://t.me/+AbC-d_1".into())),
            ),
            (
                "https://t.me/joinchat/XyZ",
                Ok(Chat::Invite("https://t.me/joinchat/XyZ".into())),
            ),
            ("https://t.me/durov", Ok(user("durov"))),
            ("https://t.me/durov/55", Ok(user("durov"))),
            ("https://t.me/s/durov", Ok(user("durov"))),
            ("https://t.me/durov?start=1", Ok(user("durov"))),
            ("https://t.me/c", Err(LinkError::InvalidPrivate)),
            ("https://evil.com/x", Err(LinkError::Unsupported)),
            ("5/", Err(LinkError::Unsupported)),
            ("", Err(LinkError::Empty)),
            ("\n", Err(LinkError::Unsupported)),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_chat_reference(value), expected, "{value:?}");
        }
    }

    #[test]
    fn supported_prefix_requires_the_exact_host() {
        let cases = [
            ("https://t.me", true),
            ("https://t.me/", true),
            ("http://telegram.me/x", true),
            ("  https://t.me/a", true),
            ("https://t.me.evil.com/x", false),
            ("https://t.mex/a", false),
            ("t.me/x", false),
            ("", false),
        ];
        for (value, expected) in cases {
            assert_eq!(is_supported_link(value), expected, "{value:?}");
        }
    }
}
