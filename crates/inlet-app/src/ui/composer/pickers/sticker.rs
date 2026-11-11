// SPDX-License-Identifier: AGPL-3.0-or-later
//! The sticker tab: each community's stickers, the open one first, with search by name
//! and tags. Stickers of another community are locked unless the account may use them
//! (the right to use external stickers and the instance's global expressions, Fluxer's
//! rule). A click sends the sticker at once as a message of its own, as Fluxer does.

use std::ops::Range;

use gpui::{div, prelude::*, px, uniform_list, AnyElement, Context, ObjectFit, SharedString};

use super::emoji::empty;
use super::{usable, PickerIcon, PICKER_WIDTH};
use crate::app::Inlet;
use crate::backend::composer::gif::Cmd;
use crate::backend::composer::snowflake_u64;
use crate::model::Sticker;
use crate::theme;
use crate::ui::media::animation_budget::Motion;
use crate::ui::media::image::{picture_of, Shown};
use crate::ui::media::sticker::sticker_requests;

/// Stickers per row.
const COLUMNS: usize = 4;
/// A cell, in logical pixels.
const CELL: f32 = (PICKER_WIDTH - 24.) / COLUMNS as f32;
/// The sticker in its cell.
const PICTURE: f32 = 80.;
/// Stickers listed per community.
const MAX_PER_GUILD: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct StickerCell {
    pub sticker: Sticker,
    pub locked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Header(SharedString),
    Cells(Vec<StickerCell>),
}

/// What the sticker grid is built from: a redraw with the same key uses the grid it has.
#[derive(Clone, Debug, PartialEq)]
pub struct GridKey {
    pub query: String,
    /// The communities in picker order, with where their sticker lists live and their
    /// lengths (the account state replaces a list when the stickers change).
    pub guilds: Vec<(inlet_state::GuildId, usize, usize)>,
    pub channel_guild: Option<String>,
    pub external: bool,
    pub global: bool,
}

/// One community's stickers: `(sticker, tags)`.
pub struct GuildStickers {
    pub guild: String,
    pub name: String,
    pub stickers: Vec<(Sticker, Vec<String>)>,
}

