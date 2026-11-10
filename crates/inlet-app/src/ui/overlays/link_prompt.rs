// SPDX-License-Identifier: AGPL-3.0-or-later
//! The prompt for a link from outside (`inlet://`) or an invite link: it says where the
//! link goes, and nothing happens until a button is clicked. See
//! `backend::platform::links`.

use gpui::{div, prelude::*, AnyElement, Context, FontWeight};

use super::modal::modal;
use crate::app::Inlet;
use crate::backend::platform::links::Prompt;
use crate::theme;
use crate::ui::markdown;
use crate::ui::widgets::button;

impl Inlet {
    pub(super) fn render_link_prompt(&self, prompt: Prompt, cx: &mut Context<Self>) -> AnyElement {
        let title = |text: String| {
            div()
                .font_weight(FontWeight::BOLD)
                .text_color(theme::text_strong())
                .child(text)
                .into_any_element()
        };
        let text = |text: String| {
            div()
                .text_color(theme::text_muted())
                .child(text)
                .into_any_element()
        };
        let cancel = |label: String| {
            button("link-prompt-cancel", label, false).on_click(cx.listener(|this, _, _, cx| {
                this.overlays.link_prompt = None;
                cx.notify();
            }))
        };
        let mut children = Vec::new();
        let mut buttons = div().flex().flex_row().gap_2();
        match prompt {
            Prompt::OpenChannel {
                guild,
                channel,
                message: _,
                channel_name,
                guild_name,
            } => {
                children.push(title(t!("link-prompt-channel-title")));
                children.push(text(match guild_name {
                    Some(guild_name) => t!(
                        "link-prompt-channel-body",
                        channel = channel_name,
                        guild = guild_name
                    ),
                    None => t!("link-prompt-dm-body", channel = channel_name),
                }));
                buttons = buttons
                    .child(
                        button("link-prompt-open", t!("link-prompt-open"), true).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.overlays.link_prompt = None;
                                this.follow_channel_link(
                                    guild.clone(),
                                    channel.clone(),
                                    window,
                                    cx,
                                );
                            }),
                        ),
                    )
                    .child(cancel(t!("common-cancel")));
            }
            Prompt::UnknownChannel => {
                children.push(title(t!("link-prompt-unknown-title")));
                children.push(text(t!("link-prompt-unknown-body")));
                buttons = buttons.child(cancel(t!("link-prompt-dismiss")));
            }
            Prompt::Invite {
                url,
                host,
                known_instance,
            } => {
                children.push(title(t!("link-prompt-invite-title")));
                if !known_instance {
                    children.push(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_strong())
                            .child(t!("link-prompt-invite-other", host = host.as_str()))
                            .into_any_element(),
                    );
                }
                children.push(text(t!("link-prompt-invite-body", host = host.as_str())));
                // Open exactly what was checked, and only http(s).
                let parsed = markdown::openable(&url);
                buttons = buttons
                    .child(
                        button("link-prompt-invite", t!("link-prompt-invite-open"), true).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.overlays.link_prompt = None;
                                if let Some(u) = &parsed {
                                    cx.open_url(u.as_str());
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(cancel(t!("common-cancel")));
            }
            Prompt::SignInFirst => {
                children.push(title(t!("link-prompt-sign-in-title")));
                children.push(text(t!("link-prompt-sign-in-body")));
                buttons = buttons.child(cancel(t!("link-prompt-dismiss")));
            }
        }
        children.push(buttons.into_any_element());
        modal("link-prompt", 480., children).into_any_element()
    }
}
