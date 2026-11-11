// SPDX-License-Identifier: AGPL-3.0-or-later
//! The emoji tab: favourites, recently used, each community's custom emoji (the open
//! one first; other communities' locked unless the account may use them) and the
//! Unicode categories in Fluxer's order, with search, a skin tone and a jump bar.
//!
//! The grid is a list of rows of the same height (headers and rows of emoji), so only
//! the rows on screen are drawn: Twemoji come from the bounded drawing cache, custom
//! emoji through the session's image client and the one animation budget.

use std::ops::Range;

use gpui::{
    canvas, div, prelude::*, px, uniform_list, AnyElement, App, ClickEvent, Context, Corners,
    MouseButton, MouseDownEvent, ScrollStrategy, SharedString,
};
use inlet_emoji::{Category, SkinTone};
use inlet_protocol as p;

use super::prefs::{self, Key, PickerPrefs, MAX_RECENT};
use super::{usable, Insert, PickerIcon, Target};
use crate::app::Inlet;
use crate::backend::composer::gif::PrefEdit;
use crate::backend::composer::snowflake_u64;
use crate::theme;
use crate::ui::media::emoji::{self as media_emoji, paint_emoji};

/// Emoji per row.
pub const COLUMNS: usize = 9;
/// A cell (and every row), in logical pixels.
const CELL: f32 = 44.;
/// An emoji in its cell.
const GLYPH: f32 = 32.;
/// Most search results.
const MAX_RESULTS: usize = 180;
/// Custom emoji listed per community.
const MAX_PER_GUILD: usize = 500;
/// Jumps in the bar under the grid.
const MAX_JUMPS: usize = 12;

/// One emoji in the grid.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Unicode(&'static inlet_emoji::Emoji),
    Custom(CustomCell),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CustomCell {
    pub guild: String,
    pub id: String,
    /// ASCII letters, digits, `_` and `-` only.
    pub name: String,
    pub animated: bool,
    /// From another community, and the account may not use it here.
    pub locked: bool,
}

impl Cell {
    /// Fluxer's usage key.
    pub fn key(&self) -> String {
        match self {
            Cell::Unicode(e) => prefs::unicode_key(e),
            Cell::Custom(c) => prefs::custom_key(&c.guild, &c.id),
        }
    }
}

/// A row of the grid.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Header(SharedString),
    Cells(Vec<Cell>),
}

/// What the emoji grid is built from, apart from the account state's emoji themselves:
/// when none of it changes, a redraw uses the grid it has.
#[derive(Clone, Debug, PartialEq)]
pub struct GridKey {
    pub query: String,
    /// [`PickerPrefs::rev`]: favourites and recently used.
    pub prefs_rev: u64,
    /// The communities in picker order, each with where its emoji list lives and how long
    /// it is (the account state replaces the list when the emoji change).
    pub guilds: Vec<(inlet_state::GuildId, usize, usize)>,
    pub channel_guild: Option<String>,
    pub external: bool,
    pub global: bool,
}

/// One community's custom emoji: `(id, name, animated)`.
pub struct GuildEmojiSet {
    pub guild: String,
    pub name: String,
    pub emojis: Vec<(String, String, bool)>,
}

/// What the grid is built from.
pub struct Inputs<'a> {
    pub prefs: &'a PickerPrefs,
    pub now_ms: i64,
    /// The account's communities, the open one first.
    pub guilds: &'a [GuildEmojiSet],
    /// The open channel's community (`None`: a direct message).
    pub channel_guild: Option<&'a str>,
    /// The right to use external emoji in the open channel.
    pub external: bool,
    /// The instance's `feature_global_expressions` for the account.
    pub global: bool,
}

