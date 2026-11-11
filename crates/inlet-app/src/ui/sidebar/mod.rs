// SPDX-License-Identifier: AGPL-3.0-or-later
//! The column next to the guild rail: guild header, channel or DM list, user panel.

pub mod channel_list;
mod dm_list;
pub mod favorites;
mod guild_header;
mod user_panel;

use std::collections::HashSet;

use gpui::{div, prelude::*, px, AnyElement, Context, Window};

use crate::app::Inlet;
use crate::backend::messages;
use crate::model::{self, Channel, Guild};
use crate::theme;

/// Channel list / DM list width.
pub const SIDEBAR_WIDTH: f32 = 320.;

#[derive(Default)]
pub struct SidebarState {
    /// Categories the person folded away in the channel list.
    pub collapsed: HashSet<String>,
}

impl Inlet {
    pub(crate) fn on_guilds(&mut self, guilds: Vec<Guild>, cx: &mut Context<Self>) {
        self.guilds = guilds;
        if let Some(selected) = &self.selected_guild {
            if !self.guilds.iter().any(|g| &g.id == selected) {
                self.chat.notice = Some(t!("sidebar-guild-gone"));
                self.select_guild(None, cx);
            }
        }
    }

    pub(crate) fn on_channels(
        &mut self,
        guild_id: Option<String>,
        channels: Vec<Channel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if guild_id == self.selected_guild {
            self.channels = model::sidebar_order(&channels);
            let open_still_exists = self
                .selected_channel
                .as_ref()
                .is_some_and(|id| self.channels.iter().any(|c| &c.id == id));
            if self.selected_channel.is_some() && !open_still_exists {
                self.chat.notice = Some(t!("sidebar-channel-deleted"));
                self.selected_channel = None;
            } else if let Some(open) = self.selected_channel.clone() {
                // The open channel may have changed (for example its slowmode).
                self.composer_open_channel(&open, window, cx);
            }
            // Back to an account: the channel it had open.
            if let Some(wanted) = self.switcher.take_wanted_channel() {
                if self.channels.iter().any(|c| c.id == wanted) {
                    self.select_channel(wanted, window, cx);
                }
            }
            #[cfg(feature = "demo")]
            if let Some(wanted) = self.demo_channel.take() {
                if self.channels.iter().any(|c| c.id == wanted) {
                    self.select_channel(wanted, window, cx);
                }
            }
            // A channel a confirmed link asked for (see `desktop`).
            self.take_link_channel(window, cx);
            if self.selected_channel.is_none() {
                match self.channels.iter().find(|c| c.is_messageable()).cloned() {
                    Some(first) => self.select_channel(first.id, window, cx),
                    None => self.clear_messages(),
                }
            }
        }
    }

    pub(crate) fn select_guild(&mut self, guild_id: Option<String>, cx: &mut Context<Self>) {
        self.threads_leave_channel(None, cx);
        self.selected_guild = guild_id.clone();
        self.selected_channel = None;
        self.channels.clear();
        self.clear_messages();
        self.backend.send(messages::Cmd::OpenGuild { guild_id });
        cx.notify();
    }

    pub(crate) fn select_channel(
        &mut self,
        channel_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_channel = Some(channel_id.clone());
        // A forum opens the forum view; any other channel closes it.
        if self.open_forum(&channel_id, window, cx) {
            cx.notify();
            return;
        }
        self.composer_open_channel(&channel_id, window, cx);
        self.open_channel_messages(cx);
        cx.notify();
    }

    pub(crate) fn render_channel_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = match self.selected_guild {
            None => self.dm_rows(cx),
            Some(_) => self.channel_rows(cx),
        };
        div()
            .debug_selector(|| "sidebar".into())
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::bg_app())
            .child(self.render_guild_header(cx))
            .child(
                div()
                    .id("channels")
                    .flex_1()
                    .overflow_y_scroll()
                    .pt_2()
                    .pr_2()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .children(rows),
            )
    }
}
