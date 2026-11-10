// SPDX-License-Identifier: AGPL-3.0-or-later
//! Editing an own message: Up in an empty composer opens the account's last message in
//! the channel, Enter saves, Escape cancels, as in Fluxer. The edit gets its own editor
//! with the same limit, and the saved text carries `allowed_mentions` like a send. The
//! edit stays open until the server took it; a refused edit stays open with its text.
// Deviation: the edit box takes the composer's place instead of the message's row; the
// message list can draw it in the row with [`ComposerView::render_edit`].

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, Focusable, Subscription, Window};
use inlet_editor::{Editor, EditorEvent, Id, Resolve};
use inlet_protocol as p;
use inlet_state::{AccountState, ChannelId, GuildId, RoleId, UserId};

use super::editor::{new_editor, ComposerEvent, ComposerView};
use crate::backend::composer::Mentions;
use crate::theme;

/// The message being edited.
pub struct InlineEdit {
    pub message_id: String,
    pub channel_id: String,
    pub editor: Entity<Editor>,
    /// The wire text it started with; saving it unchanged sends nothing.
    original: String,
    /// Whether the original reply notified its author; `None` when not known.
    replied_user: Option<bool>,
    /// Saved; waiting for the server.
    pub saving: bool,
    /// The text last sent to the server.
    sent: Option<String>,
    _events: Subscription,
}

/// An own message to edit, as the message list gives it.
pub struct EditTarget {
    pub message_id: String,
    pub channel_id: String,
    /// The wire text.
    pub content: String,
    /// Whether the message, a reply, notified the author it replies to; `None` when not
    /// known (then an edit never adds that author).
    pub replied_user: Option<bool>,
}

/// Whether a reply notified the author of the message it replies to: that author is
/// among its mentions. `None` for a message that is no reply, or an unknown original.
fn reply_notified_author(m: &p::Message) -> Option<bool> {
    let original = m.extra.get("referenced_message")?;
    let author = original.get("author")?.get("id")?.as_str()?;
    Some(m.mentions.iter().any(|u| u.id == author))
}

/// The newest message of the account in the channel's window, never someone else's.
pub fn last_own(state: &AccountState, channel: ChannelId) -> Option<EditTarget> {
    let me = state.me_id();
    let m = state
        .messages(channel)?
        .iter()
        .rev()
        .find(|m| UserId::parse(&m.author.id) == Some(me))?;
    Some(EditTarget {
        message_id: m.id.clone(),
        channel_id: channel.to_string(),
        content: m.content.clone(),
        replied_user: reply_notified_author(m),
    })
}

/// Pill labels from the account state, for a message being edited.
pub struct StateNames<'a> {
    pub state: &'a AccountState,
    pub guild: Option<GuildId>,
}

impl Resolve for StateNames<'_> {
    fn user(&self, id: Id) -> Option<String> {
        let uid = UserId(id.0);
        let member = self.guild.and_then(|g| self.state.member(g, uid));
        let nick = member.and_then(|m| m.display_name()).map(str::to_owned);
        nick.or_else(|| self.state.user(uid).map(|u| u.display_name().to_owned()))
    }
    fn role(&self, id: Id) -> Option<String> {
        let guild = self.state.guild(self.guild?)?;
        guild.roles.get(RoleId(id.0)).map(|r| r.name.clone())
    }
    fn channel(&self, id: Id) -> Option<String> {
        self.state.channel(ChannelId(id.0))?.name.clone()
    }
}

