// SPDX-License-Identifier: AGPL-3.0-or-later
//! Editing an own message: Up in an empty composer opens the account's last message in
//! the channel, Enter saves, Escape cancels, as in Fluxer. The edit gets its own editor
//! with the same limit, and the saved text carries `allowed_mentions` like a send.
//! The edit box takes the composer's place; the message list can show it in the row
//! instead with [`ComposerView::render_edit`].

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, Focusable, Subscription, Window};
use inlet_editor::{Editor, EditorEvent, Resolve};

use super::editor::{new_editor, ComposerEvent, ComposerView};
use crate::backend::composer::Mentions;
use crate::model::Message;
use crate::theme;

/// The message being edited.
pub struct InlineEdit {
    pub message_id: String,
    pub channel_id: String,
    pub editor: Entity<Editor>,
    /// The wire text it started with; saving it unchanged sends nothing.
    original: String,
    _events: Subscription,
}

/// The newest message in `messages` (oldest first) that the account wrote. Other people's
/// messages are skipped, never chosen.
pub fn last_editable<'a>(
    messages: impl DoubleEndedIterator<Item = &'a Message>,
    my_user_id: &str,
) -> Option<&'a Message> {
    if my_user_id.is_empty() {
        return None;
    }
    messages.rev().find(|m| m.author_id == my_user_id)
}

impl ComposerView {
    /// Opens `content` (wire text) of an own message for editing. `names` labels its pills.
    pub fn start_edit(
        &mut self,
        message_id: String,
        channel_id: String,
        content: &str,
        names: &dyn Resolve,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.channel.as_ref().map(|c| &c.channel_id) != Some(&channel_id) {
            return;
        }
        let editor = new_editor(self.max_len, cx);
        editor.update(cx, |e, cx| e.set_wire(content, names, window, cx));
        let original = editor.read(cx).to_wire();
        let events = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            if *event == EditorEvent::Submit {
                this.save_edit(window, cx);
            }
            cx.notify();
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.autocomplete = None;
        self.edit = Some(InlineEdit {
            message_id,
            channel_id,
            editor,
            original,
            _events: events,
        });
        cx.notify();
    }

    /// Enter in the edit box.
    pub(super) fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else {
            return;
        };
        let (wire, over, allowed) = {
            let buffer = edit.editor.read(cx).buffer();
            (
                buffer.to_wire(),
                buffer.is_over_limit(),
                buffer.allowed_mentions(),
            )
        };
        if over {
            self.error = Some(t!("composer-too-long", max = self.max_len.to_string()));
            window.play_system_bell();
            return;
        }
        let content = wire.trim().to_owned();
        // An empty edit would delete the text; Fluxer asks first. Here it is a cancel.
        if !content.is_empty() && content != edit.original.trim() {
            cx.emit(ComposerEvent::Edit {
                channel_id: edit.channel_id.clone(),
                message_id: edit.message_id.clone(),
                content,
                mentions: Mentions::from_editor(&allowed, false),
            });
        }
        self.cancel_edit(window, cx);
    }

    pub(super) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit = None;
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// The edit box: the editor and a hint line.
    pub fn render_edit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let edit = self.edit.as_ref()?;
        Some(
            div()
                .mx(px(6.))
                .mb(px(6.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .min_h(px(58.))
                        .px_4()
                        .py(px(18.))
                        .flex()
                        .items_center()
                        .rounded(px(8.))
                        .bg(theme::bg_textarea())
                        .border_1()
                        .border_color(theme::focus_ring())
                        .text_color(theme::text())
                        .text_size(px(16.))
                        .line_height(px(22.))
                        .child(edit.editor.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .text_size(px(12.))
                        .text_color(theme::text_tertiary())
                        .child(t!("composer-edit-title"))
                        .child(
                            div()
                                .id("edit-cancel")
                                .text_color(theme::link())
                                .cursor_pointer()
                                .child(t!("composer-edit-cancel"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.cancel_edit(window, cx)),
                                ),
                        )
                        .child(t!("composer-edit-enter"))
                        .child(
                            div()
                                .id("edit-save")
                                .text_color(theme::link())
                                .cursor_pointer()
                                .child(t!("composer-edit-save"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_edit(window, cx)),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::ui::composer) fn message(id: &str, author: &str) -> Message {
        Message {
            id: id.into(),
            channel_id: "1".into(),
            author_id: author.into(),
            author: author.into(),
            author_username: author.into(),
            author_bot: false,
            timestamp_ms: 0,
            timestamp: String::new(),
            content: format!("text {id}"),
            blocks: vec![],
            truncated: false,
            attachments: vec![],
            embeds: vec![],
            reply: None,
            mentions_me: false,
            edited: false,
        }
    }

    #[test]
    fn the_last_editable_message_is_my_newest() {
        let list = [
            message("1", "me"),
            message("2", "other"),
            message("3", "me"),
            message("4", "other"),
            message("5", "other"),
        ];
        // My newest message, past the newer ones of others.
        assert_eq!(
            last_editable(list.iter(), "me").map(|m| m.id.as_str()),
            Some("3")
        );
        // Nobody else's message is ever offered.
        let theirs = [message("1", "other"), message("2", "third")];
        assert!(last_editable(theirs.iter(), "me").is_none());
        // Before the account is known, nothing.
        assert!(last_editable(list.iter(), "").is_none());
    }
}
