// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message composer's text model and gpui element.
//!
//! The text is plain. Mentions and custom emoji are *tokens*: a span of the display text
//! that shows as a pill and is sent as `<@id>`, `<#id>` or `<:name:id>`. See [`Buffer`].
//! [`Editor`] and [`EditorElement`] put it on screen, with wrapping, selection and input
//! method support.

#![forbid(unsafe_code)]

mod buffer;
mod element;
mod history;
mod selection;
mod tokens;
mod trigger;

pub use buffer::{Buffer, TooLong};
pub use element::{
    bind_keys, Editor, EditorElement, EditorEvent, EditorStyle, Newline, Submit, KEY_CONTEXT,
};
pub use selection::{
    line_end, line_range_at, line_start, next_grapheme, next_word, prev_grapheme, prev_word,
    word_range_at, Move, Selection,
};
pub use tokens::{Id, NoNames, Resolve, Token, TokenSpan};
pub use trigger::{trigger_at, Trigger};
