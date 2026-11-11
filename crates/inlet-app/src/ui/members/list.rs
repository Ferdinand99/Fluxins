// SPDX-License-Identifier: AGPL-3.0-or-later
//! The member list of the open channel as the server sends it (GUILD_MEMBER_LIST_UPDATE),
//! and which rows to ask for.
//!
//! The server keeps a list per channel: group headers (hoisted roles, then `online`,
//! then `offline`) with their members under them. The client subscribes to ranges of
//! rows (op 14) and gets those rows, and every later change to them. Only the rows on
//! screen are asked for ([`window`]), in pages of 100 like Fluxer, and only rows inside
//! the subscribed ranges are kept, so a server cannot fill memory with rows nobody asked
//! for. The list holds a compact copy of each member (name, roles, avatar, status), at
//! most [`MAX_RANGES`] x [`PAGE`] rows.

use std::collections::BTreeMap;

use inlet_protocol as p;
use inlet_state::{ChannelId, GuildId, RoleId, UserId};

use crate::model::{self, UserAvatar};

/// Rows per subscribed range (Fluxer's `MEMBER_LIST_RANGE_PAGE_SIZE`).
pub const PAGE: u32 = 100;
/// Highest row the server serves (`MEMBER_LIST_RANGE_MAX_END`).
pub const MAX_END: u32 = 100_000;
/// Ranges subscribed at once: the first page and the pages on screen.
pub const MAX_RANGES: usize = 3;
/// Rows above and below the screen that count as visible, so the next page is asked
/// for a little before it scrolls in.
pub const BUFFER_ROWS: u32 = 20;
/// Ranges whose rows are kept while subscriptions are on their way.
const MAX_KEPT_RANGES: usize = 10;
/// Groups kept from one update (hoisted roles plus online and offline).
const MAX_GROUPS: usize = 256;
/// Roles kept per member (for the name colour and the profile).
const MAX_ROLES: usize = 64;
/// Characters of a custom status kept.
const MAX_STATUS_CHARS: usize = 128;

/// One member, as the list draws them.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberRow {
    pub user_id: UserId,
    /// Nickname, display name or username, cleaned.
    pub name: String,
    /// Account name, cleaned.
    pub username: String,
    pub roles: Vec<RoleId>,
    pub avatar: Option<UserAvatar>,
    pub bot: bool,
    /// `online`, `idle`, `dnd` or `offline` (unknown values count as offline).
    pub status: &'static str,
    /// The custom status text, cleaned.
    pub custom_status: Option<String>,
}

/// One row of the list.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// A group header: a hoisted role's ID, `online` or `offline`, and its count.
    Group {
        id: String,
        count: u64,
    },
    Member(Box<MemberRow>),
}

/// A status as the list knows it.
pub fn normalize_status(s: &str) -> &'static str {
    match s {
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

impl MemberRow {
    /// The row of a member list item; `None` without a usable user.
    pub fn of(m: &p::GuildMember) -> Option<MemberRow> {
        let user = m.user.as_ref()?;
        let user_id = UserId::parse(&user.id)?;
        let name = m.display_name().unwrap_or_else(|| user.display_name());
        let presence = m.presence();
        let custom_status = presence
            .as_ref()
            .and_then(|p| p.custom_status.as_ref())
            .and_then(|c| c.text.as_deref())
            .and_then(model::clean_text)
            .map(|t| t.chars().take(MAX_STATUS_CHARS).collect::<String>());
        Some(MemberRow {
            user_id,
            name: model::clean_name(name),
            username: model::clean_name(&user.username),
            roles: m
                .roles
                .iter()
                .filter_map(|r| RoleId::parse(r))
                .take(MAX_ROLES)
                .collect(),
            avatar: crate::backend::media::user_avatar(user),
            bot: user.bot,
            status: normalize_status(presence.as_ref().map_or("", |p| p.status.as_str())),
            custom_status,
        })
    }
}

impl Row {
    fn of(item: &p::MemberListItem) -> Option<Row> {
        Some(match item {
            p::MemberListItem::Group(g) => Row::Group {
                id: g.id.chars().take(32).collect(),
                count: g.count,
            },
            p::MemberListItem::Member(m) => Row::Member(Box::new(MemberRow::of(m)?)),
        })
    }
}

/// The member list of one channel.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberList {
    pub guild: GuildId,
    pub channel: ChannelId,
    pub member_count: u64,
    pub online_count: u64,
    /// Group headers in order, with their counts.
    pub groups: Vec<(String, u64)>,
    /// Rows by position. Only rows inside `ranges` are kept.
    rows: BTreeMap<u32, Row>,
    /// Subscribed ranges, inclusive.
    ranges: Vec<[u32; 2]>,
    /// Whether an update came for the list yet.
    pub loaded: bool,
}

