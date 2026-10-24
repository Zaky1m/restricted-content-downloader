use std::sync::OnceLock;

use regex::Regex;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum CaptionMode {
    #[default]
    KeepAll,
    RemoveAll,
    RemoveLinksUsers,
}

impl CaptionMode {
    pub fn from_setting(value: &str) -> Self {
        match value {
            "remove_all" => Self::RemoveAll,
            "remove_links_users" => Self::RemoveLinksUsers,
            _ => Self::KeepAll,
        }
    }
}

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("static pattern"))
}

/// Removes e-mail addresses, @usernames, links and bare domains from a piece of plain text.
fn strip_links(text: &str) -> String {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    static MENTION: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static DOMAIN: OnceLock<Regex> = OnceLock::new();
    // The regex crate has no lookbehind: keep the preceding character
    // instead of asserting it is not a word character.
    // Whole address goes, including the name before the @.
    let text = pattern(&EMAIL, r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+").replace_all(text, "");
    let text = pattern(&MENTION, r"(^|\W)@\w+").replace_all(&text, "$1");
    let text = pattern(&URL, r"(?i)https?://\S+").replace_all(&text, "");
    pattern(
        &DOMAIN,
        r"(?i)(?:www\.)?[\w-]+\.(?:com|net|org|io|me|co|tv|info|xyz|ru|uk|de|fr|it|es|nl|be|ch|at|au|ca|in|jp|kr|cn|br|mx|ar|cl|pe|ve|ec|bo|uy)(?:/\S*)?",
    )
    .replace_all(&text, "")
    .into_owned()
}

/// `text` is the caption as HTML. Only the text between tags is cleaned: a pattern running over a tag would
/// leave it unclosed, and Telegram refuses the message (ENTITY_BOUNDS_INVALID). Hidden links (`<a>`) lose
/// the link and keep their words; formatting left with nothing inside goes.
pub fn process_caption(text: &str, mode: CaptionMode, is_media: bool) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    static EMPTY_ELEMENT: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static BLANK_LINES: OnceLock<Regex> = OnceLock::new();

    if text.is_empty() {
        return String::new();
    }
    match mode {
        CaptionMode::KeepAll => text.to_string(),
        CaptionMode::RemoveAll if is_media => String::new(),
        CaptionMode::RemoveAll => text.to_string(),
        CaptionMode::RemoveLinksUsers => {
            let mut out = String::with_capacity(text.len());
            let mut last = 0;
            for tag in pattern(&TAG, r"<[^>]*>").find_iter(text) {
                out.push_str(&strip_links(&text[last..tag.start()]));
                let name = tag.as_str();
                let is_link = name == "</a>" || name.starts_with("<a ") || name == "<a>";
                if !is_link {
                    out.push_str(name);
                }
                last = tag.end();
            }
            out.push_str(&strip_links(&text[last..]));
            // `<b></b>` and alike would become zero-length entities, which Telegram refuses.
            let empty = pattern(
                &EMPTY_ELEMENT,
                r"<([a-z][\w-]*)(?:\s[^>]*)?>(\s*)</([a-z][\w-]*)>",
            );
            loop {
                let mut changed = false;
                let next = empty.replace_all(&out, |c: &regex::Captures| {
                    if c[1] == c[3] {
                        changed = true;
                        c[2].to_string()
                    } else {
                        c[0].to_string()
                    }
                });
                let next = next.into_owned();
                out = next;
                if !changed {
                    break;
                }
            }
            let text = pattern(&SPACES, r"[ \t]+").replace_all(&out, " ");
            let text = pattern(&BLANK_LINES, r"\n{3,}").replace_all(&text, "\n\n");
            text.trim().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(text: &str) -> String {
        process_caption(text, CaptionMode::RemoveLinksUsers, true)
    }

    #[test]
    fn removes_mentions_links_and_domains() {
        assert_eq!(
            clean("Hello @user join https://t.me/x now"),
            "Hello join now"
        );
        assert_eq!(clean("visit example.com/page today"), "visit today");
        assert_eq!(clean("www.site.org ok"), "ok");
        assert_eq!(clean("@a @b"), "");
        assert_eq!(clean("كلام @عربي ok"), "كلام ok");
        assert_eq!(clean("x.in"), "");
    }

    #[test]
    fn squeezes_whitespace_and_trims() {
        assert_eq!(
            clean("line1\n\n\n\nline2   spaced"),
            "line1\n\nline2 spaced"
        );
        assert_eq!(clean("  lead  "), "lead");
        assert_eq!(clean("plain text"), "plain text");
    }

    #[test]
    fn mention_must_not_follow_a_word_character() {
        assert_eq!(clean("@a@b"), "@b");
        assert_eq!(clean("@@a"), "@");
    }

    /// Approved changes from the Python bot: an e-mail goes whole (no leftover
    /// name before the @) and `.py` file names are no longer taken for sites.
    #[test]
    fn approved_fixes_over_the_python_bot() {
        assert_eq!(clean("mail me a@b.com"), "mail me");
        assert_eq!(clean("mail first.last+x@mail.example.org now"), "mail now");
        assert_eq!(clean("file.py is here"), "file.py is here");
    }

    #[test]
    fn tags_are_never_cut() {
        // From a real post: the domain pattern used to run over `</u><u>` and leave `<u>` open.
        assert_eq!(
            clean("site: <u>telegram.org/moderation</u><u>.</u> More"),
            "site: <u>.</u> More"
        );
        assert_eq!(clean("<b>@user</b> hi"), "hi");
        assert_eq!(
            clean("<a href=\"https://x.com\">click here</a> now @me"),
            "click here now"
        );
        assert_eq!(clean("<i>a</i> <b>keep</b>"), "<i>a</i> <b>keep</b>");
        assert_eq!(clean("<b><i>https://x.y.com</i></b> end"), "end");
    }

    #[test]
    fn other_modes() {
        assert_eq!(process_caption("x", CaptionMode::RemoveAll, true), "");
        assert_eq!(process_caption("x", CaptionMode::RemoveAll, false), "x");
        assert_eq!(process_caption("", CaptionMode::RemoveAll, true), "");
        assert_eq!(process_caption("x", CaptionMode::KeepAll, true), "x");
        assert_eq!(CaptionMode::from_setting("weird"), CaptionMode::KeepAll);
        assert_eq!(
            CaptionMode::from_setting("remove_all"),
            CaptionMode::RemoveAll
        );
    }
}
