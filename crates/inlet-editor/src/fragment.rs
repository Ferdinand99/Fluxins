// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text with tokens, for copy and paste inside Inlet.
//!
//! Other apps get the plain text. The clipboard item also carries a small description of the
//! tokens, so a pill that is cut and pasted stays a pill.

use crate::sanitize::sanitize_input;
use crate::tokens::{Token, TokenSpan};

const HEADER: &str = "inlet-editor-fragment 1";

/// A piece of the buffer. The ranges of the tokens are relative to `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub text: String,
    pub tokens: Vec<TokenSpan>,
}

impl Fragment {
    /// The description of the tokens for the clipboard, or `None` when there are none.
    pub fn to_metadata(&self) -> Option<String> {
        if self.tokens.is_empty() {
            return None;
        }
        let mut out = String::from(HEADER);
        for t in &self.tokens {
            out.push('\n');
            out.push_str(&format!(
                "{} {} {}",
                t.range.start,
                t.range.end,
                t.token.to_wire()
            ));
        }
        Some(out)
    }

    /// Reads a fragment back from clipboard text and its description. Anything that does not
    /// fit the text exactly gives `None`, and the caller pastes the text as plain text.
    pub fn from_clipboard(text: &str, metadata: &str) -> Option<Fragment> {
        let mut lines = metadata.lines();
        if lines.next()? != HEADER {
            return None;
        }
        if sanitize_input(text) != text {
            return None;
        }
        let mut tokens: Vec<TokenSpan> = Vec::new();
        for line in lines {
            let mut parts = line.splitn(3, ' ');
            let start: usize = parts.next()?.parse().ok()?;
            let end: usize = parts.next()?.parse().ok()?;
            let wire = parts.next()?;
            let (token, used) = Token::parse_wire(wire)?;
            let in_order = tokens.last().is_none_or(|last| last.range.end <= start);
            if used != wire.len()
                || start >= end
                || end > text.len()
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
                || !in_order
            {
                return None;
            }
            tokens.push(TokenSpan {
                range: start..end,
                token,
            });
        }
        Some(Fragment {
            text: text.to_owned(),
            tokens,
        })
    }
}
