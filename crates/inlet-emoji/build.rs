// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Turns data/emojis.json into Rust statics (OUT_DIR/data_generated.rs).
//
// data/emojis.json is Fluxer's emoji list (shortcodes, categories, skin tones, keywords),
// copied unchanged from https://github.com/fluxerapp/fluxer at the commit named in
// SOURCES.md. It is AGPL-3.0-or-later, like Inlet.

use std::fmt::Write as _;
use std::{env, fs, path::PathBuf};

use serde_json::Value;

/// Category order of Fluxer's emoji picker; the JSON object order is not relied on.
const CATEGORIES: [(&str, &str); 8] = [
    ("people", "People"),
    ("nature", "Nature"),
    ("food", "Food"),
    ("activity", "Activity"),
    ("travel", "Travel"),
    ("objects", "Objects"),
    ("symbols", "Symbols"),
    ("flags", "Flags"),
];

fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(c),
            _ => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

fn str_list(values: &[Value]) -> String {
    let items: Vec<String> = values
        .iter()
        .map(|v| lit(v.as_str().expect("string in emoji list")))
        .collect();
    format!("&[{}]", items.join(", "))
}

fn main() {
    println!("cargo:rerun-if-changed=data/emojis.json");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root: Value = serde_json::from_str(
        &fs::read_to_string(manifest.join("data/emojis.json")).expect("read data/emojis.json"),
    )
    .expect("parse data/emojis.json");
    let categories = root["categories"].as_object().expect("categories object");
    assert_eq!(
        categories.len(),
        CATEGORIES.len(),
        "unexpected category set"
    );

    let mut emojis = String::new();
    let mut keywords = String::new();
    let mut ranges = String::new();
    let mut skins: Vec<(String, [String; 5])> = Vec::new();
    let mut extra: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut seen_names = std::collections::HashSet::new();

    for (key, variant) in CATEGORIES {
        let list = categories
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("category {key} missing from data/emojis.json"));
        let start = total;
        for entry in list {
            let glyph = entry["surrogates"].as_str().expect("surrogates");
            let names = entry["names"].as_array().expect("names");
            assert!(!names.is_empty(), "emoji without a name: {glyph}");
            for name in names {
                let name = name.as_str().expect("name");
                // search.rs compares names as they are.
                assert!(
                    name.to_lowercase() == name,
                    "name {name:?} is not lower case"
                );
                assert!(
                    seen_names.insert(name.to_owned()),
                    "duplicate name {name:?}"
                );
            }
            let skin_list = entry.get("skins").and_then(Value::as_array);
            let has_skins = skin_list.is_some_and(|s| !s.is_empty());
            let _ = writeln!(
                emojis,
                "    Emoji {{ glyph: {}, names: {}, category: Category::{variant}, skin_tones: {has_skins} }},",
                lit(glyph),
                str_list(names),
            );
            let kw = entry.get("keywords").and_then(Value::as_array).map_or_else(
                || "&[]".to_owned(),
                |k| {
                    // Lower case here, so that search does not copy them at run time.
                    let lowered: Vec<Value> = k
                        .iter()
                        .map(|v| Value::from(v.as_str().expect("keyword").to_lowercase()))
                        .collect();
                    str_list(&lowered)
                },
            );
            let _ = writeln!(keywords, "    {kw},");
            if let Some(list) = skin_list.filter(|s| !s.is_empty()) {
                assert!(
                    list.len() == 5 || list.len() == 25,
                    "skin layout of {glyph}"
                );
                let glyphs: Vec<String> = list
                    .iter()
                    .map(|s| lit(s["surrogates"].as_str().expect("skin surrogates")))
                    .collect();
                skins.push((
                    glyph.to_owned(),
                    [
                        glyphs[0].clone(),
                        glyphs[1].clone(),
                        glyphs[2].clone(),
                        glyphs[3].clone(),
                        glyphs[4].clone(),
                    ],
                ));
                extra.extend(glyphs.into_iter().skip(5));
            }
            total += 1;
        }
        let _ = writeln!(ranges, "    (Category::{variant}, {start}, {total}),");
    }

    // Sorted by the glyph string so the runtime can binary search.
    skins.sort_by(|a, b| a.0.cmp(&b.0));

    let mut shortcuts: Vec<(String, String)> = root["shortcuts"]
        .as_object()
        .expect("shortcuts object")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("shortcut target").to_owned()))
        .collect();
    shortcuts.sort();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "pub(crate) static EMOJIS: [Emoji; {total}] = [\n{emojis}];\n"
    );
    let _ = writeln!(
        out,
        "pub(crate) static KEYWORDS: [&[&str]; {total}] = [\n{keywords}];\n"
    );
    let _ = writeln!(
        out,
        "pub(crate) static CATEGORY_RANGES: [(Category, usize, usize); {}] = [\n{ranges}];\n",
        CATEGORIES.len()
    );
    let _ = writeln!(
        out,
        "/// Base glyph and its five single-tone variants (light to dark).\npub(crate) static SKINS: [(&str, [&str; 5]); {}] = [",
        skins.len()
    );
    for (base, tones) in &skins {
        let _ = writeln!(out, "    ({}, [{}]),", lit(base), tones.join(", "));
    }
    let _ = writeln!(out, "];\n");
    let _ = writeln!(
        out,
        "/// Mixed-tone variants of couples (not offered by the skin tone picker).\npub(crate) static MIXED_TONE_GLYPHS: [&str; {}] = [",
        extra.len()
    );
    for glyph in &extra {
        let _ = writeln!(out, "    {glyph},");
    }
    let _ = writeln!(out, "];\n");
    let _ = writeln!(
        out,
        "/// Text shortcuts such as `:D`, sorted, with the shortcode they stand for.\npub(crate) static SHORTCUTS: [(&str, &str); {}] = [",
        shortcuts.len()
    );
    for (text, name) in &shortcuts {
        let _ = writeln!(out, "    ({}, {}),", lit(text), lit(name));
    }
    let _ = writeln!(out, "];");

    let dest = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("data_generated.rs");
    fs::write(dest, out).expect("write data_generated.rs");
}
