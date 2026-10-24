//! Locale loading and text formatting. Behavior follows the Python `languages.py`.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;

use crate::smallcaps::{mark_direction, small_caps};

const RTL: [&str; 5] = ["ar", "fa", "he", "iw", "ur"];
const ESCAPED_FIELDS: [&str; 6] = [
    "first_name",
    "last_name",
    "user_name",
    "username",
    "channel_title",
    "error",
];

pub const DEFAULT_LANGUAGE: &str = "en";

type Map = BTreeMap<String, BTreeMap<String, String>>;

/// The loaded languages. They can be reloaded while the bot runs (a new locale file is picked up at once).
pub struct Locales {
    languages: std::sync::RwLock<std::sync::Arc<Map>>,
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, inner, out);
            }
        }
        Value::String(text) => {
            out.insert(prefix.to_string(), text.clone());
        }
        _ => {}
    }
}

fn is_rtl(language: &str) -> bool {
    RTL.contains(&language.split('-').next().unwrap_or(language))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

impl Locales {
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(Self::from_map(Self::read(dir)?))
    }

    fn from_map(map: Map) -> Self {
        Self {
            languages: std::sync::RwLock::new(std::sync::Arc::new(map)),
        }
    }

    fn map(&self) -> std::sync::Arc<Map> {
        self.languages
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Re-reads the folder. A folder that fails to load leaves the current languages in place.
    pub fn reload(&self, dir: &Path) -> Result<()> {
        let fresh = Self::read(dir)?;
        match self.languages.write() {
            Ok(mut guard) => *guard = std::sync::Arc::new(fresh),
            Err(poisoned) => *poisoned.into_inner() = std::sync::Arc::new(fresh),
        }
        Ok(())
    }

    fn read(dir: &Path) -> Result<Map> {
        let code_pattern = Regex::new(r"^[a-zA-Z]{2,3}(?:[-_][a-zA-Z0-9]{2,8})*$")?;
        let mut languages = BTreeMap::new();
        let mut names: Vec<_> = fs::read_dir(dir)
            .with_context(|| format!("cannot read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        names.sort();
        for path in names {
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let code = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !code_pattern.is_match(&code) {
                tracing::warn!(file = %path.display(), "ignoring invalid language filename");
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let value: Value = match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => v,
                _ => {
                    tracing::error!(file = %path.display(), "invalid language file");
                    continue;
                }
            };
            let mut flat = BTreeMap::new();
            flatten("", &value, &mut flat);
            languages.insert(code, flat);
        }
        if !languages.contains_key(DEFAULT_LANGUAGE) {
            bail!(
                "default language '{DEFAULT_LANGUAGE}' was not found in {}",
                dir.display()
            );
        }
        Ok(languages)
    }

    pub fn codes(&self) -> Vec<String> {
        self.map().keys().cloned().collect()
    }

    pub fn has(&self, code: &str) -> bool {
        self.map().contains_key(code)
    }

    pub fn exists(&self, code: &str) -> bool {
        !code.is_empty() && self.map().keys().any(|c| c.eq_ignore_ascii_case(code))
    }

    /// Exact, then case-insensitive, then base language ("pt-BR" -> "pt-br" -> "pt").
    pub fn normalize(&self, code: Option<&str>) -> String {
        let languages = self.map();
        let Some(code) = code
            .map(|c| c.trim().replace('_', "-"))
            .filter(|c| !c.is_empty())
        else {
            return DEFAULT_LANGUAGE.into();
        };
        if languages.contains_key(&code) {
            return code;
        }
        let lowered = code.to_lowercase();
        if let Some(found) = languages.keys().find(|c| c.to_lowercase() == lowered) {
            return found.clone();
        }
        // Other spellings of the Chinese scripts the locale files are named after.
        let alias = match lowered.as_str() {
            "zh" | "zh-cn" | "zh-sg" => Some("zh-hans"),
            "zh-hk" | "zh-mo" => Some("zh-hant"),
            _ => None,
        };
        if let Some(alias) = alias.filter(|a| languages.contains_key(*a)) {
            return alias.to_string();
        }
        let base = lowered.split('-').next().unwrap_or(&lowered).to_string();
        if languages.contains_key(&base) {
            return base;
        }
        if let Some(found) = languages
            .keys()
            .find(|c| c.to_lowercase().split('-').next() == Some(base.as_str()))
        {
            return found.clone();
        }
        DEFAULT_LANGUAGE.into()
    }

    /// `(code, display name)` sorted by case-folded name, as the language picker lists them.
    pub fn available(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self
            .map()
            .iter()
            .map(|(code, map)| {
                (
                    code.clone(),
                    map.get("lang_name")
                        .cloned()
                        .unwrap_or_else(|| code.clone()),
                )
            })
            .collect();
        list.sort_by_key(|(_, name)| name.to_lowercase());
        list
    }

    /// A byte count with the units of the language, so right-to-left text never mixes in Latin units.
    pub fn size(&self, language: Option<&str>, bytes: u64) -> String {
        let lang = self.normalize(language);
        crate::format::readable_size_with(bytes, |key| self.unit(&lang, key))
    }

    /// A duration with the units of the language.
    pub fn duration(&self, language: Option<&str>, seconds: i64) -> String {
        let lang = self.normalize(language);
        crate::format::readable_time_with(seconds, |key| self.unit(&lang, key))
    }

    // Unit templates skip small caps and direction marks: they sit inside a line that already has them.
    fn unit(&self, language: &str, key: &str) -> String {
        self.raw(language, &format!("units.{key}"))
            .unwrap_or_else(|| crate::format::english_unit(key).to_string())
    }

    fn raw(&self, language: &str, key: &str) -> Option<String> {
        let languages = self.map();
        languages
            .get(language)
            .and_then(|m| m.get(key))
            .or_else(|| languages.get(DEFAULT_LANGUAGE).and_then(|m| m.get(key)))
            .cloned()
    }

    /// Text for `key` with `{placeholders}` filled from `args`; an unknown placeholder stays visible.
    pub fn text(&self, language: Option<&str>, key: &str, args: &[(&str, &str)]) -> String {
        let lang = self.normalize(language);
        let Some(value) = self.raw(&lang, key) else {
            tracing::warn!(key, language = %lang, "translation key not found");
            return format!("KEY_NOT_FOUND:{key}");
        };
        let value = if lang == "en" && key != "lang_name" {
            small_caps(&value)
        } else {
            value
        };
        if args.is_empty() {
            return mark_direction(&lang, &value);
        }
        let filled = fill(&value, &lang, args);
        mark_direction(&lang, &filled)
    }
}

fn fill(template: &str, language: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if !after[..end].contains('{') => {
                let name = &after[..end];
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, value)) if ESCAPED_FIELDS.contains(&name) => {
                        let escaped = escape_html(value);
                        if is_rtl(language) {
                            out.push_str(&format!("\u{200f}\u{2068}{escaped}\u{2069}\u{200f}"));
                        } else {
                            out.push_str(&escaped);
                        }
                    }
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Locales {
        let mut languages = BTreeMap::new();
        let mut en = BTreeMap::new();
        en.insert("lang_name".into(), "🇺🇸 English".into());
        en.insert("hi".into(), "Hello {first_name} {x}".into());
        en.insert("only_en".into(), "Only here".into());
        languages.insert("en".into(), en);
        let mut ar = BTreeMap::new();
        ar.insert("lang_name".into(), "العربية".into());
        ar.insert("hi".into(), "اهلا {first_name}\nسطر".into());
        ar.insert("units.mb".into(), "{n} ميجا".into());
        ar.insert("units.second".into(), "{n} ثانية".into());
        languages.insert("ar".into(), ar);
        let mut pt = BTreeMap::new();
        pt.insert("lang_name".into(), "Português".into());
        languages.insert("pt-br".into(), pt);
        Locales::from_map(languages)
    }

    #[test]
    fn normalizes_codes() {
        let l = sample();
        assert_eq!(l.normalize(None), "en");
        assert_eq!(l.normalize(Some("PT_BR")), "pt-br");
        assert_eq!(l.normalize(Some("pt")), "pt-br");
        assert_eq!(l.normalize(Some("ar-EG")), "ar");
        assert_eq!(l.normalize(Some("zz")), "en");
    }

    #[test]
    fn english_is_small_caps_and_escaped() {
        let l = sample();
        assert_eq!(
            l.text(Some("en"), "hi", &[("first_name", "<b>Al</b>")]),
            "ʜᴇʟʟᴏ &lt;b&gt;Al&lt;/b&gt; {x}"
        );
        assert_eq!(l.text(Some("en"), "lang_name", &[]), "🇺🇸 English");
    }

    #[test]
    fn rtl_names_are_isolated_and_lines_marked() {
        let l = sample();
        assert_eq!(
            l.text(Some("ar"), "hi", &[("first_name", "Ali")]),
            "\u{200f}اهلا \u{200f}\u{2068}Ali\u{2069}\u{200f}\n\u{200f}سطر"
        );
    }

    #[test]
    fn falls_back_to_english_and_reports_missing() {
        let l = sample();
        assert_eq!(l.text(Some("ar"), "only_en", &[]), "Only here");
        assert_eq!(l.text(Some("en"), "nope", &[]), "KEY_NOT_FOUND:nope");
    }

    #[test]
    fn units_follow_the_language() {
        let l = sample();
        assert_eq!(l.size(Some("ar"), 5 << 20), "5.0 ميجا");
        assert_eq!(l.duration(Some("ar"), 38), "38 ثانية");
        assert_eq!(l.size(Some("en"), 5 << 20), "5.0 MB");
        // A unit the language lacks falls back to English.
        assert_eq!(l.duration(Some("ar"), 60), "1m");
    }
}
