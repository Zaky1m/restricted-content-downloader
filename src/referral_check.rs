//! The addition question a new user solves before a referral is credited. At most three wrong
//! answers; after that the user joins normally with no referral.

use serde::{Deserialize, Serialize};
use unicode_general_category::{GeneralCategory, get_general_category};

pub const MAX_WRONG: u8 = 3;
/// A forgotten question expires on its own (never longer than a day).
pub const TTL_SECONDS: i64 = 60 * 60;
pub const STATE_KIND: &str = "referral_check";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub inviter: i64,
    pub a: u8,
    pub b: u8,
    pub wrong: u8,
    /// The buttons offered: the answer and three other numbers, shuffled.
    #[serde(default)]
    pub options: Vec<i64>,
    /// Buttons already pressed wrongly; they disappear from the keyboard.
    #[serde(default)]
    pub removed: Vec<i64>,
    /// Message ids of the question and of the user's answers, deleted once the check ends.
    #[serde(default)]
    pub messages: Vec<i32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Correct,
    Wrong { left: u8 },
    Failed,
}

impl Check {
    pub fn new(inviter: i64) -> Self {
        let a: u8 = rand::random_range(1..=9);
        let b: u8 = rand::random_range(1..=9);
        let answer = i64::from(a) + i64::from(b);
        let mut options = vec![answer];
        while options.len() < 4 {
            let candidate = answer + rand::random_range(-4..=4);
            if candidate > 0 && !options.contains(&candidate) {
                options.push(candidate);
            }
        }
        for i in (1..options.len()).rev() {
            options.swap(i, rand::random_range(0..=i));
        }
        Self {
            inviter,
            a,
            b,
            wrong: 0,
            options,
            removed: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Options still on the keyboard.
    pub fn visible_options(&self) -> Vec<i64> {
        self.options
            .iter()
            .copied()
            .filter(|o| !self.removed.contains(o))
            .collect()
    }

    pub fn answer(&self) -> i64 {
        i64::from(self.a) + i64::from(self.b)
    }

    /// Judges one reply and counts it when wrong.
    pub fn judge(&mut self, text: &str) -> Verdict {
        if parse_number(text) == Some(self.answer()) {
            return Verdict::Correct;
        }
        self.wrong += 1;
        if let Some(chosen) = parse_number(text) {
            self.removed.push(chosen);
        }
        if self.wrong >= MAX_WRONG {
            Verdict::Failed
        } else {
            Verdict::Wrong {
                left: MAX_WRONG - self.wrong,
            }
        }
    }
}

/// Value of a decimal digit in any script (Arabic-Indic, Persian, Devanagari, Thai ...).
fn digit_value(c: char) -> Option<u32> {
    if get_general_category(c) != GeneralCategory::DecimalNumber {
        return None;
    }
    // Digits come in contiguous runs of ten starting at zero.
    let mut back = 0u32;
    let mut cursor = c as u32;
    while cursor > 0 {
        let Some(prev) = char::from_u32(cursor - 1) else {
            break;
        };
        if get_general_category(prev) != GeneralCategory::DecimalNumber {
            break;
        }
        back += 1;
        cursor -= 1;
    }
    Some(back % 10)
}

/// A whole number written with any digits, ignoring spaces and direction marks.
pub fn parse_number(text: &str) -> Option<i64> {
    let mut value: i64 = 0;
    let mut seen = false;
    for c in text.chars() {
        if c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        let d = digit_value(c)?;
        value = value.checked_mul(10)?.checked_add(i64::from(d))?;
        seen = true;
    }
    seen.then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_in_every_script() {
        assert_eq!(parse_number("12"), Some(12));
        assert_eq!(parse_number(" ١٢ "), Some(12));
        assert_eq!(parse_number("۱۲"), Some(12));
        assert_eq!(parse_number("१२"), Some(12));
        assert_eq!(parse_number("๑๒"), Some(12));
        assert_eq!(parse_number("１２"), Some(12));
        assert_eq!(parse_number("\u{200f}7"), Some(7));
        assert_eq!(parse_number("twelve"), None);
        assert_eq!(parse_number("1+1"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("99999999999999999999"), None);
    }

    #[test]
    fn three_wrong_answers_fail() {
        let mut c = Check {
            inviter: 1,
            a: 3,
            b: 4,
            wrong: 0,
            options: vec![7, 6, 8, 9],
            removed: vec![],
            messages: vec![],
        };
        assert_eq!(c.judge("8"), Verdict::Wrong { left: 2 });
        assert_eq!(c.judge("hello"), Verdict::Wrong { left: 1 });
        assert_eq!(c.judge("1"), Verdict::Failed);
    }

    #[test]
    fn right_answer_passes_even_after_mistakes() {
        let mut c = Check {
            inviter: 1,
            a: 3,
            b: 4,
            wrong: 2,
            options: vec![7, 6, 8, 9],
            removed: vec![],
            messages: vec![],
        };
        assert_eq!(c.judge("٧"), Verdict::Correct);
    }

    #[test]
    fn options_contain_the_answer_once_and_wrong_ones_disappear() {
        for _ in 0..200 {
            let mut c = Check::new(1);
            assert_eq!(c.options.len(), 4);
            assert_eq!(c.options.iter().filter(|o| **o == c.answer()).count(), 1);
            assert!(c.options.iter().all(|o| *o > 0));
            let wrong = *c.options.iter().find(|o| **o != c.answer()).unwrap();
            c.judge(&wrong.to_string());
            assert_eq!(c.visible_options().len(), 3);
        }
    }

    #[test]
    fn questions_are_small() {
        for _ in 0..200 {
            let c = Check::new(1);
            assert!((1..=9).contains(&c.a) && (1..=9).contains(&c.b));
        }
    }
}
