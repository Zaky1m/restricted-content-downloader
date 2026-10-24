use std::sync::OnceLock;

use regex::Regex;

/// Drops spaces, punctuation and bidi marks, turns a leading `00` into `+`,
/// and adds `+` to a bare number (Telegram shares contacts without it).
pub fn normalize(phone: &str) -> String {
    static NOISE: OnceLock<Regex> = OnceLock::new();
    let noise = NOISE.get_or_init(|| {
        Regex::new(r"[\s()\-.\u{200e}\u{200f}\u{202a}-\u{202e}]").expect("static pattern")
    });
    let cleaned = noise.replace_all(phone.trim(), "").into_owned();
    if let Some(rest) = cleaned.strip_prefix("00") {
        format!("+{rest}")
    } else if !cleaned.is_empty() && cleaned.chars().all(|c| c.is_ascii_digit()) {
        format!("+{cleaned}")
    } else {
        cleaned
    }
}

pub fn is_valid_international(phone: &str) -> bool {
    static VALID: OnceLock<Regex> = OnceLock::new();
    let valid = VALID.get_or_init(|| Regex::new(r"^\+[1-9]\d{6,14}$").expect("static pattern"));
    valid.is_match(&normalize(phone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_the_python_bot() {
        let cases = [
            ("+20 100-123 4567", "+201001234567", true),
            ("00201001234567", "+201001234567", true),
            ("201001234567", "+201001234567", true),
            ("(+1) 555.123.4567", "+15551234567", true),
            ("\u{200e}+20100", "+20100", false),
            ("abc", "abc", false),
            ("", "", false),
            ("+0123456789", "+0123456789", false),
            ("+1234567", "+1234567", true),
            ("+123456", "+123456", false),
        ];
        for (input, normalized, valid) in cases {
            assert_eq!(normalize(input), normalized, "{input:?}");
            assert_eq!(is_valid_international(input), valid, "{input:?}");
        }
    }
}
