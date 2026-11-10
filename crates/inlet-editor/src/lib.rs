// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message composer's text model and gpui element.
//!
//! The text is plain. Mentions and custom emoji are *tokens*: a span of the display text
//! that shows as a pill and is sent as `<@id>`, `<#id>` or `<:name:id>`. See [`Buffer`].
//! [`Editor`] and [`EditorElement`] put it on screen, with wrapping, selection and input
//! method support.
//!
//! # Sending a message
//!
//! The composer MUST send `allowed_mentions` built from [`Buffer::allowed_mentions`] with
//! every message it sends or edits: the picked users and roles as lists, and `parse:
//! ["everyone"]` only when `everyone` is set (see [`AllowedMentions`] for the case with both).
//! [`Buffer::to_wire`] escapes typed text that looks like a mention, but the escapes depend on
//! how the server parses today. `allowed_mentions` is the guard that does not: the server
//! drops every user and role mention that is not on the list. The limit given to
//! [`Buffer::new`] must be the instance's message length limit, in UTF-16 code units.
//!
//! # Limits
//! gpui shapes text in logical order and does no bidirectional reordering, so
//! right-to-left text (Arabic, Hebrew) is shown in the wrong order and the caret and the
//! selection follow that order. The web client gets this from the browser. It cannot be
//! fixed in this crate.

#![forbid(unsafe_code)]

mod buffer;
mod element;
mod fragment;
mod history;
mod sanitize;
mod selection;
mod tokens;
mod trigger;
mod wire;

pub use buffer::{Buffer, TooLong};
pub use element::{
    bind_keys, Editor, EditorElement, EditorEvent, EditorStyle, Newline, Submit, KEY_CONTEXT,
};

/// The actions the editor handles, for key bindings of your own and for tests.
pub mod actions {
    pub use crate::element::{
        Backspace, Copy, Cut, Delete, DeleteWordLeft, DeleteWordRight, DocEnd, DocStart, Down, End,
        Home, Left, Newline, Paste, Redo, Right, SelectAll, SelectDocEnd, SelectDocStart,
        SelectDown, SelectEnd, SelectHome, SelectLeft, SelectRight, SelectUp, SelectWordLeft,
        SelectWordRight, Submit, Undo, Up, WordLeft, WordRight,
    };
}
pub use fragment::Fragment;
pub use selection::{
    line_end, line_range_at, line_start, nearest_grapheme_boundary, next_grapheme, next_word,
    prev_backspace_boundary, prev_grapheme, prev_word, word_range_at, Move, Selection,
};
pub use tokens::{Id, NoNames, Resolve, Token, TokenSpan};
pub use trigger::{trigger_at, Trigger};
pub use wire::{server_mentions, AllowedMentions, Mentions};
