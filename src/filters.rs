use std::collections::BTreeSet;

pub const FILTER_TYPES: [&str; 6] = ["videos", "photos", "audio", "files", "text", "other"];

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FilterMode {
    /// `-sv`: save only the listed types.
    Save,
    /// `-rm`: skip the listed types.
    Remove,
}

#[derive(Debug, PartialEq, Eq, Default)]
pub struct FilterSettings {
    pub mode: Option<FilterMode>,
    pub types: BTreeSet<String>,
    pub min_duration: Option<u64>,
    pub max_duration: Option<u64>,
}

fn duration_after(flags: &[&str], name: &str) -> Option<u64> {
    let index = flags.iter().position(|f| *f == name)?;
    let value = flags.get(index + 1)?;
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// `--fdl` is ignored: forced download is a stored user setting now.
pub fn extract_filter_settings(flags: &[&str]) -> FilterSettings {
    let flags: Vec<&str> = flags.iter().copied().filter(|f| *f != "--fdl").collect();
    let (mode, mode_index) = if let Some(i) = flags.iter().position(|f| *f == "-sv") {
        (Some(FilterMode::Save), i)
    } else if let Some(i) = flags.iter().position(|f| *f == "-rm") {
        (Some(FilterMode::Remove), i)
    } else {
        (None, 0)
    };

    let mut types = BTreeSet::new();
    if mode.is_some() {
        let mut index = mode_index + 1;
        while index < flags.len() {
            let flag = flags[index];
            if flag == "-sv" || flag == "-rm" {
                break;
            }
            if flag == "--min" || flag == "--max" {
                index += 2;
                continue;
            }
            if let Some(kind) = flag.strip_prefix("--") {
                if FILTER_TYPES.contains(&kind) {
                    types.insert(kind.to_string());
                }
            }
            index += 1;
        }
    }
    FilterSettings {
        mode,
        types,
        min_duration: duration_after(&flags, "--min"),
        max_duration: duration_after(&flags, "--max"),
    }
}

/// Whether a message of `kind` passes the filter. Anything unknown passes.
pub fn should_process(kind: Option<&str>, settings: &FilterSettings) -> bool {
    let (Some(mode), Some(kind)) = (settings.mode, kind) else {
        return true;
    };
    if settings.types.is_empty() {
        return true;
    }
    match mode {
        FilterMode::Save => settings.types.contains(kind),
        FilterMode::Remove => !settings.types.contains(kind),
    }
}

/// Duration 0 means "unknown" and always passes.
pub fn duration_allowed(duration: u64, settings: &FilterSettings) -> bool {
    if duration == 0 {
        return true;
    }
    if settings.min_duration.is_some_and(|min| duration < min) {
        return false;
    }
    !settings.max_duration.is_some_and(|max| duration > max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(flags: &[&str]) -> (Option<FilterMode>, Vec<String>, Option<u64>, Option<u64>) {
        let s = extract_filter_settings(flags);
        (
            s.mode,
            s.types.into_iter().collect(),
            s.min_duration,
            s.max_duration,
        )
    }
    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_match_the_python_bot() {
        use FilterMode::{Remove, Save};
        assert_eq!(
            parse(&["-sv", "--videos", "--audio"]),
            (Some(Save), names(&["audio", "videos"]), None, None)
        );
        assert_eq!(
            parse(&["-rm", "--photos", "--min", "30", "--max", "60"]),
            (Some(Remove), names(&["photos"]), Some(30), Some(60))
        );
        assert_eq!(parse(&["--min", "10"]), (None, vec![], Some(10), None));
        assert_eq!(
            parse(&["-sv", "--videos", "-rm", "--photos"]),
            (Some(Save), names(&["videos"]), None, None)
        );
        assert_eq!(
            parse(&["-sv", "--fdl", "--other"]),
            (Some(Save), names(&["other"]), None, None)
        );
        assert_eq!(
            parse(&["-rm", "--bogus", "--text"]),
            (Some(Remove), names(&["text"]), None, None)
        );
        assert_eq!(
            parse(&["--max", "abc", "--min", "5"]),
            (None, vec![], Some(5), None)
        );
        assert_eq!(parse(&["--min"]), (None, vec![], None, None));
        assert_eq!(parse(&[]), (None, vec![], None, None));
        assert_eq!(
            parse(&["-sv", "--min", "5", "--videos"]),
            (Some(Save), names(&["videos"]), Some(5), None)
        );
    }

    #[test]
    fn message_kind_filter() {
        let save = extract_filter_settings(&["-sv", "--videos"]);
        assert!(should_process(Some("videos"), &save));
        assert!(!should_process(Some("photos"), &save));
        let remove = extract_filter_settings(&["-rm", "--videos"]);
        assert!(!should_process(Some("videos"), &remove));
        assert!(should_process(Some("photos"), &remove));
        assert!(should_process(None, &save));
        assert!(should_process(Some("photos"), &FilterSettings::default()));
        let empty = extract_filter_settings(&["-sv"]);
        assert!(should_process(Some("photos"), &empty));
    }

    #[test]
    fn duration_limits() {
        let limits = extract_filter_settings(&["--min", "10", "--max", "60"]);
        assert!(duration_allowed(0, &limits));
        assert!(!duration_allowed(9, &limits));
        assert!(duration_allowed(10, &limits));
        assert!(duration_allowed(60, &limits));
        assert!(!duration_allowed(61, &limits));
        assert!(duration_allowed(1_000, &FilterSettings::default()));
    }
}
