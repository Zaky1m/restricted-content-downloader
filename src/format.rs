/// Unit keys under `units.` in the locale files, smallest first.
const SIZE_UNITS: [&str; 6] = ["b", "kb", "mb", "gb", "tb", "pb"];

/// The English unit templates; `{n}` is the number.
pub fn english_unit(key: &str) -> &'static str {
    match key {
        "b" => "{n} B",
        "kb" => "{n} KB",
        "mb" => "{n} MB",
        "gb" => "{n} GB",
        "tb" => "{n} TB",
        "pb" => "{n} PB",
        "day" => "{n}d",
        "hour" => "{n}h",
        "minute" => "{n}m",
        _ => "{n}s",
    }
}

/// Human readable byte count. A value is always printed with at least one
/// decimal ("1.0 KB") because the Python bot printed a float.
pub fn readable_size(bytes: u64) -> String {
    readable_size_with(bytes, |key| english_unit(key).to_string())
}

/// `readable_size` with the unit templates of a language.
pub fn readable_size_with(bytes: u64, unit: impl Fn(&str) -> String) -> String {
    if bytes == 0 {
        return unit(SIZE_UNITS[0]).replace("{n}", "0");
    }
    let mut index = 0;
    let mut scaled = bytes;
    while scaled >= 1024 && index < SIZE_UNITS.len() - 1 {
        scaled /= 1024;
        index += 1;
    }
    let value = bytes as f64 / 1024f64.powi(index as i32);
    let mut text = format!("{value:.2}");
    if text.ends_with('0') {
        text.pop();
    }
    unit(SIZE_UNITS[index]).replace("{n}", &text)
}

/// "1d 2h 3m 4s", omitting zero parts; zero seconds prints "0s".
pub fn readable_time(seconds: i64) -> String {
    readable_time_with(seconds, |key| english_unit(key).to_string())
}

/// `readable_time` with the unit templates of a language.
pub fn readable_time_with(seconds: i64, unit: impl Fn(&str) -> String) -> String {
    let seconds = seconds.max(0);
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, seconds) = (rest / 60, rest % 60);
    let mut parts = Vec::new();
    for (value, key) in [(days, "day"), (hours, "hour"), (minutes, "minute")] {
        if value > 0 {
            parts.push(unit(key).replace("{n}", &value.to_string()));
        }
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(unit("second").replace("{n}", &seconds.to_string()));
    }
    parts.join(" ")
}

/// A float printed the way Python does: at most what `round(x, 2)` leaves, and `45.0` rather than `45`.
pub fn py_float_2(value: f64) -> String {
    let text = format!("{}", (value * 100.0).round() / 100.0);
    if text.contains('.') || text.contains('e') {
        text
    } else {
        format!("{text}.0")
    }
}

/// Python's `urllib.parse.quote_plus`: spaces become `+`, unreserved characters stay, the rest is %XX.
pub fn quote_plus(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// File name with the characters Windows and shells dislike replaced by `_`; `fallback` if nothing is left.
pub fn safe_filename(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c <= '\u{1f}' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback.to_string()
    } else {
        shorten(trimmed)
    }
}

/// File systems refuse names above 255 bytes; long names keep their start and their extension.
fn shorten(name: &str) -> String {
    const MAX: usize = 200;
    if name.len() <= MAX {
        return name.to_string();
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.len() <= 16)
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut stem = String::new();
    for c in name.chars() {
        if stem.len() + c.len_utf8() + extension.len() > MAX {
            break;
        }
        stem.push(c);
    }
    format!("{}{extension}", stem.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_the_python_bot() {
        let cases: [(u64, &str); 15] = [
            (0, "0 B"),
            (1, "1.0 B"),
            (1023, "1023.0 B"),
            (1024, "1.0 KB"),
            (1536, "1.5 KB"),
            (1500, "1.46 KB"),
            (1_048_576, "1.0 MB"),
            (3_145_728, "3.0 MB"),
            (10_485_760, "10.0 MB"),
            (123_456_789, "117.74 MB"),
            (2_086_666_240, "1.94 GB"),
            (2_147_483_648, "2.0 GB"),
            (4_294_967_296, "4.0 GB"),
            (1_099_511_627_776, "1.0 TB"),
            (1_125_899_906_842_624, "1.0 PB"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(readable_size(bytes), expected, "{bytes}");
        }
    }

    #[test]
    fn size_stays_in_petabytes() {
        assert_eq!(readable_size(1 << 60), "1024.0 PB");
    }

    #[test]
    fn time_omits_zero_parts() {
        assert_eq!(readable_time(0), "0s");
        assert_eq!(readable_time(59), "59s");
        assert_eq!(readable_time(60), "1m");
        assert_eq!(readable_time(3_661), "1h 1m 1s");
        assert_eq!(readable_time(90_000), "1d 1h");
        assert_eq!(readable_time(-5), "0s");
    }

    #[test]
    fn safe_filenames() {
        assert_eq!(safe_filename("a/b:c?.jpg", "x"), "a_b_c_.jpg");
        assert_eq!(safe_filename("  ", "x.bin"), "x.bin");
        assert_eq!(safe_filename("ok name.mp4", "x"), "ok name.mp4");
        assert_eq!(safe_filename("..", "x.bin"), "x.bin");
        let long = format!("{}.mp4", "ع".repeat(300));
        let short = safe_filename(&long, "x");
        assert!(short.len() <= 200 && short.ends_with(".mp4"));
    }

    #[test]
    fn quotes_like_python() {
        assert_eq!(quote_plus("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
        assert_eq!(
            quote_plus("https://t.me/x?start=1"),
            "https%3A%2F%2Ft.me%2Fx%3Fstart%3D1"
        );
    }

    #[test]
    fn floats_print_like_python() {
        assert_eq!(py_float_2(45.0), "45.0");
        assert_eq!(py_float_2(12.345), "12.35");
        assert_eq!(py_float_2(0.5), "0.5");
        assert_eq!(py_float_2(100.0), "100.0");
    }
}