impl Inputs<'_> {
    fn custom(&self, guild: &str, id: &str, name: &str, animated: bool) -> Cell {
        Cell::Custom(CustomCell {
            guild: guild.to_owned(),
            id: id.to_owned(),
            name: name.to_owned(),
            animated,
            locked: !usable(guild, self.channel_guild, self.external, self.global),
        })
    }

    /// The emoji of a usage key, if it still exists.
    fn by_key(&self, key: &str) -> Option<Cell> {
        match prefs::parse_key(key)? {
            Key::Unicode(e) => Some(Cell::Unicode(e)),
            Key::Custom { guild, id } => {
                let set = self
                    .guilds
                    .iter()
                    .find(|s| guild.is_empty() || s.guild == guild)?;
                let (id, name, animated) = set.emojis.iter().find(|(i, _, _)| i == id)?;
                Some(self.custom(&set.guild, id, name, *animated))
            }
        }
    }
}

fn category_label(c: Category) -> String {
    match c {
        Category::People => t!("picker-cat-people"),
        Category::Nature => t!("picker-cat-nature"),
        Category::Food => t!("picker-cat-food"),
        Category::Activity => t!("picker-cat-activity"),
        Category::Travel => t!("picker-cat-travel"),
        Category::Objects => t!("picker-cat-objects"),
        Category::Symbols => t!("picker-cat-symbols"),
        Category::Flags => t!("picker-cat-flags"),
    }
}

/// A search text as Fluxer reads it: without surrounding colons, lower case, spaces as
/// `_`.
pub fn normalise(query: &str) -> String {
    query
        .trim()
        .trim_matches(':')
        .to_lowercase()
        .replace(' ', "_")
}

fn push_section(rows: &mut Vec<Row>, jumps: &mut Vec<usize>, title: String, cells: Vec<Cell>) {
    if cells.is_empty() {
        return;
    }
    jumps.push(rows.len());
    rows.push(Row::Header(title.into()));
    for chunk in cells.chunks(COLUMNS) {
        rows.push(Row::Cells(chunk.to_vec()));
    }
}

/// The grid's rows for `query`, and the rows the jump bar jumps to (section headers).
pub fn rows(inputs: &Inputs<'_>, query: &str) -> (Vec<Row>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut jumps = Vec::new();
    let q = normalise(query);
    if !q.is_empty() {
        // Custom emoji whose name starts with the text first, then those that contain it,
        // then Unicode emoji ranked the way Fluxer does.
        let mut custom: Vec<(u8, Cell)> = Vec::new();
        for set in inputs.guilds {
            for (id, name, animated) in &set.emojis {
                let lower = name.to_lowercase();
                let rank = if lower.starts_with(&q) {
                    0
                } else if lower.contains(&q) {
                    1
                } else {
                    continue;
                };
                custom.push((rank, inputs.custom(&set.guild, id, name, *animated)));
            }
        }
        custom.sort_by_key(|(r, _)| *r);
        let mut cells: Vec<Cell> = custom
            .into_iter()
            .map(|(_, c)| c)
            .take(MAX_RESULTS)
            .collect();
        let room = MAX_RESULTS.saturating_sub(cells.len());
        cells.extend(inlet_emoji::search(&q, room).into_iter().map(Cell::Unicode));
        for chunk in cells.chunks(COLUMNS) {
            rows.push(Row::Cells(chunk.to_vec()));
        }
        return (rows, jumps);
    }
    let favourites: Vec<Cell> = inputs
        .prefs
        .favourites()
        .iter()
        .filter_map(|k| inputs.by_key(k))
        .collect();
    push_section(
        &mut rows,
        &mut jumps,
        t!("picker-section-favourites"),
        favourites,
    );
    let recent: Vec<Cell> = inputs
        .prefs
        .recent(inputs.now_ms, MAX_RECENT * 2)
        .iter()
        .filter_map(|k| inputs.by_key(k))
        .take(MAX_RECENT)
        .collect();
    push_section(&mut rows, &mut jumps, t!("picker-section-recent"), recent);
    for set in inputs.guilds {
        let cells = set
            .emojis
            .iter()
            .map(|(id, name, animated)| inputs.custom(&set.guild, id, name, *animated))
            .collect();
        push_section(&mut rows, &mut jumps, set.name.clone(), cells);
    }
    for (category, emojis) in inlet_emoji::categories() {
        let cells = emojis.iter().map(Cell::Unicode).collect();
        push_section(&mut rows, &mut jumps, category_label(*category), cells);
    }
    (rows, jumps)
}

