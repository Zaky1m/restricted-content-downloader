//! Inline keyboards with the colored button styles (primary, success, danger).

use grammers_client::message::ReplyMarkup;
use grammers_tl_types as tl;
use unicode_general_category::{GeneralCategory as Cat, get_general_category};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Plain,
    Blue,
    Green,
    Red,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Button {
    Callback {
        text: String,
        data: String,
        color: Color,
    },
    Url {
        text: String,
        url: String,
        color: Color,
    },
}

/// Python's `str.isspace`: the Unicode white space plus the four separator controls.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Button labels keep only letters, numbers, marks, spaces, `/` and `-`; emoji go.
pub fn plain_button_text(value: &str) -> String {
    let kept: String = value
        .chars()
        .filter(|c| !matches!(c, '\u{fe0f}' | '\u{fe0e}' | '\u{200d}' | '\u{20e3}'))
        .filter(|&c| {
            is_space(c)
                || c == '/'
                || c == '-'
                || matches!(
                    get_general_category(c),
                    Cat::UppercaseLetter
                        | Cat::LowercaseLetter
                        | Cat::TitlecaseLetter
                        | Cat::ModifierLetter
                        | Cat::OtherLetter
                        | Cat::DecimalNumber
                        | Cat::LetterNumber
                        | Cat::OtherNumber
                        | Cat::NonspacingMark
                        | Cat::SpacingMark
                        | Cat::EnclosingMark
                )
        })
        .collect();
    kept.split(is_space)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The word shown when a label is only an emoji (button labels drop emoji), chosen from the action like the
/// Python bot did: cancel, delete, confirm, else open.
pub fn fallback_key(data: &str) -> &'static str {
    if ["cancel", "_no", "qc_"].iter().any(|p| data.contains(p)) {
        "buttons.cancel"
    } else if data.contains("del") {
        "buttons.delete"
    } else if ["confirm", "_ok", "_yes"].iter().any(|p| data.contains(p)) {
        "buttons.confirm"
    } else {
        "buttons.open"
    }
}

impl Button {
    pub fn is_blank(&self) -> bool {
        match self {
            Button::Callback { text, .. } | Button::Url { text, .. } => text.is_empty(),
        }
    }

    pub fn callback(text: &str, data: &str, color: Color) -> Self {
        Button::Callback {
            text: plain_button_text(text),
            data: data.to_string(),
            color,
        }
    }

    pub fn url(text: &str, url: &str, color: Color) -> Self {
        Button::Url {
            text: plain_button_text(text),
            url: url.to_string(),
            color,
        }
    }

    fn raw(&self) -> tl::enums::KeyboardButton {
        match self {
            Button::Callback { text, data, color } => tl::types::KeyboardButtonCallback {
                requires_password: false,
                style: style(*color),
                text: text.clone(),
                data: data.clone().into_bytes(),
            }
            .into(),
            Button::Url { text, url, color } => tl::types::KeyboardButtonUrl {
                style: style(*color),
                text: text.clone(),
                url: url.clone(),
            }
            .into(),
        }
    }
}

fn style(color: Color) -> Option<tl::enums::KeyboardButtonStyle> {
    let (primary, danger, success) = match color {
        Color::Plain => return None,
        Color::Blue => (true, false, false),
        Color::Red => (false, true, false),
        Color::Green => (false, false, true),
    };
    Some(
        tl::types::KeyboardButtonStyle {
            bg_primary: primary,
            bg_danger: danger,
            bg_success: success,
            icon: None,
        }
        .into(),
    )
}

pub fn keyboard(rows: &[Vec<Button>]) -> ReplyMarkup {
    // Telegram hides a button with no text, so a blank label is a bug worth seeing in the log.
    if let Some(blank) = rows.iter().flatten().find(|b| b.is_blank()) {
        tracing::error!(button = ?blank, "keyboard has a button without a label");
    }
    ReplyMarkup {
        raw: tl::enums::ReplyMarkup::ReplyInlineMarkup(tl::types::ReplyInlineMarkup {
            rows: rows
                .iter()
                .map(|row| {
                    tl::types::KeyboardButtonRow {
                        buttons: row.iter().map(Button::raw).collect(),
                    }
                    .into()
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_only_labels_get_a_word_by_action() {
        assert_eq!(fallback_key("cancel_login"), "buttons.cancel");
        assert_eq!(fallback_key("thumb_confirm_no"), "buttons.cancel");
        assert_eq!(fallback_key("login_key_del"), "buttons.delete");
        assert_eq!(fallback_key("login_key_ok"), "buttons.confirm");
        assert_eq!(fallback_key("thumb_confirm_yes"), "buttons.confirm");
        assert!(Button::callback("❌", "x", Color::Red).is_blank());
    }

    /// Every literal emoji-only label must go through `Bot::button`, which swaps in a translated word.
    #[test]
    fn no_raw_emoji_buttons_in_the_code() {
        let mut offenders = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.ends_with("telegram/keyboards.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Every way a label is written straight into a button: the constructor and the
                // color helpers of the admin panel.
                for (number, line) in text.lines().enumerate() {
                    for call in ["Button::callback(\"", "red(\"", "green(\"", "blue(\""] {
                        for rest in line.split(call).skip(1) {
                            if let Some(label) = rest.split('"').next()
                                && plain_button_text(label).is_empty()
                            {
                                offenders.push(format!("{}:{}", path.display(), number + 1));
                            }
                        }
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "blank button labels: {offenders:?}");
    }

    #[test]
    fn labels_match_python() {
        assert_eq!(plain_button_text("✅ Done"), "Done");
        assert_eq!(plain_button_text("📥 تحميل  الملف"), "تحميل الملف");
        assert_eq!(plain_button_text("1️⃣ One"), "1 One");
        assert_eq!(plain_button_text("❌"), "");
        assert_eq!(plain_button_text("🔙 Back/Next-x"), "Back/Next-x");
        assert_eq!(plain_button_text("  a   b "), "a b");
        assert_eq!(plain_button_text("हिन्दी ⌯ ok"), "हिन्दी ok");
        assert_eq!(plain_button_text("ǫ ᴀ"), "ǫ ᴀ");
    }

    #[test]
    fn colors_become_styles() {
        let tl::enums::ReplyMarkup::ReplyInlineMarkup(m) =
            keyboard(&[vec![Button::callback("x", "d", Color::Green)]]).raw
        else {
            panic!("not inline");
        };
        let tl::enums::KeyboardButtonRow::Row(row) = &m.rows[0];
        let tl::enums::KeyboardButton::Callback(b) = &row.buttons[0] else {
            panic!("not callback")
        };
        let Some(tl::enums::KeyboardButtonStyle::Style(s)) = &b.style else {
            panic!("no style")
        };
        assert!(s.bg_success && !s.bg_danger && !s.bg_primary);
        assert_eq!(b.data, b"d");
    }
}
