// SPDX-License-Identifier: AGPL-3.0-or-later
//! The member search on top of the member list. Typing filters the members loaded
//! at once; after a pause of [`PAUSE`] the guild is searched on the server (op 8,
//! names starting with the text), and its answers join the list. While text is in
//! the field the results replace the grouped list.
//!
//! Budget: one op 8 per pause in typing (never twice for the same text); the
//! protocol sends at most 10 per 10 s and keeps only the newest 4 waiting.

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, FontWeight, Task};
use inlet_protocol as p;
use inlet_state::GuildId;

use super::list::MemberRow;
use super::MemberIcon;
use crate::app::Inlet;
use crate::backend::social::members::Cmd;
use crate::text_input::TextInput;
use crate::theme;

/// Pause in typing before the server is asked.
pub const PAUSE: Duration = Duration::from_millis(400);
/// Results shown at most.
const MAX_RESULTS: usize = 50;
/// Characters of a query.
const MAX_QUERY_CHARS: usize = 100;

/// The search's state.
#[derive(Default)]
pub struct Search {
    pub input: Option<Entity<TextInput>>,
    /// The text as last read from the field.
    query: String,
    typed_at: Option<Instant>,
    /// The text last sent to the server, and the nonce of its answer.
    asked: Option<(String, String)>,
    next_nonce: u64,
    /// Members the server found for `asked`.
    found: Vec<MemberRow>,
    /// Redraws once the pause is over.
    wake: Option<Task<()>>,
}

impl Search {
    /// Whether text is in the field.
    pub fn active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Another channel or account: the results go, the field keeps nothing.
    pub fn clear(&mut self) {
        self.query.clear();
        self.typed_at = None;
        self.asked = None;
        self.found.clear();
        self.wake = None;
        // The field is emptied on the next frame (it needs its context).
        self.input = None;
    }

    /// A GUILD_MEMBERS_CHUNK: kept when it answers the newest search.
    pub fn on_chunk(&mut self, chunk: &p::models::GuildMembersChunk) -> bool {
        let Some((_, nonce)) = &self.asked else {
            return false;
        };
        if chunk.nonce.as_deref() != Some(nonce.as_str()) {
            return false;
        }
        let presences: Vec<(&str, &str)> = chunk
            .presences
            .iter()
            .filter_map(|p| Some((p.user_id()?, p.status.as_str())))
            .collect();
        for m in chunk.members.iter().take(MAX_RESULTS) {
            let Some(mut row) = MemberRow::of(m) else {
                continue;
            };
            if let Some((_, s)) = presences
                .iter()
                .find(|(id, _)| *id == row.user_id.to_string())
            {
                row.status = super::list::normalize_status(s);
            }
            if !self.found.iter().any(|f| f.user_id == row.user_id) {
                self.found.push(row);
            }
        }
        self.found.truncate(MAX_RESULTS);
        true
    }
}

/// Whether a member matches the text (name or account name, ignoring case).
pub fn matches(m: &MemberRow, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    !q.is_empty() && (m.name.to_lowercase().contains(&q) || m.username.to_lowercase().contains(&q))
}

impl Inlet {
    /// Once per frame: reads the field and asks the server after a pause.
    pub(super) fn search_frame(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.members.search.input.clone() else {
            return;
        };
        let text: String = input
            .read(cx)
            .text()
            .chars()
            .take(MAX_QUERY_CHARS)
            .collect();
        let s = &mut self.members.search;
        if text != s.query {
            s.query = text;
            s.typed_at = Some(Instant::now());
            s.found.clear();
            s.asked = None;
            // Redraw when the pause is over, to send the search.
            s.wake = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(PAUSE).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            }));
        }
        let q = s.query.trim().to_string();
        let paused = s.typed_at.is_some_and(|t| t.elapsed() >= PAUSE);
        if q.is_empty() || !paused || s.asked.as_ref().is_some_and(|(a, _)| *a == q) {
            return;
        }
        let Some((guild, _)) = self.member_list_shown() else {
            return;
        };
        let s = &mut self.members.search;
        s.next_nonce += 1;
        let nonce = format!("ml{}", s.next_nonce);
        s.asked = Some((q.clone(), nonce.clone()));
        self.backend.send(Cmd::Search {
            guild_id: guild.to_string(),
            query: q,
            nonce,
        });
    }

    /// The search field.
    pub(super) fn render_member_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let input = self
            .members
            .search
            .input
            .get_or_insert_with(|| {
                cx.new(|cx| {
                    TextInput::new(t!("members-search-placeholder"), cx).max_chars(MAX_QUERY_CHARS)
                })
            })
            .clone();
        div()
            .flex_none()
            .px(px(8.))
            .pt(px(8.))
            .pb(px(4.))
            .child(
                div().relative().flex().items_center().child(input).child(
                    div()
                        .absolute()
                        .right(px(10.))
                        .child(MemberIcon::Search.el(px(16.), theme::text_tertiary())),
                ),
            )
            .into_any_element()
    }

    /// The members matching the search: the loaded ones first, then the server's.
    pub(super) fn render_member_search_results(
        &mut self,
        guild: GuildId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.members.search.query.clone();
        let mut rows: Vec<MemberRow> = self
            .members
            .list
            .as_ref()
            .map(|l| {
                l.members()
                    .filter(|m| matches(m, &query))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for f in &self.members.search.found {
            if !rows.iter().any(|r| r.user_id == f.user_id) {
                rows.push(f.clone());
            }
        }
        rows.truncate(MAX_RESULTS);
        let searching = self.members.search.asked.is_none() || self.members.search.found.is_empty();
        let mut out = div()
            .id("member-search-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        if rows.is_empty() {
            let label = if searching {
                t!("members-searching")
            } else {
                t!("members-no-results")
            };
            out = out.child(
                div()
                    .px(px(8.))
                    .py(px(12.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_tertiary())
                    .child(label),
            );
        }
        for (i, m) in rows.iter().enumerate() {
            out = out.child(self.render_member_row(&format!("s{i}"), guild, m, cx));
        }
        out.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, name: &str) -> MemberRow {
        let m: p::GuildMember =
            serde_json::from_value(json!({"user": {"id": id, "username": name}, "roles": []}))
                .unwrap();
        MemberRow::of(&m).unwrap()
    }

    #[test]
    fn only_the_newest_search_answer_is_kept() {
        let mut s = Search::default();
        let chunk = |nonce: &str| -> p::models::GuildMembersChunk {
            serde_json::from_value(json!({
                "guild_id": "1", "nonce": nonce,
                "members": [{"user": {"id": "5", "username": "ada"}, "roles": []}],
                "presences": [{"user": {"id": "5"}, "status": "idle"}]
            }))
            .unwrap()
        };
        assert!(!s.on_chunk(&chunk("ml1")));
        s.asked = Some(("ad".into(), "ml2".into()));
        assert!(!s.on_chunk(&chunk("ml1")));
        assert!(s.on_chunk(&chunk("ml2")));
        assert!(s.on_chunk(&chunk("ml2")));
        assert_eq!(s.found.len(), 1);
        assert_eq!(s.found[0].status, "idle");
        assert!(matches(&row("6", "Grace"), " gra"));
        assert!(!matches(&row("6", "Grace"), " "));
    }
}
