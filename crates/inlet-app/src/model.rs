//! What the UI shows. The backend converts protocol types into these, so the views do
//! not depend on the wire format.

#[derive(Clone, Debug)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

impl Guild {
    /// Up to three initials for the guild rail ("Inlet Dev" -> "ID").
    pub fn initials(&self) -> String {
        let s: String = self
            .name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(3)
            .collect();
        if s.is_empty() { "?".into() } else { s }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Voice,
    Category,
    Dm,
    GroupDm,
    Other,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<String>,
    pub position: i64,
}

impl Channel {
    pub fn is_messageable(&self) -> bool {
        matches!(self.kind, ChannelKind::Text | ChannelKind::Dm | ChannelKind::GroupDm)
    }
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub filename: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    pub author: String,
    /// Already formatted for display, in local time.
    pub timestamp: String,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub edited: bool,
}

/// Orders channels the way the sidebar shows them: uncategorised first, then each
/// category followed by its children, all by position.
pub fn sidebar_order(channels: &[Channel]) -> Vec<Channel> {
    let mut by_pos = channels.to_vec();
    by_pos.sort_by_key(|c| (c.position, c.id.clone()));
    let mut out = Vec::with_capacity(by_pos.len());
    for c in by_pos.iter().filter(|c| c.parent_id.is_none() && c.kind != ChannelKind::Category) {
        out.push(c.clone());
    }
    for cat in by_pos.iter().filter(|c| c.kind == ChannelKind::Category) {
        out.push(cat.clone());
        for c in by_pos.iter().filter(|c| c.parent_id.as_deref() == Some(cat.id.as_str())) {
            out.push(c.clone());
        }
    }
    // Children whose category is unknown still show up.
    for c in by_pos.iter() {
        if c.kind != ChannelKind::Category && !out.iter().any(|o| o.id == c.id) {
            out.push(c.clone());
        }
    }
    out
}

/// Formats an RFC 3339 timestamp in local time; falls back to the raw text.
pub fn format_timestamp(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: &str, kind: ChannelKind, parent: Option<&str>, pos: i64) -> Channel {
        Channel { id: id.into(), name: id.into(), kind, parent_id: parent.map(Into::into), position: pos }
    }

    #[test]
    fn categories_group_their_children() {
        let list = vec![
            ch("b", ChannelKind::Text, Some("cat"), 1),
            ch("cat", ChannelKind::Category, None, 0),
            ch("a", ChannelKind::Text, None, 5),
            ch("orphan", ChannelKind::Text, Some("gone"), 0),
        ];
        let ids: Vec<_> = sidebar_order(&list).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["a", "cat", "b", "orphan"]);
    }

    #[test]
    fn initials() {
        assert_eq!(Guild { id: "1".into(), name: "Inlet Dev Hub".into() }.initials(), "IDH");
        assert_eq!(Guild { id: "1".into(), name: "".into() }.initials(), "?");
    }
}
