// SPDX-License-Identifier: AGPL-3.0-or-later
//! Undo and redo. Each step is a snapshot of the whole buffer, which is cheap for a message
//! (a few thousand characters at most). Typing is grouped into words.

use crate::tokens::TokenSpan;

/// The most steps kept for undo.
const MAX_STEPS: usize = 200;

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub text: String,
    pub tokens: Vec<TokenSpan>,
}

/// One undo or redo step: the buffer to go back to, and where the caret was then.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub snapshot: Snapshot,
    pub cursor: usize,
}

/// The kind of an edit, used to decide whether it joins the open group.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    /// One typed character, inserted at `at` and ending at `end`.
    Typing { at: usize, end: usize, space: bool },
    /// A deletion of `start..end`.
    Delete { start: usize, end: usize },
    /// A step of an input method composition.
    Composition,
    /// Paste, token, replacement: always a step of its own.
    Other,
}

#[derive(Clone, Copy, Debug)]
enum Open {
    Typing { end: usize, last_space: bool },
    Delete { start: usize },
    Composition,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    open: Option<Open>,
}

impl History {
    /// Call before an edit. Pushes the state to return to, unless the edit joins the open group.
    pub fn begin(&mut self, kind: Kind, snapshot: impl FnOnce() -> Snapshot, cursor: usize) {
        let joins = match (self.open, kind) {
            (Some(Open::Typing { end, last_space }), Kind::Typing { at, space, .. }) => {
                // A word ends with the space after it: "hello " is one step, "world" the next.
                at == end && !(last_space && !space)
            }
            (Some(Open::Delete { start }), Kind::Delete { start: s, end: e }) => {
                e == start || s == start
            }
            (Some(Open::Composition), Kind::Composition) => true,
            _ => false,
        };
        if !joins {
            self.undo.push(Entry {
                snapshot: snapshot(),
                cursor,
            });
            if self.undo.len() > MAX_STEPS {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.open = match kind {
            Kind::Typing { end, space, .. } => Some(Open::Typing {
                end,
                last_space: space,
            }),
            Kind::Delete { start, .. } => Some(Open::Delete { start }),
            Kind::Composition => Some(Open::Composition),
            Kind::Other => None,
        };
    }

    /// The next edit starts a new step.
    pub fn close_group(&mut self) {
        self.open = None;
    }

    pub fn undo(&mut self, current: Snapshot, cursor: usize) -> Option<Entry> {
        let entry = self.undo.pop()?;
        self.redo.push(Entry {
            snapshot: current,
            cursor,
        });
        self.open = None;
        Some(entry)
    }

    /// Takes back the last step without keeping it for redo.
    pub fn pop_undo(&mut self) -> Option<Entry> {
        self.open = None;
        self.undo.pop()
    }

    pub fn redo(&mut self, current: Snapshot, cursor: usize) -> Option<Entry> {
        let entry = self.redo.pop()?;
        self.undo.push(Entry {
            snapshot: current,
            cursor,
        });
        self.open = None;
        Some(entry)
    }
}
