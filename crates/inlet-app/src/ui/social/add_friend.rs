// SPDX-License-Identifier: AGPL-3.0-or-later
//! Adding a friend by username (`name`, or `name#0001` on instances with tags), as
//! Fluxer's `AddFriendForm`. A window over the page (a layer): Enter or the button
//! sends the request, the answer stays in the window until the next try.
// Deviation: Fluxer's desktop client shows the form as a tab of the friends page; here
// the Add Friend button opens it as a window, so the keyboard rules of layers apply.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, Focusable, FontWeight, Subscription, Window,
};

use super::SocialPart;
use crate::app::Inlet;
use crate::backend::social::relationships::Cmd;
use crate::text_input::{InputEvent, TextInput};
use crate::theme;
use crate::ui::overlays::modal::modal;
use crate::ui::widgets::button;

/// The add-friend window.
pub struct AddFriend {
    pub input: Entity<TextInput>,
    /// The answer to the last try: `Ok(name)` sent, `Err(reason)` not.
    pub result: Option<Result<String, String>>,
    /// The name of the request on its way.
    sending: Option<String>,
    _submit: Subscription,
}

/// Splits `name#0001` into the username and its tag. `None` when it cannot be a
/// username: empty, longer than 32 characters, or a tag that is not 1-4 digits.
pub fn parse_username(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    let (name, tag) = match text.rsplit_once('#') {
        Some((name, tag)) => (name, Some(tag)),
        None => (text, None),
    };
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 32 {
        return None;
    }
    if let Some(tag) = tag {
        if tag.is_empty() || tag.len() > 4 || !tag.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some((name.to_owned(), tag.map(str::to_owned)))
}

impl Inlet {
    /// Opens the window with the field focused.
    pub(crate) fn open_add_friend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| TextInput::new(t!("social-add-friend-placeholder"), cx).max_chars(40));
        let submit = cx.subscribe(&input, |this, _, _: &InputEvent, cx| {
            this.send_friend_request(cx)
        });
        let field = input.focus_handle(cx);
        self.focus_layer_field(&field, window, cx);
        self.social.add_friend = Some(AddFriend {
            input,
            result: None,
            sending: None,
            _submit: submit,
        });
        cx.notify();
    }

    fn send_friend_request(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.social.add_friend.as_mut() else {
            return;
        };
        let text = w.input.read(cx).text().to_owned();
        match parse_username(&text) {
            Some((username, discriminator)) => {
                w.sending = Some(text.trim().to_owned());
                w.result = None;
                self.social_action(
                    Cmd::AddFriend {
                        username,
                        discriminator,
                    },
                    cx,
                );
            }
            None => w.result = Some(Err(t!("social-add-friend-invalid"))),
        }
        cx.notify();
    }

    /// The request went out.
    pub(super) fn add_friend_sent(&mut self, cx: &mut Context<Self>) {
        if let Some(w) = self.social.add_friend.as_mut() {
            let name = w.sending.take().unwrap_or_default();
            w.result = Some(Ok(name));
            w.input.update(cx, |i, cx| i.clear(cx));
        }
    }

    /// The request was refused.
    pub(super) fn add_friend_failed(&mut self, reason: String) {
        if let Some(w) = self.social.add_friend.as_mut() {
            w.sending = None;
            w.result = Some(Err(reason));
        }
    }

    pub(super) fn render_add_friend(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(w) = self.social.add_friend.as_ref() else {
            return div().into_any_element();
        };
        let (input, result) = (w.input.clone(), w.result.clone());
        let busy = self.social_busy("add-friend");
        let line = result.map(|r| match r {
            Ok(name) => div()
                .text_size(px(14.))
                .text_color(theme::status_online())
                .child(t!(
                    "social-add-friend-sent",
                    name = crate::model::clean_name(&name)
                )),
            Err(reason) => div()
                .text_size(px(14.))
                .text_color(theme::danger())
                .child(reason),
        });
        let card = modal(
            "add-friend",
            480.,
            [
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(t!("social-add-friend"))
                    .into_any_element(),
                div()
                    .text_color(theme::text_muted())
                    .child(t!("social-add-friend-body"))
                    .into_any_element(),
                input.into_any_element(),
                div().children(line).into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("add-friend-cancel", t!("common-cancel"), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.social.add_friend = None;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button(
                            "add-friend-send",
                            if busy {
                                t!("social-sending")
                            } else {
                                t!("social-add-friend-send")
                            },
                            true,
                        )
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.send_friend_request(cx))),
                    )
                    .into_any_element(),
            ],
        );
        self.social_keys(SocialPart::AddFriend, card, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_and_tags_are_read_like_fluxer_takes_them() {
        assert_eq!(parse_username(" ada "), Some(("ada".into(), None)));
        assert_eq!(
            parse_username("ada#0042"),
            Some(("ada".into(), Some("0042".into())))
        );
        assert_eq!(
            parse_username("a#b#7"),
            Some(("a#b".into(), Some("7".into())))
        );
        assert_eq!(parse_username(""), None);
        assert_eq!(parse_username("ada#"), None);
        assert_eq!(parse_username("ada#12345"), None);
        assert_eq!(parse_username("ada#12a"), None);
        assert_eq!(parse_username(&"x".repeat(33)), None);
    }
}
