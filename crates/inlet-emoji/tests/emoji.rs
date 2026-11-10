// SPDX-License-Identifier: AGPL-3.0-or-later

use std::borrow::Cow;
use std::collections::HashSet;

use inlet_emoji::{
    all, by_glyph, by_shortcode, by_shortcut, categories, emoji_only, glyphs, search,
    shortcode_glyph, twemoji_code, twemoji_svg, with_skin_tone, Category, SkinTone,
    MAX_JUMBO_EMOJI,
};
use serde_json::Value;

/// Fluxer's list, copied unchanged (see SOURCES.md). The generated tables are checked
/// against it here, through the public API.
fn fluxer() -> Value {
    serde_json::from_str(include_str!("../data/emojis.json")).expect("emojis.json")
}

fn category_key(category: Category) -> &'static str {
    category.key()
}

#[test]
fn shortcodes_match_fluxers_list() {
    let fluxer = fluxer();
    let mut names_seen = 0;
    let mut emoji_seen = 0;
    for (category, emoji) in categories() {
        let expected = fluxer["categories"][category_key(*category)]
            .as_array()
            .expect("category in Fluxer's list");
        assert_eq!(emoji.len(), expected.len(), "{category:?} size");
        for (ours, theirs) in emoji.iter().zip(expected) {
            let glyph = theirs["surrogates"].as_str().unwrap();
            let names: Vec<&str> = theirs["names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap())
                .collect();
            assert_eq!(ours.glyph, glyph);
            assert_eq!(ours.names, names.as_slice(), "names of {glyph}");
            assert_eq!(ours.category, *category);
            assert_eq!(
                ours.skin_tones,
                theirs.get("skins").is_some(),
                "tones of {glyph}"
            );
            for name in &names {
                assert_eq!(by_shortcode(name).map(|e| e.glyph), Some(glyph), ":{name}:");
                assert_eq!(
                    by_shortcode(&format!(":{name}:")).map(|e| e.glyph),
                    Some(glyph)
                );
                names_seen += 1;
            }
            emoji_seen += 1;
        }
    }
    assert_eq!(emoji_seen, all().len());
    assert_eq!(emoji_seen, 1940);
    assert_eq!(names_seen, 2361);
    // Names are unique: no shortcode points at two emoji.
    let unique: HashSet<&str> = all().iter().flat_map(|e| e.names.iter().copied()).collect();
    assert_eq!(unique.len(), names_seen);

    // The categories come in the picker's order.
    let order: Vec<Category> = categories().iter().map(|(c, _)| *c).collect();
    assert_eq!(order, Category::ALL);

    // A few well-known ones, so a broken generator cannot pass by agreeing with itself.
    assert_eq!(by_shortcode("smile").unwrap().glyph, "\u{1F604}");
    assert_eq!(by_shortcode("thumbsup").unwrap().glyph, "\u{1F44D}");
    assert_eq!(by_shortcode("+1").unwrap().glyph, "\u{1F44D}");
    assert_eq!(by_shortcode("heart").unwrap().glyph, "\u{2764}\u{FE0F}");
    assert!(by_shortcode("definitely_not_an_emoji").is_none());
    assert!(by_shortcode("").is_none());

    // Text shortcuts.
    for (text, name) in fluxer["shortcuts"].as_object().unwrap() {
        let name = name.as_str().unwrap();
        assert_eq!(
            by_shortcut(text).map(|e| e.glyph),
            by_shortcode(name).map(|e| e.glyph),
            "shortcut {text}"
        );
    }
    assert_eq!(by_shortcut(":D").unwrap().name(), "smile");
    assert_eq!(by_shortcut("<3").unwrap().name(), "heart");
    assert!(by_shortcut(":not-a-shortcut").is_none());
}

#[test]
fn search_ranks_prefix_before_substring() {
    let results = search("smile", 50);
    assert!(!results.is_empty());
    assert_eq!(results[0].name(), "smile", "the exact name comes first");
    let rank = |needle: &str| {
        results
            .iter()
            .position(|e| e.names.contains(&needle))
            .unwrap()
    };
    // A name that starts with the query beats one that only contains it.
    let prefix = rank("smile_cat");
    let word = rank("slight_smile");
    assert!(
        prefix < word,
        "smile_cat ({prefix}) before slight_smile ({word})"
    );
    let contains = results.iter().position(|e| {
        e.names
            .iter()
            .all(|n| !n.starts_with("smile") && n.contains("smile"))
    });
    if let Some(contains) = contains {
        assert!(prefix < contains);
    }
    // Name matches come before keyword matches: 'smile' is a keyword of many emoji whose
    // names do not contain it, and those come last.
    let first_keyword_only = results
        .iter()
        .position(|e| !e.names.iter().any(|n| n.contains("smile")))
        .unwrap_or(results.len());
    assert!(results[..first_keyword_only]
        .iter()
        .all(|e| e.names.iter().any(|n| n.contains("smile"))));
    assert!(results[first_keyword_only..]
        .iter()
        .all(|e| !e.names.iter().any(|n| n.contains("smile"))));

    // The limit and a leading colon.
    assert_eq!(search("smile", 3).len(), 3);
    assert_eq!(search("smile", 0).len(), 0);
    assert_eq!(search(":SMILE", 1)[0].name(), "smile");
    // `thumbsup` finds `thumbs_up` names through the name without separators.
    assert!(search("thumbsup", 10)
        .iter()
        .any(|e| e.glyph == "\u{1F44D}"));
    // Keywords find emoji whose names do not contain the word.
    assert!(search("cheerful", 10)
        .iter()
        .any(|e| e.name() == "grinning"));
    assert!(search("zzzzqqq", 10).is_empty());
    // An empty query gives the start of the picker.
    assert_eq!(search("", 2)[0].glyph, all()[0].glyph);
}

#[test]
fn skin_tone_is_applied_only_where_allowed() {
    let wave = by_shortcode("wave").unwrap();
    assert!(wave.skin_tones);
    assert_eq!(with_skin_tone(wave, SkinTone::Default), wave.glyph);
    assert_eq!(with_skin_tone(wave, SkinTone::Light), "\u{1F44B}\u{1F3FB}");
    assert_eq!(with_skin_tone(wave, SkinTone::Dark), "\u{1F44B}\u{1F3FF}");
    for tone in SkinTone::TONES {
        let glyph = with_skin_tone(wave, tone);
        assert!(glyph.contains(tone.modifier().unwrap()));
        assert!(matches!(glyph, Cow::Borrowed(_)));
    }

    // No tones: the glyph comes back unchanged for every tone.
    let smile = by_shortcode("smile").unwrap();
    assert!(!smile.skin_tones);
    for tone in SkinTone::TONES {
        assert_eq!(with_skin_tone(smile, tone), smile.glyph);
    }

    // Every emoji with tones has five distinct variants that carry the right modifier,
    // and Fluxer's own list agrees about each of them.
    let fluxer = fluxer();
    let mut with_tones = 0;
    for emoji in all().iter().filter(|e| e.skin_tones) {
        with_tones += 1;
        let theirs = fluxer["categories"][emoji.category.key()]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["surrogates"] == emoji.glyph)
            .unwrap();
        let skins = theirs["skins"].as_array().unwrap();
        let mut seen = HashSet::new();
        for (index, tone) in SkinTone::TONES.into_iter().enumerate() {
            let glyph = with_skin_tone(emoji, tone);
            assert_eq!(glyph, skins[index]["surrogates"].as_str().unwrap());
            assert!(glyph.contains(tone.modifier().unwrap()), "{}", emoji.name());
            assert!(seen.insert(glyph.into_owned()));
        }
    }
    assert!(with_tones > 100, "{with_tones} emoji with tones");
    assert!(all().iter().filter(|e| !e.skin_tones).all(|e| {
        SkinTone::TONES
            .into_iter()
            .all(|t| with_skin_tone(e, t) == e.glyph)
    }));

    // Fluxer's `name::skin-tone-N` spelling.
    assert_eq!(
        shortcode_glyph("wave::skin-tone-3").unwrap(),
        "\u{1F44B}\u{1F3FD}"
    );
    assert_eq!(
        shortcode_glyph(":wave::skin-tone-5:").unwrap(),
        "\u{1F44B}\u{1F3FF}"
    );
    assert!(shortcode_glyph("smile::skin-tone-3").is_none());
    assert!(shortcode_glyph("wave::skin-tone-6").is_none());
    assert_eq!(shortcode_glyph("smile").unwrap(), "\u{1F604}");

    // A tone variant leads back to its base emoji.
    assert_eq!(by_glyph("\u{1F44B}\u{1F3FD}").unwrap().name(), "wave");
}