impl ComposerView {
    /// Opens an own message for editing. `names` labels its pills.
    pub fn start_edit(
        &mut self,
        target: EditTarget,
        names: &dyn Resolve,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let EditTarget {
            message_id,
            channel_id,
            content,
            replied_user,
        } = target;
        if self.channel.as_ref().map(|c| &c.channel_id) != Some(&channel_id) {
            return;
        }
        let editor = new_editor(self.max_len, cx);
        editor.update(cx, |e, cx| e.set_wire(&content, names, window, cx));
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
            replied_user,
            saving: false,
            sent: None,
            _events: events,
        });
        cx.notify();
    }

    /// Enter in the edit box: sends the edit and keeps the box until the server answers.
    pub(super) fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else {
            return;
        };
        if edit.saving {
            return;
        }
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
        // Deviation: Fluxer asks whether to delete the message when an edit empties it;
        // here an empty edit is a cancel.
        if content.is_empty() || content == edit.original.trim() {
            self.cancel_edit(window, cx);
            return;
        }
        cx.emit(ComposerEvent::Edit {
            channel_id: edit.channel_id.clone(),
            message_id: edit.message_id.clone(),
            content: content.clone(),
            // Keep what the original did for its reply's author; when that is not
            // known, never let the edit add that author as a mention.
            mentions: Mentions::from_editor(&allowed, Some(edit.replied_user.unwrap_or(false))),
        });
        if let Some(edit) = &mut self.edit {
            edit.saving = true;
            edit.sent = Some(content);
        }
        self.error = None;
        cx.notify();
    }

    pub(super) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit = None;
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// The server took the edit: the box closes, unless the person typed more while it
    /// was saving; then it stays open with that text, to be saved again.
    pub fn on_edited(&mut self, message_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.as_mut().filter(|e| e.message_id == message_id) else {
            return;
        };
        let now = edit.editor.read(cx).buffer().to_wire().trim().to_owned();
        let sent = edit.sent.take();
        if sent.as_deref() == Some(now.as_str()) {
            self.cancel_edit(window, cx);
        } else {
            edit.saving = false;
            if let Some(sent) = sent {
                // What the server has now; saving the same again sends nothing.
                edit.original = sent;
            }
            cx.notify();
        }
    }

    /// The server refused the edit: the box stays open with its text and says why.
    /// `false` when that edit is no longer open (the caller says it elsewhere).
    pub fn on_edit_failed(
        &mut self,
        message_id: &str,
        reason: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = self.edit.as_mut().filter(|e| e.message_id == message_id) else {
            return false;
        };
        edit.saving = false;
        edit.sent = None;
        self.error = Some(t!("composer-edit-failed", error = reason));
        cx.notify();
        true
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
pub(crate) mod tests {
    use super::*;
    use inlet_state::MessagesPage;
    use serde_json::{json, Value};

    /// An account `10` in guild `100` with text channel `301` holding `messages`.
    pub(crate) fn state_with(messages: Value) -> (AccountState, ChannelId) {
        let everyone = (1u64 << 10) | (1 << 11) | (1 << 16);
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": { "id": "10", "username": "me" },
            "guilds": [{
                "id": "100",
                "properties": { "id": "100", "name": "G", "owner_id": "1" },
                "roles": [{ "id": "100", "name": "@everyone", "position": 0,
                            "permissions": everyone.to_string() }],
                "channels": [{ "id": "301", "type": 0, "name": "general", "position": 0,
                               "permission_overwrites": [] }],
                "members": [{ "user": { "id": "10", "username": "me" }, "roles": [] }]
            }]
        }))
        .unwrap();
        let mut state = AccountState::from_ready(&ready);
        let ch = ChannelId::parse("301").unwrap();
        let list: Vec<p::Message> = serde_json::from_value(messages).unwrap();
        state.begin_load(ch);
        state.apply_messages(ch, MessagesPage::latest(list, 50));
        (state, ch)
    }

    pub(in crate::ui::composer) fn message(id: u64, author: &str, content: &str) -> Value {
        json!({
            "id": (id << 22).to_string(), "channel_id": "301", "type": 0,
            "author": { "id": author, "username": format!("u{author}") },
            "content": content, "timestamp": "2026-11-10T10:00:00Z"
        })
    }

    #[test]
    fn the_last_own_message_is_the_newest_of_the_account() {
        let (state, ch) = state_with(json!([
            message(5, "20", "theirs, newest"),
            message(4, "10", "mine"),
            message(3, "10", "mine, older"),
        ]));
        let target = last_own(&state, ch).unwrap();
        assert_eq!(target.content, "mine");
        assert_eq!(target.replied_user, None, "not a reply");
        let (state, ch) = state_with(json!([message(5, "20", "only theirs")]));
        assert!(last_own(&state, ch).is_none());
    }

    #[test]
    fn a_reply_remembers_whether_it_notified_its_author() {
        let mut quiet = message(4, "10", "re");
        quiet["referenced_message"] = message(2, "20", "q");
        let mut loud = quiet.clone();
        loud["mentions"] = json!([{ "id": "20", "username": "u20" }]);
        let quiet: p::Message = serde_json::from_value(quiet).unwrap();
        let loud: p::Message = serde_json::from_value(loud).unwrap();
        assert_eq!(reply_notified_author(&quiet), Some(false));
        assert_eq!(reply_notified_author(&loud), Some(true));
    }
}
