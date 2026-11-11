// SPDX-License-Identifier: AGPL-3.0-or-later
//! Code blocks: the box from Fluxer's `Markup.module.css` (`.codeContainer`), a copy
//! button on hover, and highlighting for a selection of languages with `syntect`
//! (MIT).
//!
//! Parsing never highlights. It only decides which blocks may be highlighted: a known
//! language, a block of at most [`MAX_HIGHLIGHT_BYTES`], and room left in the
//! message's budget ([`MAX_MESSAGE_HIGHLIGHT_BYTES`] for its text and all its embeds).
//! The first time such a block is drawn, it is highlighted once off the UI thread
//! (`RenderCtx::off_thread`) and the result is kept in the block, which lives in the
//! converted message. Until then it is plain monospace. Each run stops at a time
//! budget and at a line longer than [`MAX_HIGHLIGHT_LINE`]; the rest of the block
//! stays plain.
//!
//! Deviations: Fluxer highlights with tree-sitter grammars (Arborium) for more
//! languages, in the GitHub Dark colours that `theme.rs` copies; KaTeX blocks (`tex`,
//! `latex`, `katex`) show as code with a "math" label instead of typeset maths; ANSI
//! blocks show their text without colours.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, svg, AnyElement, ClipboardItem, FontWeight, HighlightStyle, SharedString,
    StyledText,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::blocks::Draw;
use super::{Code, CodeColor};
use crate::app::Inlet;
use crate::theme;

/// Longest language name kept after the fence; longer ones are not a language.
pub(super) const MAX_LANGUAGE_CHARS: usize = 32;
/// Blocks larger than this are not highlighted.
pub(super) const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024;
/// Code highlighted per message, its text and all its embeds together; blocks past it
/// stay plain.
pub(super) const MAX_MESSAGE_HIGHLIGHT_BYTES: usize = 64 * 1024;
/// Highlighting stops at a line longer than this, the first line too: one line is one
/// call into the grammar, and the clock is only read between lines.
pub(super) const MAX_HIGHLIGHT_LINE: usize = 512;
/// Time one block may take to highlight, counted from before its first line; the rest
/// stays plain.
const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(25);
/// Coloured ranges kept per block; the rest of the block is plain.
const MAX_SPANS: usize = 4_096;

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
    let highlight = match language.as_deref() {
        Some(l) if !math && content.len() <= MAX_HIGHLIGHT_BYTES => extension(l)
            .filter(|_| take_budget(content.len()))
            .map(|ext| Arc::new(Highlight::new(ext))),
        _ => None,
    };
    Code {
        content: content.into(),
        language,
        math,
        highlight,
    }
}

thread_local! {
    /// Bytes of code the message being parsed on this thread may still highlight.
    static MESSAGE_BUDGET: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Runs `f` with one highlighting budget for everything it parses: a message's text and
/// all its embeds. Nested calls share the outer budget.
pub fn with_message_budget<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            MESSAGE_BUDGET.with(|b| b.set(None));
        }
    }
    if MESSAGE_BUDGET.with(Cell::get).is_some() {
        return f();
    }
    MESSAGE_BUDGET.with(|b| b.set(Some(MAX_MESSAGE_HIGHLIGHT_BYTES)));
    let _reset = Reset;
    f()
}

/// Takes `len` bytes from the message's budget; `false` when not enough is left.
fn take_budget(len: usize) -> bool {
    MESSAGE_BUDGET.with(|b| match b.get() {
        Some(left) if left >= len => {
            b.set(Some(left - len));
            true
        }
        Some(_) => false,
        None => len <= MAX_MESSAGE_HIGHLIGHT_BYTES,
    })
}

/// One coloured range of a code block (byte offsets; a block is at most
/// [`MAX_HIGHLIGHT_BYTES`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub color: CodeColor,
}

impl Span {
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// The colours of one code block, worked out once, off the UI thread, the first time
/// the block is drawn. Clones of the block share it.
#[derive(Debug)]
pub struct Highlight {
    /// The file extension `syntect` knows the language by.
    ext: &'static str,
    /// A run was started (only one ever is).
    started: AtomicBool,
    spans: OnceLock<Box<[Span]>>,
}

impl Highlight {
    fn new(ext: &'static str) -> Self {
        Highlight {
            ext,
            started: AtomicBool::new(false),
            spans: OnceLock::new(),
        }
    }

    /// The colours, once they are worked out.
    pub fn spans(&self) -> Option<&[Span]> {
        self.spans.get().map(|s| &s[..])
    }