impl MemberList {
    pub fn new(guild: GuildId, channel: ChannelId) -> Self {
        MemberList {
            guild,
            channel,
            member_count: 0,
            online_count: 0,
            groups: Vec::new(),
            rows: BTreeMap::new(),
            ranges: vec![[0, PAGE - 1]],
            loaded: false,
        }
    }

    /// Rows in the list: every group header and the members under them (the server
    /// leaves out an offline group of more than 1,000 members, and its rows with it).
    pub fn len(&self) -> u32 {
        let total = if self.groups.is_empty() {
            self.member_count
        } else {
            self.groups
                .iter()
                .fold(self.groups.len() as u64, |n, (_, c)| n.saturating_add(*c))
        };
        total.min(u64::from(MAX_END) + 1) as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A loaded row.
    pub fn row(&self, ix: u32) -> Option<&Row> {
        self.rows.get(&ix)
    }

    /// The loaded members, in list order.
    pub fn members(&self) -> impl Iterator<Item = &MemberRow> {
        self.rows.values().filter_map(|r| match r {
            Row::Member(m) => Some(m.as_ref()),
            Row::Group { .. } => None,
        })
    }

    /// Whether row `ix` is a group header, as far as the groups tell (they give every
    /// header's position even when the row is not loaded).
    pub fn is_header(&self, ix: u32) -> bool {
        if let Some(row) = self.rows.get(&ix) {
            return matches!(row, Row::Group { .. });
        }
        let mut at: u64 = 0;
        for (_, count) in &self.groups {
            if at == u64::from(ix) {
                return true;
            }
            at = at.saturating_add(1).saturating_add(*count);
            if at > u64::from(ix) {
                return false;
            }
        }
        false
    }

    /// Ranges the UI asked for and the backend may send soon: their rows are taken
    /// from now on, and nothing is dropped. Rows are dropped only when the backend
    /// says what it really subscribed to ([`MemberList::set_ranges`]): Fluxer syncs
    /// only the ranges it did not have, so a page dropped here for a wish that was
    /// never sent would not come back.
    pub fn extend_ranges(&mut self, ranges: &[[u32; 2]]) {
        for r in ranges {
            if !self.ranges.contains(r) {
                self.ranges.push(*r);
            }
        }
        // Bounded: the newest ranges stay (at most 10, as a subscription may have).
        if self.ranges.len() > MAX_KEPT_RANGES {
            let cut = self.ranges.len() - MAX_KEPT_RANGES;
            self.ranges.drain(..cut);
            let keep = self.ranges.clone();
            self.rows
                .retain(|ix, _| keep.iter().any(|[s, e]| s <= ix && ix <= e));
        }
    }

    /// The ranges the server has now (what the backend last subscribed to): rows
    /// outside them are dropped.
    pub fn set_ranges(&mut self, ranges: &[[u32; 2]]) {
        self.ranges = ranges.to_vec();
        let keep: Vec<[u32; 2]> = self.ranges.clone();
        self.rows
            .retain(|ix, _| keep.iter().any(|[s, e]| s <= ix && ix <= e));
    }

    fn subscribed(&self, ix: u64) -> bool {
        self.ranges
            .iter()
            .any(|[s, e]| u64::from(*s) <= ix && ix <= u64::from(*e))
    }

    /// Applies an update for this list. Returns false for another guild or channel.
    ///
    /// `SYNC` replaces a range, `INVALIDATE` forgets one, `INSERT` and `DELETE` add or
    /// remove one row and move the rows after it, `UPDATE` replaces one row. Ranges
    /// of more than 100 rows, past the end of the list, or outside the subscription
    /// change nothing.
    pub fn apply(&mut self, u: &p::GuildMemberListUpdate) -> bool {
        if GuildId::parse(&u.guild_id) != Some(self.guild) {
            return false;
        }
        let list = u
            .channel_id
            .as_deref()
            .and_then(ChannelId::parse)
            .or_else(|| ChannelId::parse(&u.id));
        if list != Some(self.channel) {
            return false;
        }
        self.loaded = true;
        self.member_count = u.member_count;
        self.online_count = u.online_count;
        self.groups = u
            .groups
            .iter()
            .take(MAX_GROUPS)
            .map(|g| (g.id.chars().take(32).collect(), g.count))
            .collect();
        for op in &u.ops {
            // The op names are upper case on the wire.
            let kind = op.op.to_ascii_lowercase();
            match kind.as_str() {
                "sync" | "invalidate" => {
                    let (start, end) = match op.range.as_slice() {
                        [s, e] if s <= e && e - s < u64::from(PAGE) && *e <= u64::from(MAX_END) => {
                            (*s, *e)
                        }
                        _ => continue,
                    };
                    let sync = kind == "sync";
                    for i in start..=end {
                        if !self.subscribed(i) {
                            continue;
                        }
                        let item = sync
                            .then(|| op.items.get((i - start) as usize))
                            .flatten()
                            .and_then(Row::of);
                        match item {
                            Some(row) => self.rows.insert(i as u32, row),
                            None => self.rows.remove(&(i as u32)),
                        };
                    }
                }
                "insert" | "update" | "delete" => {
                    let Some(ix) = op.index.filter(|i| *i <= u64::from(MAX_END)) else {
                        continue;
                    };
                    let ix = ix as u32;
                    match kind.as_str() {
                        "insert" => {
                            self.shift(ix, true);
                            self.put(ix, op.item.as_ref());
                        }
                        "update" => self.put(ix, op.item.as_ref()),
                        _ => self.shift(ix, false),
                    }
                }
                other => log::debug!("unknown member list op {other:?}"),
            }
        }
        // Rows past the end of the list are gone.
        let len = self.len();
        self.rows.retain(|ix, _| *ix < len);
        true
    }

    fn put(&mut self, ix: u32, item: Option<&p::MemberListItem>) {
        if !self.subscribed(u64::from(ix)) {
            return;
        }
        match item.and_then(Row::of) {
            Some(row) => self.rows.insert(ix, row),
            None => self.rows.remove(&ix),
        };
    }

    /// Moves the rows from `ix` on one place down (`insert`) or removes row `ix` and
    /// moves the rows after it one place up. Rows that leave the subscription go.
    fn shift(&mut self, ix: u32, insert: bool) {
        let tail = self.rows.split_off(&ix);
        for (i, row) in tail {
            let to = if insert {
                i.checked_add(1)
            } else if i == ix {
                None
            } else {
                Some(i - 1)
            };
            if let Some(to) = to.filter(|t| self.subscribed(u64::from(*t))) {
                self.rows.insert(to, row);
            }
        }
    }
}

/// The ranges to subscribe to with rows `first..=last` on screen in a list of `total`
/// rows (`None` while its size is unknown): the first page (it holds the top groups,
/// as in Fluxer) and the pages the screen and [`BUFFER_ROWS`] around it touch. Never
/// more than [`MAX_RANGES`] ranges, never past the end of the list.
pub fn window(first: u32, last: u32, total: Option<u32>) -> Vec<[u32; 2]> {
    let last = last.max(first);
    let (lo, mut hi) = (
        first.saturating_sub(BUFFER_ROWS),
        last.saturating_add(BUFFER_ROWS).min(MAX_END),
    );
    if let Some(total) = total {
        if total == 0 {
            return vec![[0, PAGE - 1]];
        }
        hi = hi.min(total - 1);
    }
    let lo = lo.min(hi);
    let mut pages = vec![0u32];
    for page in lo / PAGE..=hi / PAGE {
        if !pages.contains(&page) {
            pages.push(page);
        }
    }
    // The pages closest to the screen win when the screen spans more.
    if pages.len() > MAX_RANGES {
        let mid = (first / PAGE + last / PAGE) / 2;
        let mut rest: Vec<u32> = pages[1..].to_vec();
        rest.sort_by_key(|p| p.abs_diff(mid));
        rest.truncate(MAX_RANGES - 1);
        rest.sort_unstable();
        pages.truncate(1);
        pages.extend(rest);
    }
    pages
        .into_iter()
        .map(|p| [p * PAGE, (p * PAGE + PAGE - 1).min(MAX_END)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn member(id: u64, name: &str, status: &str) -> Value {
        json!({"member": {"user": {"id": id.to_string(), "username": name},
            "roles": [], "presence": {"status": status}}})
    }

    fn update(ops: Value, groups: Value) -> p::GuildMemberListUpdate {
        serde_json::from_value(json!({
            "guild_id": "100", "id": "200", "channel_id": "200",
            "member_count": 5, "online_count": 3, "groups": groups, "ops": ops
        }))
        .unwrap()
    }

    fn list() -> MemberList {
        MemberList::new(
            GuildId::parse("100").unwrap(),
            ChannelId::parse("200").unwrap(),
        )
    }

    /// A SYNC of whole pages: row `i` is member `i + 1` (row 0 the group header).
    fn sync_pages(pages: &[u32]) -> p::GuildMemberListUpdate {
        let ops: Vec<Value> = pages
            .iter()
            .map(|p| {
                let (s, e) = (p * PAGE, p * PAGE + PAGE - 1);
                let items: Vec<Value> = (s..=e)
                    .map(|i| {
                        if i == 0 {
                            json!({"group": {"id": "online", "count": 399}})
                        } else {
                            member(u64::from(i) + 1, &format!("m{i}"), "online")
                        }
                    })
                    .collect();
                json!({"op": "SYNC", "range": [s, e], "items": items})
            })
            .collect();
        update(json!(ops), json!([{"id": "online", "count": 399}]))
    }

    fn loaded(l: &MemberList, page: u32) -> bool {
        (page * PAGE..page * PAGE + PAGE).all(|i| l.row(i).is_some())
    }

    /// Fluxer syncs only the ranges it did not have. A page the UI stopped wanting
    /// for a moment (a wish the backend never sent, 300 ms coalescing) keeps its rows,
    /// so when the person scrolls back over a page edge nothing stays a placeholder.
    #[test]
    fn rows_stay_until_the_backend_really_unsubscribes_them() {
        let mut l = list();
        // Sent {0, 2}; the server synced both pages.
        l.set_ranges(&[[0, 99], [200, 299]]);
        assert!(l.apply(&sync_pages(&[0, 2])));
        assert!(loaded(&l, 0) && loaded(&l, 2));
        // Within 300 ms: {0, 2, 3}, then {0, 3} (page 2 off the screen), then {0, 2, 3}
        // again (scrolled back up over the edge). Only the last one is sent.
        let b = [[0, 99], [200, 299], [300, 399]];
        l.extend_ranges(&b);
        l.extend_ranges(&[[0, 99], [300, 399]]);
        l.extend_ranges(&b);
        assert!(loaded(&l, 2), "page 2 is not dropped for a wish never sent");
        l.set_ranges(&b);
        // The server had {0, 2}: it syncs only page 3.
        assert!(l.apply(&sync_pages(&[3])));
        for page in 0..4 {
            if page != 1 {
                assert!(loaded(&l, page), "page {page}");
            }
        }
        // When the backend really sends {0, 3}, page 2 goes (the server dropped it).
        l.set_ranges(&[[0, 99], [300, 399]]);
        assert!(!loaded(&l, 2) && loaded(&l, 3));
        // Many wishes before a send keep at most 10 ranges.
        for p in 4..20 {
            l.extend_ranges(&[[p * PAGE, p * PAGE + PAGE - 1]]);
        }
        assert!(l.ranges.len() <= MAX_KEPT_RANGES);
    }

    fn names(l: &MemberList) -> Vec<(u32, String)> {
        (0..l.len())
            .filter_map(|i| match l.row(i)? {
                Row::Group { id, count } => Some((i, format!("{id}—{count}"))),
                Row::Member(m) => Some((i, m.name.clone())),
            })
            .collect()
    }

    #[test]
    fn member_list_requests_only_visible_ranges() {
        // Before the size is known: the first page only.
        assert_eq!(window(0, 15, None), vec![[0, 99]]);
        assert_eq!(window(0, 15, Some(0)), vec![[0, 99]]);
        // A small list: still one page, never past its end.
        assert_eq!(window(0, 15, Some(30)), vec![[0, 99]]);
        // Scrolled to row 950 of 5,000: the first page and the one on screen.
        assert_eq!(window(950, 965, Some(5_000)), vec![[0, 99], [900, 999]]);
        // Near a page edge the next page comes along (the buffer), nothing more.
        assert_eq!(
            window(985, 999, Some(5_000)),
            vec![[0, 99], [900, 999], [1_000, 1_099]]
        );
        // At the end of a 100,001-row list: never past row 100,000.
        let end = window(99_990, 100_000, Some(100_001));
        assert_eq!(end, vec![[0, 99], [99_900, 99_999], [100_000, 100_000]]);
        // A huge screen still subscribes to at most three ranges of 100 rows, around
        // what is shown.
        let tall = window(400, 900, Some(5_000));
        assert_eq!(tall.len(), MAX_RANGES);
        assert_eq!(tall[0], [0, 99]);
        // Whatever the scroll position, nothing but the first page and the pages
        // around the screen is asked for: at most 300 rows of a 100,000-row list.
        for first in (0..100_000).step_by(997) {
            let w = window(first, first + 20, Some(100_001));
            let rows: u32 = w.iter().map(|[s, e]| e - s + 1).sum();
            assert!(rows <= 300, "{first}: {w:?}");
            assert!(w.iter().all(|[s, e]| s <= e && e - s < PAGE));
            // Every range but the first page touches the rows on screen or the buffer.
            let (lo, hi) = (first.saturating_sub(BUFFER_ROWS), first + 20 + BUFFER_ROWS);
            assert!(
                w.iter().skip(1).all(|[s, e]| *e >= lo && *s <= hi),
                "{first}: {w:?}"
            );
        }
    }

    #[test]
    fn list_update_ops_sync_insert_update_delete_invalidate() {
        let groups = json!([{"id": "online", "count": 3}, {"id": "offline", "count": 2}]);
        let mut l = list();
        assert!(!l.loaded);
        let sync = update(
            json!([{"op": "SYNC", "range": [0, 5], "items": [
                {"group": {"id": "online", "count": 3}},
                member(1, "ana", "online"),
                member(2, "bo", "dnd"),
                member(3, "cy", "idle"),
                {"group": {"id": "offline", "count": 2}},
                member(4, "di", "offline")
            ]}]),
            groups.clone(),
        );
        assert!(l.apply(&sync));
        assert!(l.loaded);
        assert_eq!(l.len(), 7);
        assert_eq!(
            names(&l),
            vec![
                (0, "online—3".into()),
                (1, "ana".into()),
                (2, "bo".into()),
                (3, "cy".into()),
                (4, "offline—2".into()),
                (5, "di".into()),
            ]
        );
        assert!(l.is_header(0) && l.is_header(4) && !l.is_header(6));
        let Some(Row::Member(bo)) = l.row(2) else {
            panic!()
        };
        assert_eq!(bo.status, "dnd");

        // INSERT at 2 moves bo and the rows after him down one.
        let insert = update(
            json!([{"op": "INSERT", "index": 2, "item": member(9, "eve", "online")}]),
            json!([{"id": "online", "count": 4}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&insert));
        assert_eq!(
            names(&l)[1..5],
            [
                (1, "ana".into()),
                (2, "eve".into()),
                (3, "bo".into()),
                (4, "cy".into())
            ]
        );
        assert_eq!(names(&l)[5], (5, "offline—2".into()));
        // UPDATE replaces one row in place.
        let upd = update(
            json!([{"op": "UPDATE", "index": 3, "item": member(2, "bob", "online")}]),
            json!([{"id": "online", "count": 4}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&upd));
        assert_eq!(names(&l)[3], (3, "bob".into()));
        // DELETE removes one row and moves the rest up.
        let delete = update(
            json!([{"op": "DELETE", "index": 1}]),
            json!([{"id": "online", "count": 3}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&delete));
        assert_eq!(
            names(&l),
            vec![
                (0, "online—3".into()),
                (1, "eve".into()),
                (2, "bob".into()),
                (3, "cy".into()),
                (4, "offline—2".into()),
                (5, "di".into()),
            ]
        );
        // INVALIDATE forgets a range.
        let invalidate = update(
            json!([{"op": "INVALIDATE", "range": [1, 2]}]),
            json!([{"id": "online", "count": 3}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&invalidate));
        assert!(l.row(1).is_none() && l.row(2).is_none());
        assert!(l.row(3).is_some());

        // Hostile or foreign updates change nothing they should not.
        let before = l.clone();
        let mut other = invalidate.clone();
        other.guild_id = "101".into();
        assert!(!l.apply(&other));
        let hostile = update(
            json!([
                {"op": "SYNC", "range": [0, 100_000], "items": []},
                {"op": "SYNC", "range": [5, 2], "items": []},
                {"op": "INSERT", "index": 4_000_000_000u64, "item": member(7, "x", "online")},
                {"op": "UPDATE", "item": member(7, "x", "online")},
                {"op": "WHATEVER", "range": [0, 1]}
            ]),
            json!([{"id": "online", "count": 3}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&hostile));
        assert_eq!(names(&l), names(&before));

        // Rows outside the subscription are not kept, and leave it when it changes.
        l.set_ranges(&[[0, 3]]);
        assert!(l.row(4).is_none() && l.row(5).is_none());
        let far = update(
            json!([{"op": "UPDATE", "index": 5, "item": member(4, "di", "offline")}]),
            json!([{"id": "online", "count": 3}, {"id": "offline", "count": 2}]),
        );
        assert!(l.apply(&far));
        assert!(l.row(5).is_none());
        // A list that shrinks drops the rows past its end.
        let shrink = update(json!([]), json!([{"id": "online", "count": 1}]));
        assert!(l.apply(&shrink));
        assert_eq!(l.len(), 2);
        assert!(l.row(3).is_none());
        // A huge count does not overflow.
        let huge = update(json!([]), json!([{"id": "offline", "count": u64::MAX}]));
        assert!(l.apply(&huge));
        assert_eq!(l.len(), MAX_END + 1);
    }

    #[test]
    fn rows_clean_names_and_unknown_statuses() {
        let m: p::GuildMember = serde_json::from_value(json!({
            "user": {"id": "5", "username": "a\u{202e}b", "global_name": "Name\u{200b}", "bot": true},
            "nick": "", "roles": ["7", "x"],
            "presence": {"status": "weird", "custom_status": {"text": "  "}}
        }))
        .unwrap();
        let row = MemberRow::of(&m).unwrap();
        assert_eq!(row.name, "Name");
        assert_eq!(row.username, "ab");
        assert_eq!(row.status, "offline");
        assert!(row.bot && row.custom_status.is_none());
        assert_eq!(row.roles, vec![RoleId::parse("7").unwrap()]);
    }
}