/// The tab's rows: a header and rows of stickers per community; with search text, the
/// matching stickers alone.
pub fn rows(
    sets: &[GuildStickers],
    query: &str,
    channel_guild: Option<&str>,
    external: bool,
    global: bool,
) -> Vec<Row> {
    let q = query.trim().to_lowercase();
    let mut rows = Vec::new();
    let mut found = Vec::new();
    for set in sets {
        let cells: Vec<StickerCell> = set
            .stickers
            .iter()
            .filter(|(s, tags)| {
                q.is_empty()
                    || s.name.to_lowercase().contains(&q)
                    || tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .map(|(s, _)| StickerCell {
                sticker: s.clone(),
                locked: !usable(&set.guild, channel_guild, external, global),
            })
            .collect();
        if cells.is_empty() {
            continue;
        }
        if q.is_empty() {
            rows.push(Row::Header(set.name.clone().into()));
            rows.extend(cells.chunks(COLUMNS).map(|c| Row::Cells(c.to_vec())));
        } else {
            found.extend(cells);
        }
    }
    rows.extend(found.chunks(COLUMNS).map(|c| Row::Cells(c.to_vec())));
    rows
}

impl Inlet {
    fn picker_guild_stickers(&self, cx: &gpui::App) -> Vec<GuildStickers> {
        let Some(account) = self.account.as_ref() else {
            return Vec::new();
        };
        let order = self.picker_guild_order(cx);
        let state = account.read(cx);
        order
            .into_iter()
            .filter_map(|g| {
                let gs = state.guild(g)?;
                let stickers: Vec<(Sticker, Vec<String>)> = gs
                    .stickers
                    .iter()
                    .filter(|s| snowflake_u64(&s.id).is_some())
                    .take(MAX_PER_GUILD)
                    .map(|s| {
                        (
                            Sticker {
                                id: s.id.clone(),
                                name: crate::model::clean_name(&s.name),
                                animated: s.animated,
                            },
                            s.tags.iter().take(10).cloned().collect(),
                        )
                    })
                    .collect();
                (!stickers.is_empty()).then(|| GuildStickers {
                    guild: g.to_string(),
                    name: crate::model::clean_name(gs.name()),
                    stickers,
                })
            })
            .collect()
    }

    pub(super) fn render_sticker_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = self.composer.pickers.open.as_ref() else {
            return div().into_any_element();
        };
        let (query, handle) = (open.query.clone(), open.list.clone());
        let (channel_guild, _, external) = self.picker_channel(cx);
        let global = self
            .composer
            .pickers
            .setup
            .as_ref()
            .is_some_and(|s| s.global_expressions);
        let key = GridKey {
            query,
            guilds: self.picker_guild_lists(cx, true),
            channel_guild,
            external,
            global,
        };
        // Built again only when the key changes, not on every redraw.
        let mut cache = std::mem::take(&mut self.composer.pickers.sticker_rows);
        let grid = cache.get(key.clone(), || {
            let sets = self.picker_guild_stickers(cx);
            let rows = rows(
                &sets,
                &key.query,
                key.channel_guild.as_deref(),
                key.external,
                key.global,
            );
            (rows, !sets.is_empty())
        });
        self.composer.pickers.sticker_rows = cache;
        if grid.0.is_empty() {
            return empty(if grid.1 {
                t!("picker-stickers-none")
            } else {
                t!("picker-stickers-empty")
            });
        }
        let count = grid.0.len();
        let list = uniform_list(
            "picker-sticker-grid",
            count,
            cx.processor(move |this: &mut Inlet, range: Range<usize>, _, cx| {
                this.picker_images(true);
                let out = range
                    .filter_map(|ix| grid.0.get(ix).map(|r| (ix, r)))
                    .map(|(ix, row)| this.render_sticker_row(ix, row, cx))
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
            .into_any_element()
    }

    fn render_sticker_row(&mut self, ix: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
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
                let mut el = div().h(px(CELL)).flex().flex_row();
                for (col, cell) in cells.iter().enumerate() {
                    el = el.child(self.render_sticker_cell(ix, col, cell, cx));
                }
                el.into_any_element()
            }
        }
    }

    fn render_sticker_cell(
        &mut self,
        row: usize,
        col: usize,
        cell: &StickerCell,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = self.media.images.animations().borrow().prefs;
        let shown = match &self.media.setup {
            Some(setup) => {
                let (moving, still) =
                    sticker_requests(&setup.proxy, &cell.sticker, &prefs, self.media.scale);
                self.moving(moving.as_ref(), still.as_ref(), cx)
            }
            None => Shown::Unavailable,
        };
        let anim = self.media.images.animator(Motion::Sticker);
        let picture = picture_of(shown, (PICTURE, PICTURE), 0., ObjectFit::Contain, &anim);
        let (id, locked) = (cell.sticker.id.clone(), cell.locked);
        div()
            .id(SharedString::from(format!("picker-sticker-{row}-{col}")))
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
                        .right(px(6.))
                        .bottom(px(6.))
                        .child(PickerIcon::Lock.el(14., theme::text_muted())),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if locked {
                    this.chat.notice = Some(t!("picker-sticker-locked"));
                } else {
                    let sticker_id = id.clone();
                    let target = this.composer.pickers.open.as_ref().map(|o| o.target);
                    if let Some(target) = target {
                        let send = |channel_id, nonce| Cmd::SendSticker {
                            channel_id,
                            sticker_id,
                            nonce,
                        };
                        this.send_to_channel(target, send, cx);
                    }
                    this.picked(false, cx);
                }
                cx.notify();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sticker(id: &str, name: &str) -> (Sticker, Vec<String>) {
        (
            Sticker {
                id: id.into(),
                name: name.into(),
                animated: false,
            },
            vec!["party".into()],
        )
    }

    #[test]
    fn stickers_are_grouped_searched_and_locked() {
        let sets = [
            GuildStickers {
                guild: "1".into(),
                name: "One".into(),
                stickers: (0..5).map(|i| sticker(&format!("1{i}"), "wave")).collect(),
            },
            GuildStickers {
                guild: "2".into(),
                name: "Two".into(),
                stickers: vec![sticker("21", "cat")],
            },
        ];
        let all = rows(&sets, "", Some("1"), false, true);
        assert_eq!(
            all.len(),
            2 + 1 + 2,
            "a header and rows of four per community"
        );
        let locked: Vec<bool> = all
            .iter()
            .filter_map(|r| match r {
                Row::Cells(c) => Some(c.iter().map(|c| c.locked).collect::<Vec<_>>()),
                Row::Header(_) => None,
            })
            .flatten()
            .collect();
        assert_eq!(locked, [false, false, false, false, false, true]);
        // Search by name or tag, without headers.
        let found = rows(&sets, "CAT", Some("1"), true, true);
        assert_eq!(found.len(), 1);
        assert!(matches!(&found[0], Row::Cells(c) if c.len() == 1 && !c[0].locked));
        assert_eq!(rows(&sets, "party", None, false, false).len(), 2);
        assert!(rows(&sets, "nothing", None, false, false).is_empty());
    }
}