#[test]
fn every_glyph_has_a_twemoji_svg() {
    let mut count = 0;
    for glyph in glyphs() {
        let svg = twemoji_svg(glyph).unwrap_or_else(|| {
            panic!("no Twemoji drawing for {glyph:?} ({})", twemoji_code(glyph))
        });
        let text = std::str::from_utf8(&svg).expect("SVG is UTF-8");
        assert!(text.contains("<svg"), "{glyph:?} is not an SVG");
        assert!(
            text.trim_end().ends_with("</svg>"),
            "{glyph:?} is cut short"
        );
        count += 1;
    }
    assert_eq!(count, 3970);
    // A smiley is the right one: Twemoji's grinning face is the first of the list.
    let svg = twemoji_svg("\u{1F600}").unwrap();
    assert!(std::str::from_utf8(&svg).unwrap().starts_with("<svg"));

    // Twemoji's file names: FE0F is dropped, except inside ZWJ sequences.
    assert_eq!(twemoji_code("\u{1F600}"), "1f600");
    assert_eq!(twemoji_code("\u{2764}\u{FE0F}"), "2764");
    assert_eq!(twemoji_code("1\u{FE0F}\u{20E3}"), "31-20e3");
    assert_eq!(
        twemoji_code("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        "1f468-200d-1f469-200d-1f467"
    );
    assert_eq!(
        twemoji_code("\u{2764}\u{FE0F}\u{200D}\u{1F525}"),
        "2764-fe0f-200d-1f525"
    );
    assert_eq!(
        twemoji_code("\u{1F441}\u{FE0F}\u{200D}\u{1F5E8}\u{FE0F}"),
        "1f441-200d-1f5e8"
    );

    // Typed text may leave out FE0F where Twemoji keeps it.
    assert_eq!(
        twemoji_svg("\u{2764}\u{200D}\u{1F525}").unwrap(),
        twemoji_svg("\u{2764}\u{FE0F}\u{200D}\u{1F525}").unwrap()
    );
    // Not an emoji, so no drawing.
    assert!(twemoji_svg("a").is_none());
    assert!(twemoji_svg("").is_none());
    assert!(twemoji_svg("\u{1F600}\u{1F600}").is_none());
}

#[test]
fn emoji_only_counts_zwj_sequences_as_one() {
    // A family is one emoji, not four.
    assert_eq!(
        emoji_only("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"),
        Some(1)
    );
    assert_eq!(
        emoji_only("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}\u{1F600}"),
        Some(2)
    );
    // A flag, a keycap and a tone variant are each one.
    assert_eq!(emoji_only("\u{1F1F3}\u{1F1F4}"), Some(1));
    assert_eq!(emoji_only("1\u{FE0F}\u{20E3}"), Some(1));
    assert_eq!(emoji_only("\u{1F44B}\u{1F3FD}"), Some(1));
    // Two flags in a row are two.
    assert_eq!(emoji_only("\u{1F1F3}\u{1F1F4}\u{1F1F8}\u{1F1EA}"), Some(2));
    // FE0F may be missing, spaces between emoji are fine.
    assert_eq!(emoji_only("\u{2764}"), Some(1));
    assert_eq!(
        emoji_only("  \u{1F600} \u{2764}\u{FE0F}\n\u{1F44D}  "),
        Some(3)
    );
    // Shortcodes and custom emoji count too, as in Fluxer.
    assert_eq!(emoji_only(":smile: :wave::skin-tone-2:"), Some(2));
    assert_eq!(
        emoji_only("<:party:123456789> <a:wave:42>\u{1F600}"),
        Some(3)
    );

    // Anything else is text.
    assert_eq!(emoji_only(""), None);
    assert_eq!(emoji_only("   "), None);
    assert_eq!(emoji_only("hello"), None);
    assert_eq!(emoji_only("hi \u{1F600}"), None);
    assert_eq!(emoji_only("\u{1F600}!"), None);
    assert_eq!(emoji_only("1"), None);
    assert_eq!(emoji_only("#"), None);
    assert_eq!(emoji_only(":not_an_emoji:"), None);
    assert_eq!(
        emoji_only("<:x:1>"),
        None,
        "custom names are at least two characters"
    );

    // The limit is Fluxer's 30.
    assert_eq!(MAX_JUMBO_EMOJI, 30);
    assert_eq!(
        emoji_only(&"\u{1F600}".repeat(MAX_JUMBO_EMOJI)),
        Some(MAX_JUMBO_EMOJI)
    );
    assert_eq!(emoji_only(&"\u{1F600}".repeat(MAX_JUMBO_EMOJI + 1)), None);
    // Long input is cut off early, not scanned to the end.
    assert_eq!(emoji_only(&"\u{1F600}".repeat(100_000)), None);
}
