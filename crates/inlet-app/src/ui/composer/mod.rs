// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message composer under the chat.

use gpui::{div, prelude::*, px, Context, Entity, Subscription, Window};

use crate::app::Inlet;
use crate::backend::composer::Cmd;
use crate::text_input::{InputEvent, TextInput};

pub struct ComposerState {
    pub input: Entity<TextInput>,
}

impl ComposerState {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Inlet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(t!("composer-placeholder-empty"), cx)
                .composer()
                .max_chars(4_000)
        });
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Submit => this.send_message(cx),
            },
        ));
        Self { input }
    }
}

impl Inlet {
    fn send_message(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel.clone() else {
            return;
        };
        let text = self.composer.input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer.input.update(cx, |c, cx| c.clear(cx));
        self.backend.send(Cmd::Send { channel_id, text });
    }

    pub(crate) fn render_composer(&self) -> impl IntoElement {
        div()
            .flex_none()
            .px(px(6.))
            .pb(px(6.))
            .pt_1()
            .child(self.composer.input.clone())
    }
}

impl Inlet {
    /// Events from the backend's composer domain.
    pub(crate) fn on_composer_event(
        &mut self,
        event: crate::backend::composer::Ev,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match event {}
    }
}