    /// Works out the colours of `content` (its block's text) and keeps them. Slow:
    /// never on the UI thread.
    pub fn run(&self, content: &str) {
        // Loading the syntax set and compiling the grammar is not the input's doing:
        // it happens first, on a fixed sample, and the clock starts after it.
        warm(self.ext);
        let spans = highlight(content, self.ext, Instant::now() + HIGHLIGHT_BUDGET);
        let _ = self.spans.set(spans.into_boxed_slice());
    }

    /// Whether this call is the one that starts the run.
    fn claim(&self) -> bool {
        !self.started.swap(true, Ordering::AcqRel)
    }
}

/// Runs a job off the UI thread, then draws the chat again.
pub type OffThread = std::rc::Rc<dyn Fn(Box<dyn FnOnce() + Send>)>;

impl Inlet {
    /// [`OffThread`] for the chat: the job runs on gpui's background executor.
    pub(crate) fn markdown_off_thread(&self, cx: &mut gpui::Context<Self>) -> OffThread {
        let app = cx.to_async();
        let weak = cx.entity().downgrade();
        std::rc::Rc::new(move |job| {
            let weak = weak.clone();
            app.spawn(async move |cx| {
                let task = cx.background_executor().spawn(async move { job() });
                task.await;
                let _ = weak.update(cx, |_, cx| cx.notify());
            })
            .detach();
        })
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

/// A few common tokens (numbers, a string, comments, operators): highlighting it
/// compiles the patterns most blocks use.
const WARM_SAMPLE: &str = "1 = (\"2\", 3) // 4\n# 5\n";

/// Loads the syntax set and compiles the main patterns of the grammar for `ext`, once,
/// on [`WARM_SAMPLE`], so a block's own time budget is spent on the block.
fn warm(ext: &'static str) {
    static WARMED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let warmed = WARMED.get_or_init(Default::default);
    if warmed.lock().map_or(true, |w| w.contains(ext)) {
        return;
    }
    highlight(WARM_SAMPLE, ext, Instant::now() + Duration::from_secs(1));
    if let Ok(mut w) = warmed.lock() {
        w.insert(ext);
    }
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

fn push_span(out: &mut Vec<Span>, start: usize, end: usize, color: Option<CodeColor>) {
    let Some(color) = color else {
        return;
    };
    if start >= end {
        return;
    }
    let (start, end) = (start as u32, end as u32);
    match out.last_mut() {
        Some(last) if last.color == color && last.end == start => last.end = end,
        _ => out.push(Span { start, end, color }),
    }
}

/// Coloured ranges of `content` in the language with file extension `ext`. Stops at
/// `deadline` (checked before every line, the first too), at a line longer than
/// [`MAX_HIGHLIGHT_LINE`] and at [`MAX_SPANS`] ranges.
fn highlight(content: &str, ext: &str, deadline: Instant) -> Vec<Span> {
    if content.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    let set = syntaxes();
    let Some(syntax) = set.find_syntax_by_extension(ext) else {
        return Vec::new();
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if line.len() > MAX_HIGHLIGHT_LINE || Instant::now() >= deadline || out.len() >= MAX_SPANS {
            break;
        }
        let Ok(ops) = state.parse_line(line, set) else {
            break;
        };
        let mut last = 0;
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > last {
                push_span(&mut out, offset + last, offset + pos, classify(&stack));
                last = pos;
            }
            if stack.apply(&op).is_err() {
                return finish(content, out);
            }
        }
        if line.len() > last {
            push_span(
                &mut out,
                offset + last,
                offset + line.len(),
                classify(&stack),
            );
        }
        offset += line.len();
    }
    finish(content, out)
}

/// Ranges must fall on character boundaries for drawing; at most [`MAX_SPANS`].
fn finish(content: &str, mut out: Vec<Span>) -> Vec<Span> {
    out.truncate(MAX_SPANS);
    out.retain(|s| {
        content.is_char_boundary(s.start as usize) && content.is_char_boundary(s.end as usize)
    });
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
pub(super) fn render(id: &str, code: &Code, d: &Draw) -> AnyElement {
    let spans = match &code.highlight {
        Some(h) => match h.spans() {
            Some(spans) => spans,
            None => {
                // First drawn: highlight off the UI thread; plain until it lands.
                if let Some(off_thread) = &d.ctx.off_thread {
                    if h.claim() {
                        let (h, content) = (h.clone(), code.content.clone());
                        off_thread(Box::new(move || h.run(&content)));
                    }
                }
                &[]
            }
        },
        None => &[],
    };
    let text = StyledText::new(code.content.clone()).with_highlights(spans.iter().map(|s| {
        (
            s.range(),
            HighlightStyle {
                color: Some(color(s.color)),
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
    use super::super::{parse, Block};
    use super::*;

    /// Highlights `code` here, as the background job would.
    fn spans_of(code: &Code) -> Vec<Span> {
        match &code.highlight {
            Some(h) => {
                h.run(&code.content);
                h.spans().expect("worked out").to_vec()
            }
            None => Vec::new(),
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn code_block_language_is_capped_and_unknown_falls_back() {
        // Too long: no language at all, and the text is untouched.
        let long = "a".repeat(MAX_LANGUAGE_CHARS + 1);
        let code = convert(Some(&long), "let x = 1;\n");
        assert_eq!(code.language, None);
        assert!(code.highlight.is_none());
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
        assert!(code.highlight.is_none());
        // Known: coloured, but only once the block is drawn, never while parsing.
        let code = convert(Some("rust"), "fn main() { let s = \"hi\"; } // done");
        assert!(code.highlight.as_ref().is_some_and(|h| h.spans().is_none()));
        let spans = spans_of(&code);
        assert!(spans
            .iter()
            .any(|s| s.color == CodeColor::Keyword && &code.content[s.range()] == "fn"));
        assert!(spans.iter().any(|s| s.color == CodeColor::Comment));
        assert!(spans.iter().any(|s| s.color == CodeColor::String));
        // Maths is code with a label, not highlighted.
        let code = convert(Some("latex"), "\\frac{a}{b}");
        assert!(code.math && code.highlight.is_none());
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
    fn highlighting_stops_at_its_time_and_size_limits() {
        // Time: a deadline that has passed stops before the first line.
        assert!(highlight("fn a() {}\nfn b() {}\n", "rs", Instant::now()).is_empty());
        assert!(!highlight("fn a() {}\n", "rs", later()).is_empty());
        // A block too large is never highlighted.
        let huge = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 11 + 10);
        assert!(convert(Some("rust"), &huge).highlight.is_none());
        assert!(highlight(&huge, "rs", later()).is_empty());
        // A long first line: nothing at all.
        let long = "x".repeat(MAX_HIGHLIGHT_LINE + 1);
        assert!(highlight(&format!("{long}\nfn a() {{}}"), "rs", later()).is_empty());
        // A long line later: only what comes before it.
        let text = format!("fn a() {{}}\n{long}\nfn b() {{}}");
        let first_line = text.find('\n').unwrap();
        let spans = highlight(&text, "rs", later());
        assert!(!spans.is_empty());
        assert!(spans.iter().all(|s| s.range().end <= first_line + 1));
        // Ranges per block are capped.
        let line = format!("{}\n", "1 x ".repeat(MAX_HIGHLIGHT_LINE / 4 - 1));
        let busy = line.repeat(MAX_HIGHLIGHT_BYTES / line.len());
        assert!(highlight(&busy, "rs", later()).len() <= MAX_SPANS);
    }

    #[test]
    fn a_message_highlights_at_most_its_budget() {
        // An embed text is at most 4 096 characters; a message has up to 10 embeds
        // with 25 fields each. Blocks of about 4 KiB: 16 fit in the message's 64 KiB.
        let block = "let x = 1;\n".repeat(360);
        let text = format!("```rust\n{block}```");
        let eligible = |blocks: &[Block]| match &blocks[0] {
            Block::Code(c) => c.highlight.is_some(),
            other => panic!("{other:?}"),
        };
        let fits = MAX_MESSAGE_HIGHLIGHT_BYTES / (block.len() - 1);
        assert_eq!(fits, 16);
        let message: Vec<bool> =
            with_message_budget(|| (0..20).map(|_| eligible(&parse(&text))).collect());
        assert!(message[..fits].iter().all(|e| *e));
        assert!(message[fits..].iter().all(|e| !e));
        // The next message has its own budget.
        assert!(with_message_budget(|| eligible(&parse(&text))));
        assert!(eligible(&parse(&text)));
    }

    #[test]
    fn ansi_codes_are_stripped() {
        let code = convert(Some("ansi"), "\u{1b}[31mred\u{1b}[0m text\u{7}");
        assert_eq!(code.content.as_ref(), "red text");
    }
}
