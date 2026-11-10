// SPDX-License-Identifier: AGPL-3.0-or-later
//! Code blocks: the box from Fluxer's `Markup.module.css` (`.codeContainer`), a copy
//! button on hover, and highlighting for a selection of languages with `syntect`
//! (MIT). Highlighting runs while parsing, on the backend thread, and stops at a size
//! and time budget; past it the rest of the block is plain.
//!
//! Deviations: Fluxer highlights with tree-sitter grammars (Arborium) for more
//! languages, in the GitHub Dark colours that `theme.rs` copies; KaTeX blocks (`tex`,
//! `latex`, `katex`) show as code with a "math" label instead of typeset maths; ANSI
//! blocks show their text without colours.

use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, svg, AnyElement, ClipboardItem, FontWeight, HighlightStyle, SharedString,
    StyledText,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::blocks::Draw;
use super::{Code, CodeColor};
use crate::theme;

/// Longest language name kept after the fence; longer ones are not a language.
pub(super) const MAX_LANGUAGE_CHARS: usize = 32;
/// Blocks larger than this are not highlighted.
const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024;
/// Highlighting stops at a line longer than this.
const MAX_HIGHLIGHT_LINE: usize = 1_024;
/// Time one block may take to highlight; the rest stays plain. Generous enough for
/// the first block of a language, which compiles that grammar's patterns.
const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(50);

/// Languages that are highlighted: the names people write after the fence, and the
/// file extension `syntect` knows the language by.
const LANGUAGES: &[(&str, &str)] = &[
    ("bash", "sh"),
    ("bat", "bat"),
    ("batch", "bat"),
    ("c", "c"),
    ("cc", "cpp"),
    ("clj", "clj"),
    ("clojure", "clj"),
    ("cpp", "cpp"),
    ("cs", "cs"),
    ("csharp", "cs"),
    ("css", "css"),
    ("d", "d"),
    ("diff", "diff"),
    ("erl", "erl"),
    ("erlang", "erl"),
    ("go", "go"),
    ("golang", "go"),
    ("groovy", "groovy"),
    ("h", "h"),
    ("haskell", "hs"),
    ("hpp", "cpp"),
    ("hs", "hs"),
    ("htm", "html"),
    ("html", "html"),
    ("java", "java"),
    ("javascript", "js"),
    ("js", "js"),
    ("json", "json"),
    ("jsx", "js"),
    ("lisp", "lisp"),
    ("lua", "lua"),
    ("markdown", "md"),
    ("md", "md"),
    ("objc", "m"),
    ("objectivec", "m"),
    ("ocaml", "ml"),
    ("patch", "diff"),
    ("perl", "pl"),
    ("php", "php"),
    ("pl", "pl"),
    ("py", "py"),
    ("python", "py"),
    ("r", "r"),
    ("rb", "rb"),
    ("rs", "rs"),
    ("ruby", "rb"),
    ("rust", "rs"),
    ("scala", "scala"),
    ("sh", "sh"),
    ("shell", "sh"),
    ("sql", "sql"),
    ("svg", "xml"),
    ("tcl", "tcl"),
    // Deviation: no TypeScript grammar ships with syntect; JavaScript colours it well.
    ("ts", "js"),
    ("tsx", "js"),
    ("typescript", "js"),
    ("xml", "xml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("zsh", "sh"),
];

/// Fluxer's `KATEX_CODE_BLOCK_LANGUAGES`.
const MATH_LANGUAGES: [&str; 3] = ["katex", "latex", "tex"];

/// Fluxer's `normaliseCodeBlockLanguage`: the first word, lower case. Names that are
/// too long or have odd characters are no language at all.
pub(super) fn normalise_language(language: Option<&str>) -> Option<String> {
    let first = language?.split_whitespace().next()?;
    if first.chars().count() > MAX_LANGUAGE_CHARS
        || !first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+#._-".contains(c))
    {
        return None;
    }
    Some(first.to_ascii_lowercase())
}

/// Parsing: a code block from the fence's language and its content.
pub(super) fn convert(language: Option<&str>, content: &str) -> Code {
    let language = normalise_language(language);
    let content = content.trim_end_matches('\n');
    let math = language
        .as_deref()
        .is_some_and(|l| MATH_LANGUAGES.contains(&l));
    let content = if language.as_deref() == Some("ansi") {
        strip_ansi(content)
    } else {
        content.to_string()
    };
    let spans = match language.as_deref() {
        Some(l) if !math => extension(l)
            .map(|ext| highlight(&content, ext))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Code {
        content: content.into(),
        language,
        math,
        spans,
    }
}

fn extension(language: &str) -> Option<&'static str> {
    LANGUAGES
        .binary_search_by(|(name, _)| (*name).cmp(language))
        .ok()
        .map(|i| LANGUAGES[i].1)
}

/// Removes ANSI escape sequences (`ESC [ … letter`) and other control characters but
/// line breaks and tabs.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Scope prefixes and their colour, most specific first.
fn classes() -> &'static [(Scope, CodeColor)] {
    static CLASSES: OnceLock<Vec<(Scope, CodeColor)>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        [
            ("comment", CodeColor::Comment),
            ("string", CodeColor::String),
            ("constant.character.escape", CodeColor::Constant),
            ("constant", CodeColor::Constant),
            ("keyword", CodeColor::Keyword),
            ("storage", CodeColor::Keyword),
            ("entity.name.function", CodeColor::Function),
            ("support.function", CodeColor::Function),
            ("variable.function", CodeColor::Function),
            ("meta.function-call", CodeColor::Function),
            ("entity.name.tag", CodeColor::Tag),
            ("entity.other.attribute-name", CodeColor::Constant),
            ("entity.name", CodeColor::Type),
            ("support.type", CodeColor::Type),
            ("support.class", CodeColor::Type),
            ("variable.language", CodeColor::Constant),
            ("variable.parameter", CodeColor::Type),
            ("markup.heading", CodeColor::Function),
            ("markup.inserted", CodeColor::Tag),
            ("markup.deleted", CodeColor::Keyword),
        ]
        .into_iter()
        .filter_map(|(prefix, color)| Scope::new(prefix).ok().map(|s| (s, color)))
        .collect()
    })
}

