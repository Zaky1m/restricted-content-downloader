//! Every language file has exactly the keys and placeholders of English.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use regex::Regex;
use serde_json::Value;

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
        other => panic!("{prefix}: only strings and objects are allowed, found {other}"),
    }
}

fn load_all() -> BTreeMap<String, BTreeMap<String, String>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let mut all = BTreeMap::new();
    for entry in fs::read_dir(dir).expect("locales folder") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let code = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).expect("read"))
            .unwrap_or_else(|e| panic!("{code}: {e}"));
        let mut flat = BTreeMap::new();
        flatten("", &value, &mut flat);
        all.insert(code, flat);
    }
    all
}

fn placeholders(text: &str) -> BTreeSet<String> {
    let pattern = Regex::new(r"\{[a-z_0-9]+\}").expect("static pattern");
    pattern
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

#[test]
fn every_language_has_the_keys_of_english() {
    let all = load_all();
    assert_eq!(all.len(), 30, "language files");
    let english = &all["en"];
    assert_eq!(english.len(), 451);
    for (code, texts) in &all {
        let missing: Vec<_> = english.keys().filter(|k| !texts.contains_key(*k)).collect();
        let extra: Vec<_> = texts.keys().filter(|k| !english.contains_key(*k)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{code}: missing {missing:?}, extra {extra:?}"
        );
    }
}

#[test]
fn placeholders_are_the_same_in_every_language() {
    let all = load_all();
    let english = &all["en"];
    for (code, texts) in &all {
        for (key, reference) in english {
            assert_eq!(
                placeholders(&texts[key]),
                placeholders(reference),
                "{code}: {key}"
            );
        }
    }
}

#[test]
fn every_language_has_a_name() {
    for (code, texts) in load_all() {
        assert!(
            texts.get("lang_name").is_some_and(|n| !n.trim().is_empty()),
            "{code}: lang_name"
        );
    }
}

#[test]
fn loader_reads_every_locale_file() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let locales = rcd::i18n::Locales::load(&dir).expect("load");
    assert_eq!(locales.codes().len(), 30);
    assert_eq!(locales.normalize(Some("pt_BR")), "pt-br");
    // zh-cn was merged into zh-hans; Telegram's other Chinese codes land on the right script.
    assert_eq!(locales.normalize(Some("zh-CN")), "zh-hans");
    assert_eq!(locales.normalize(Some("zh")), "zh-hans");
    assert_eq!(locales.normalize(Some("zh-hk")), "zh-hant");
    assert_eq!(locales.normalize(Some("zh-TW")), "zh-tw");
    assert!(
        !locales
            .text(Some("ar"), "common.yes", &[])
            .starts_with("KEY_NOT_FOUND")
    );
}