/// The next skin tone of the tone button: the default, then light to dark.
pub fn next_tone(tone: SkinTone) -> SkinTone {
    match tone {
        SkinTone::Default => SkinTone::Light,
        SkinTone::Light => SkinTone::MediumLight,
        SkinTone::MediumLight => SkinTone::Medium,
        SkinTone::Medium => SkinTone::MediumDark,
        SkinTone::MediumDark => SkinTone::Dark,
        SkinTone::Dark => SkinTone::Default,
    }
}

/// A Twemoji drawing `size` logical pixels square, from the bounded drawing cache; as
/// text when the cache has no room or no drawing.
pub fn twemoji(glyph: &str, size: f32) -> AnyElement {
    let planned = media_emoji::twemoji_plan(&[glyph]).pop().flatten();
    let Some(image) = planned else {
        return div()
            .size(px(size))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .text_size(px(size * 0.75))
            .child(glyph.to_owned())
            .into_any_element();
    };
    let glyph = glyph.to_owned();
    canvas(
        move |_, window, cx| {
            let render = image.use_render_image(window, cx);
            if let Some(r) = &render {
                media_emoji::twemoji_rendered(&glyph, r);
            }
            render
        },
        move |bounds, render, window, _| {
            if let Some(render) = render {
                let _ = window.paint_image(bounds, bounds, Corners::default(), render, 0, false);
            }
        },
    )
    .size(px(size))
    .flex_none()
    .into_any_element()
}

impl Inlet {
    /// The account's custom emoji per community, the open one first.
    fn picker_guild_emojis(&self, cx: &App) -> Vec<GuildEmojiSet> {
        let Some(account) = self.account.as_ref() else {
            return Vec::new();
        };
        let order = self.picker_guild_order(cx);
        let state = account.read(cx);
        order
            .into_iter()
            .filter_map(|g| {
                let gs = state.guild(g)?;
                let emojis: Vec<(String, String, bool)> = gs
                    .emojis
                    .iter()
                    .filter(|e| {
                        !e.name.is_empty()
                            && e.name.len() <= 64
                            && e.name
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                            && snowflake_u64(&e.id).is_some()
                    })
                    .take(MAX_PER_GUILD)
                    .map(|e| (e.id.clone(), e.name.clone(), e.animated))
                    .collect();
                (!emojis.is_empty()).then(|| GuildEmojiSet {
                    guild: g.to_string(),
                    name: crate::model::clean_name(gs.name()),
                    emojis,
                })
            })
            .collect()
    }

    /// The grid's key this frame (cheap: no emoji are copied).
    fn emoji_grid_key(&self, query: &str, cx: &App) -> GridKey {
        let (channel_guild, external, _) = self.picker_channel(cx);
        let global = self
            .composer
            .pickers
            .setup
            .as_ref()
            .is_some_and(|s| s.global_expressions);
        GridKey {
            query: query.to_owned(),
            prefs_rev: self.composer.pickers.prefs.rev(),
            guilds: self.picker_guild_lists(cx, false),
            channel_guild,
            external,
            global,
        }
    }