fn classify(stack: &ScopeStack) -> Option<CodeColor> {
    for scope in stack.as_slice().iter().rev() {
        for (prefix, color) in classes() {
            if prefix.is_prefix_of(*scope) {
                return Some(*color);
            }
        }
    }
    None
}

fn push_span(
    out: &mut Vec<(Range<usize>, CodeColor)>,
    range: Range<usize>,
    color: Option<CodeColor>,
) {
    let Some(color) = color else {
        return;
    };
    if range.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((last, c)) if *c == color && last.end == range.start => last.end = range.end,
        _ => out.push((range, color)),
    }
}

/// Coloured ranges of `content` in the language with file extension `ext`.
fn highlight(content: &str, ext: &str) -> Vec<(Range<usize>, CodeColor)> {
    if content.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    let set = syntaxes();
    let Some(syntax) = set.find_syntax_by_extension(ext) else {
        return Vec::new();
    };
    // The clock starts after the first line: a grammar's patterns are compiled the
    // first time they are used, which is not the input's doing.
    let mut started: Option<Instant> = None;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let over = started.is_some_and(|t| t.elapsed() > HIGHLIGHT_BUDGET);
        if line.len() > MAX_HIGHLIGHT_LINE || over {
            break;
        }
        let Ok(ops) = state.parse_line(line, set) else {
            break;
        };
        let mut last = 0;
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > last {
                push_span(&mut out, offset + last..offset + pos, classify(&stack));
                last = pos;
            }
            if stack.apply(&op).is_err() {
                return out;
            }
        }
        if line.len() > last {
            push_span(
                &mut out,
                offset + last..offset + line.len(),
                classify(&stack),
            );
        }
        offset += line.len();
        started.get_or_insert_with(Instant::now);
    }
    // Ranges must fall on character boundaries for drawing.
    out.retain(|(r, _)| content.is_char_boundary(r.start) && content.is_char_boundary(r.end));
    out
}

fn color(c: CodeColor) -> gpui::Hsla {
    match c {
        CodeColor::Keyword => theme::syntax_keyword(),
        CodeColor::String => theme::syntax_string(),
        CodeColor::Comment => theme::syntax_comment(),
        CodeColor::Constant => theme::syntax_constant(),
        CodeColor::Function => theme::syntax_function(),
        CodeColor::Type => theme::syntax_type(),
        CodeColor::Tag => theme::syntax_tag(),
    }
}

/// Group for showing the copy button on hover.
const CODE_GROUP: &str = "markdown-code";

