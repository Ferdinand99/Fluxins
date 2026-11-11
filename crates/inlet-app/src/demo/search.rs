// SPDX-License-Identifier: AGPL-3.0-or-later
//! The demo's answer to a message search: the demo messages of the searched guild or
//! direct message whose text has every word searched for, newest first. The scene
//! `search-results` searches `#general`'s guild once it is on screen; `quick-switcher`
//! opens the quick switcher, with the other demo accounts in the background.

use inlet_protocol as p;
use inlet_state::MessageId;

use super::MockSession;
use crate::backend::search::{page_of, Answer, Ev};

/// What the `search-results` scene types into the search field.
pub const SCENE_QUERY: &str = "build";

impl MockSession {
    pub fn search(&self, search: &p::MessageSearch, answer: Answer) {
        let channels: Vec<String> = match (&search.guild_id, &search.channel_id) {
            (Some(g), _) => self
                .channels
                .get(&Some(g.clone()))
                .map(|list| list.iter().map(|c| c.id.clone()).collect())
                .unwrap_or_default(),
            (None, Some(ch)) => vec![ch.clone()],
            (None, None) => Vec::new(),
        };
        let words: Vec<String> = search
            .content
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let messages = self.messages.lock().unwrap();
        let mut found: Vec<p::Message> = channels
            .iter()
            .filter(|ch| search.channel_ids.is_empty() || search.channel_ids.contains(ch))
            .filter_map(|ch| messages.get(ch))
            .flatten()
            .filter(|m| {
                let text = m.content.to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
                    && (search.author_ids.is_empty() || search.author_ids.contains(&m.author.id))
                    && (search.pinned != Some(true) || m.pinned)
            })
            .cloned()
            .collect();
        drop(messages);
        found.sort_by_key(|m| std::cmp::Reverse(MessageId::parse(&m.id)));
        if search.sort_order.as_deref() == Some("asc") {
            found.reverse();
        }
        let per = usize::from(search.hits_per_page.unwrap_or(25).max(1));
        let page = usize::from(search.page.unwrap_or(1).max(1));
        let total = found.len() as u64;
        let results = p::MessageSearchResults {
            messages: found.into_iter().skip((page - 1) * per).take(per).collect(),
            total,
            hits_per_page: per as u64,
            page: page as u64,
            ..Default::default()
        };
        let request = answer.request();
        answer.send(Ev::Results {
            request,
            page: page_of(&results, &self.names),
        });
    }
}