    pub(super) fn render_emoji_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = self.composer.pickers.open.as_ref() else {
            return div().into_any_element();
        };
        let (query, handle) = (open.query.clone(), open.list.clone());
        let key = self.emoji_grid_key(&query, cx);
        // Built again only when the key changes, not on every redraw.
        let mut cache = std::mem::take(&mut self.composer.pickers.emoji_rows);
        let grid = cache.get(key.clone(), || {
            let guilds = self.picker_guild_emojis(cx);
            let inputs = Inputs {
                prefs: &self.composer.pickers.prefs,
                now_ms: chrono::Utc::now().timestamp_millis(),
                guilds: &guilds,
                channel_guild: key.channel_guild.as_deref(),
                external: key.external,
                global: key.global,
            };
            rows(&inputs, &key.query)
        });
        self.composer.pickers.emoji_rows = cache;
        if grid.0.is_empty() {
            return empty(t!("picker-emoji-none"));
        }
        let tone = self.composer.pickers.prefs.skin_tone();
        self.picker_images(true);
        let jump_bar = self.render_jump_bar(&grid.0, &grid.1, tone, &handle, cx);
        self.picker_images(false);
        let count = grid.0.len();
        let list = uniform_list(
            "picker-emoji-grid",
            count,
            cx.processor(move |this: &mut Inlet, range: Range<usize>, _, cx| {
                this.picker_images(true);
                let out = range
                    .filter_map(|ix| grid.0.get(ix).map(|r| (ix, r)))
                    .map(|(ix, row)| this.render_emoji_row(ix, row, tone, cx))
                    .collect::<Vec<_>>();
                this.picker_images(false);
                out
            }),
        )
        .track_scroll(&handle)
        .flex_1()
        .w_full();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px(px(12.))
            .child(list)
            .children(jump_bar)
            .into_any_element()
    }

    fn render_emoji_row(
        &mut self,
        ix: usize,
        row: &Row,
        tone: SkinTone,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            Row::Header(title) => div()
                .h(px(CELL))
                .flex()
                .items_end()
                .pb(px(6.))
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(title.clone())
                .into_any_element(),
            Row::Cells(cells) => {
                let mut row_el = div().h(px(CELL)).flex().flex_row();
                for (col, cell) in cells.iter().enumerate() {
                    row_el = row_el.child(self.render_emoji_cell(ix, col, cell, tone, cx));
                }
                row_el.into_any_element()
            }
        }
    }

    fn render_emoji_cell(
        &mut self,
        row: usize,
        col: usize,
        cell: &Cell,
        tone: SkinTone,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (picture, locked) = match cell {
            Cell::Unicode(e) => {
                let glyph = if e.skin_tones {
                    inlet_emoji::with_skin_tone(e, tone)
                } else {
                    std::borrow::Cow::Borrowed(e.glyph)
                };
                (twemoji(&glyph, GLYPH), false)
            }
            Cell::Custom(c) => (
                self.picker_custom_emoji(&c.id, c.animated, GLYPH, cx),
                c.locked,
            ),
        };
        let pick = cell.clone();
        let fav = cell.clone();
        div()
            .id(SharedString::from(format!("picker-emoji-{row}-{col}")))
            .size(px(CELL))
            .flex_none()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::floating_hover()))
            .child(div().when(locked, |d| d.opacity(0.35)).child(picture))
            .when(locked, |d| {
                d.child(
                    div()
                        .absolute()
                        .right(px(3.))
                        .bottom(px(3.))
                        .child(PickerIcon::Lock.el(12., theme::text_muted())),
                )
            })
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.pick_emoji(pick.clone(), e.modifiers().shift, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    let key = fav.key();
                    let on = !this.composer.pickers.prefs.is_favourite(&key);
                    this.edit_prefs(PrefEdit::FavouriteEmoji { key, on });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// A custom emoji in a picker: it moves while the picker is the layer on top.
    pub(super) fn picker_custom_emoji(
        &self,
        id: &str,
        animated: bool,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Marked as the picker's by the budget when the picker is on top and drawing.
        let picture = self.custom_emoji_picture(id, animated, cx);
        let budget = self.media.images.animations().clone();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| paint_emoji(&picture, bounds, &budget, window, cx),
        )
        .size(px(size))
        .flex_none()
        .into_any_element()
    }

    /// A click on an emoji: into the composer or onto the message, and counted as used.
    fn pick_emoji(
        &mut self,
        cell: Cell,
        keep_open: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.composer.pickers.open.as_ref().map(|o| o.target) else {
            return;
        };
        if let Cell::Custom(c) = &cell {
            if c.locked {
                self.chat.notice = Some(t!("picker-emoji-locked"));
                cx.notify();
                return;
            }
        }
        let tone = self.composer.pickers.prefs.skin_tone();
        let glyph_of = |e: &inlet_emoji::Emoji| {
            if e.skin_tones {
                inlet_emoji::with_skin_tone(e, tone).into_owned()
            } else {
                e.glyph.to_owned()
            }
        };
        let key = cell.key();
        let done = match (target, cell) {
            (Target::Composer | Target::Panel, Cell::Unicode(e)) => {
                self.insert_into_composer(target, Insert::Text(glyph_of(e)), window, cx)
            }
            (Target::Composer | Target::Panel, Cell::Custom(c)) => match snowflake_u64(&c.id) {
                Some(id) => self.insert_into_composer(
                    target,
                    Insert::CustomEmoji {
                        id,
                        name: c.name,
                        animated: c.animated,
                    },
                    window,
                    cx,
                ),
                None => false,
            },
            (Target::Reaction { ch, id, .. }, Cell::Unicode(e)) => {
                self.toggle_reaction(ch, id, p::Emoji::unicode(glyph_of(e)), cx);
                true
            }
            (Target::Reaction { ch, id, .. }, Cell::Custom(c)) => {
                self.toggle_reaction(ch, id, p::Emoji::custom(c.id, c.name, c.animated), cx);
                true
            }
        };
        // Only what went in counts as used; a refused pick leaves the picker open.
        if !done {
            cx.notify();
            return;
        }
        self.edit_prefs(PrefEdit::UsedEmoji {
            key,
            at_ms: chrono::Utc::now().timestamp_millis(),
        });
        self.picked(keep_open, cx);
    }

    /// The bar under the grid: one button per section, showing its first emoji.
    fn render_jump_bar(
        &mut self,
        rows: &[Row],
        jumps: &[usize],
        tone: SkinTone,
        handle: &gpui::UniformListScrollHandle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if jumps.len() < 2 {
            return None;
        }
        let mut bar = div()
            .flex()
            .flex_row()
            .justify_between()
            .py(px(6.))
            .border_t_1()
            .border_color(theme::divider());
        for (n, &ix) in jumps.iter().take(MAX_JUMPS).enumerate() {
            let icon = match rows.get(ix + 1) {
                Some(Row::Cells(cells)) => match cells.first() {
                    Some(Cell::Unicode(e)) => twemoji(&inlet_emoji::with_skin_tone(e, tone), 20.),
                    Some(Cell::Custom(c)) => self.picker_custom_emoji(&c.id, false, 20., cx),
                    None => continue,
                },
                _ => continue,
            };
            let handle = handle.clone();
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("picker-jump-{n}")))
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::floating_hover()))
                    .child(icon)
                    .on_click(move |_, _, _| handle.scroll_to_item(ix, ScrollStrategy::Top)),
            );
        }
        Some(bar.into_any_element())
    }

    /// The skin tone button next to the search: a raised hand in the tone; a click takes
    /// the next tone.
    pub(super) fn render_skin_tone_button(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tone = self.composer.pickers.prefs.skin_tone();
        let glyph = inlet_emoji::by_glyph("\u{270B}")
            .map(|e| inlet_emoji::with_skin_tone(e, tone).into_owned())
            .unwrap_or_default();
        div()
            .id("picker-skin-tone")
            .size(px(32.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::floating_hover()))
            .child(twemoji(&glyph, 22.))
            .on_click(cx.listener(move |this, _, _, cx| {
                let next = next_tone(tone);
                let value = next.modifier().map(String::from).unwrap_or_default();
                this.edit_prefs(PrefEdit::SkinTone(value));
                cx.notify();
            }))
            .into_any_element()
    }
}