/// Draws a code block (`.codeContainer`: 14 px padding, 6 px corners, a 1 px border,
/// monospace 12 px on 16 px lines).
pub(super) fn render(id: &str, code: &Code, _d: &Draw) -> AnyElement {
    let text =
        StyledText::new(code.content.clone()).with_highlights(code.spans.iter().map(|(r, c)| {
            (
                r.clone(),
                HighlightStyle {
                    color: Some(color(*c)),
                    ..Default::default()
                },
            )
        }));
    let content = code.content.clone();
    let copy = div()
        .id(SharedString::from(format!("{id}-copy")))
        .absolute()
        .top(px(4.))
        .right(px(4.))
        .p(px(4.))
        .rounded(px(4.))
        .bg(theme::bg_app())
        .cursor_pointer()
        .opacity(0.)
        .group_hover(CODE_GROUP, |s| s.opacity(1.))
        .child(
            svg()
                .data(include_bytes!("../../../assets/icons/markdown/clipboard.svg").as_slice())
                .size(px(16.))
                .text_color(theme::text_muted()),
        )
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(content.to_string()));
        });
    let label = code.math.then(|| {
        div()
            .mb(px(4.))
            .font_family(crate::assets::ui_font())
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::text_muted())
            .child(t!("markdown-math"))
    });
    div()
        .id(SharedString::from(id.to_string()))
        .group(CODE_GROUP)
        .relative()
        .w_full()
        .bg(theme::code_bg())
        .border_1()
        .border_color(theme::code_border())
        .rounded(px(6.))
        .p(px(14.))
        .font_family(super::mono_font())
        .text_size(px(12.))
        .line_height(px(16.))
        .text_color(theme::code_text())
        .children(label)
        .child(text)
        .child(copy)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_block_language_is_capped_and_unknown_falls_back() {
        // Too long: no language at all, and the text is untouched.
        let long = "a".repeat(MAX_LANGUAGE_CHARS + 1);
        let code = convert(Some(&long), "let x = 1;\n");
        assert_eq!(code.language, None);
        assert!(code.spans.is_empty());
        assert_eq!(code.content.as_ref(), "let x = 1;");
        // Odd characters: no language.
        assert_eq!(normalise_language(Some("rust\u{202e}")), None);
        assert_eq!(normalise_language(Some("<script>")), None);
        // Fluxer's normalisation: the first word, lower case.
        assert_eq!(
            normalise_language(Some("Rust title=x")).as_deref(),
            Some("rust")
        );
        // Unknown: plain text.
        let code = convert(Some("klingon"), "Qapla'");
        assert_eq!(code.language.as_deref(), Some("klingon"));
        assert!(code.spans.is_empty());
        // Known: coloured.
        let code = convert(Some("rust"), "fn main() { let s = \"hi\"; } // done");
        assert!(code
            .spans
            .iter()
            .any(|(r, c)| *c == CodeColor::Keyword && &code.content[r.clone()] == "fn"));
        assert!(code.spans.iter().any(|(_, c)| *c == CodeColor::Comment));
        assert!(code.spans.iter().any(|(_, c)| *c == CodeColor::String));
        // Maths is code with a label, not highlighted.
        let code = convert(Some("latex"), "\\frac{a}{b}");
        assert!(code.math && code.spans.is_empty());
    }

    #[test]
    fn every_listed_language_has_a_grammar() {
        let mut sorted = LANGUAGES.to_vec();
        sorted.sort();
        assert_eq!(sorted, LANGUAGES, "the table is searched by binary search");
        for (name, ext) in LANGUAGES {
            assert!(
                syntaxes().find_syntax_by_extension(ext).is_some(),
                "{name}: {ext}"
            );
        }
    }

    #[test]
    fn highlighting_stops_at_the_budget() {
        let huge = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 11 + 10);
        assert!(convert(Some("rust"), &huge).spans.is_empty());
        let long_line = format!(
            "fn a() {{}}\n{}\nfn b() {{}}",
            "x".repeat(MAX_HIGHLIGHT_LINE + 1)
        );
        let code = convert(Some("rust"), &long_line);
        let first_line = long_line.find('\n').unwrap();
        assert!(code.spans.iter().all(|(r, _)| r.end <= first_line + 1));
    }

    #[test]
    fn ansi_codes_are_stripped() {
        let code = convert(Some("ansi"), "\u{1b}[31mred\u{1b}[0m text\u{7}");
        assert_eq!(code.content.as_ref(), "red text");
    }
}
