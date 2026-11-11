// SPDX-License-Identifier: AGPL-3.0-or-later
//! The main layout with the demo data, for the layout tests (`tests/layout_metrics.rs`).
//! Demo builds only; not an API.
//!
//! [`Probe`] opens the real view without a backend thread and feeds it the events the
//! demo backend would send: the account, sign-in, READY, the guilds, the channels of
//! `g1` and the newest page of `#general`. The views mark what the tests measure with
//! gpui's `debug_selector`; the names are in [`selector`].

use std::sync::{Arc, Mutex};

use gpui::{div, prelude::*, Context, Entity, Window};
use inlet_protocol as p;
use inlet_state::ChannelId;

use crate::app::Inlet;
use crate::backend::{accounts, messages as msgs};
use crate::demo::{self, scenes, snowflake};

/// The `debug_selector` names the layout tests read.
pub mod selector {
    /// The guild rail.
    pub const RAIL: &str = "guild-rail";
    /// The direct messages entry at the top of the rail.
    pub const RAIL_HOME: &str = "rail-home";
    /// A guild entry in the rail.
    pub const RAIL_GUILD: &str = "rail-guild";
    /// The channel list column.
    pub const SIDEBAR: &str = "sidebar";
    /// The guild name above the channel list.
    pub const GUILD_HEADER: &str = "guild-header";
    /// The channel header above the messages.
    pub const CHAT_HEADER: &str = "chat-header";
    /// A channel row.
    pub const CHANNEL_ROW: &str = "channel-row";
    /// A message row (cozy), its avatar column and its text column.
    pub const MESSAGE_ROW: &str = "message-row";
    pub const MESSAGE_AVATAR_COLUMN: &str = "message-avatar-column";
    pub const MESSAGE_CONTENT: &str = "message-content";
    /// The composer's text box.
    pub const COMPOSER_BOX: &str = "composer-box";
    /// The member list beside the chat (not built yet).
    pub const MEMBER_LIST: &str = "member-list";
}

/// Corner radius of a channel row. gpui's bounds carry no radius, so the tests read
/// the value the row is drawn with.
pub const CHANNEL_ROW_RADIUS: f32 = crate::ui::sidebar::channel_list::ROW_RADIUS;

/// A window's root view holding Inlet's view, signed in to the demo account with guild
/// `g1` and `#general` open.
pub struct Probe {
    inlet: Entity<Inlet>,
}

impl Probe {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let inlet = cx.new(|cx| {
            let mut inlet = Inlet::detached(window, cx);
            open_demo(&mut inlet, window, cx);
            inlet
        });
        Self { inlet }
    }
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inlet.clone())
    }
}

/// What the demo backend sends after sign-in, then the guild and channel the
/// screenshots open, then that channel's newest page.
fn open_demo(inlet: &mut Inlet, window: &mut Window, cx: &mut Context<Inlet>) {
    let data = scenes::data(scenes::Scene::Default);
    let names = Arc::new(Mutex::new(demo::demo_names(&data.channels)));
    // As the demo backend: the account list, which account it is, then its READY.
    let main = demo::accounts::list().into_iter().take(1).collect();
    inlet.handle_event(accounts::Ev::Accounts(main).into(), window, cx);
    inlet.handle_event(
        accounts::Ev::SignedIn {
            key: demo::accounts::main_key(),
            username: demo::ME.into(),
            instance: demo::accounts::INSTANCE.into(),
        }
        .into(),
        window,
        cx,
    );
    inlet.handle_event(
        msgs::Ev::Dispatch {
            dispatch: Box::new(p::Dispatch::Ready(Box::new(data.ready))),
            rendered: Vec::new(),
        }
        .into(),
        window,
        cx,
    );
    inlet.handle_event(msgs::Ev::Guilds(data.guilds).into(), window, cx);

    let guild = snowflake("g1");
    let channel = snowflake("c-general");
    inlet.select_guild(Some(guild.clone()), cx);
    inlet.demo_channel = Some(channel.clone());
    let channels = data
        .channels
        .get(&Some(guild.clone()))
        .cloned()
        .unwrap_or_default();
    inlet.handle_event(
        msgs::Ev::Channels {
            guild_id: Some(guild),
            channels,
        }
        .into(),
        window,
        cx,
    );

    let ch = ChannelId::parse(&channel).expect("a demo snowflake");
    let request = inlet
        .chat
        .view
        .waiting_request(ch)
        .expect("opening #general asks for its newest page");
    let list = data
        .messages
        .get(&channel)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let (page, rendered) = demo::rendered_page(list, None, &names);
    inlet.handle_event(
        msgs::Ev::Page {
            channel_id: channel,
            request,
            page,
            rendered,
        }
        .into(),
        window,
        cx,
    );
}