/// A sentence in the middle of an empty tab.
pub fn empty(text: String) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(24.))
        .text_size(px(14.))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(guild: &str, emojis: &[(&str, &str)]) -> GuildEmojiSet {
        GuildEmojiSet {
            guild: guild.into(),
            name: format!("Guild {guild}"),
            emojis: emojis
                .iter()
                .map(|(id, n)| (id.to_string(), n.to_string(), false))
                .collect(),
        }
    }

    fn cells(rows: &[Row]) -> Vec<Cell> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Cells(c) => Some(c.clone()),
                Row::Header(_) => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn sections_come_in_fluxers_order_and_other_communities_are_locked() {
        let mut prefs = PickerPrefs::default();
        prefs.set_stored(p::SyncedPreferences::empty());
        prefs.edit(PrefEdit::FavouriteEmoji {
            key: "custom:2:21".into(),
            on: true,
        });
        prefs.edit(PrefEdit::UsedEmoji {
            key: "unicode:smile".into(),
            at_ms: 1_000,
        });
        let guilds = [set("1", &[("11", "here")]), set("2", &[("21", "there")])];
        let inputs = Inputs {
            prefs: &prefs,
            now_ms: 2_000,
            guilds: &guilds,
            channel_guild: Some("1"),
            external: true,
            global: false,
        };
        let (rows, jumps) = rows(&inputs, "");
        let headers: Vec<String> = jumps
            .iter()
            .filter_map(|&ix| match &rows[ix] {
                Row::Header(t) => Some(t.to_string()),
                Row::Cells(_) => None,
            })
            .collect();
        assert_eq!(headers[0], t!("picker-section-favourites"));
        assert_eq!(headers[1], t!("picker-section-recent"));
        assert_eq!(headers[2], "Guild 1");
        assert_eq!(headers[3], "Guild 2");
        assert_eq!(headers[4], t!("picker-cat-people"));
        assert_eq!(headers.len(), 4 + Category::ALL.len());
        // Every row but headers holds at most nine emoji.
        assert!(rows.iter().all(
            |r| matches!(r, Row::Header(_)) || matches!(r, Row::Cells(c) if c.len() <= COLUMNS)
        ));
        let all = cells(&rows);
        let locked = |id: &str| {
            all.iter()
                .any(|c| matches!(c, Cell::Custom(c) if c.id == id && c.locked))
        };
        assert!(!locked("11"), "the open community's own");
        assert!(locked("21"), "another's, without the global feature");
        // With it (and the right in the channel), usable.
        let inputs = Inputs {
            global: true,
            ..inputs
        };
        assert!(cells(&rows_of(&inputs, ""))
            .iter()
            .all(|c| !matches!(c, Cell::Custom(c) if c.locked)));
    }

    fn rows_of(inputs: &Inputs<'_>, q: &str) -> Vec<Row> {
        rows(inputs, q).0
    }

    #[test]
    fn search_puts_custom_emoji_first_and_strips_colons() {
        let prefs = PickerPrefs::default();
        let guilds = [set("1", &[("11", "smiley_cat_custom"), ("12", "other")])];
        let inputs = Inputs {
            prefs: &prefs,
            now_ms: 0,
            guilds: &guilds,
            channel_guild: None,
            external: false,
            global: false,
        };
        let found = cells(&rows_of(&inputs, ":SMILE"));
        assert!(matches!(&found[0], Cell::Custom(c) if c.id == "11" && c.locked));
        assert!(found
            .iter()
            .any(|c| matches!(c, Cell::Unicode(e) if e.name() == "smile")));
        assert!(found.len() <= MAX_RESULTS);
        assert!(rows_of(&inputs, "zzzzqqqq").is_empty());
    }

    #[test]
    fn the_tone_button_goes_round() {
        let mut t = SkinTone::Default;
        for _ in 0..6 {
            t = next_tone(t);
        }
        assert_eq!(t, SkinTone::Default);
    }
}
