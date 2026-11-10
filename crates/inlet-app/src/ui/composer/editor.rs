// SPDX-License-Identifier: AGPL-3.0-or-later
//! The composer view: the `inlet-editor` text field and what surrounds it. It knows the
//! open channel, keeps drafts, runs autocomplete and the commands, and turns Enter into
//! a message. It talks to the rest of the app through [`ComposerEvent`]s; the app passes
//! the backend's answers back in.
//!
//! Keys: Enter sends and Shift+Enter breaks the line (the editor's bindings). While the
//! autocomplete list is open, Up and Down move in it and Enter or Tab pick; Escape closes
//! it, then cancels an edit or a reply. Up in an empty composer edits the account's last
//! message. Nothing here acts while an input method is composing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, prelude::*, px, App, Context, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, KeyBinding, Subscription, Task, WeakEntity, Window,
};
use inlet_editor::actions::{Down, Submit, Up};
use inlet_editor::{Editor, EditorEvent, EditorStyle, Trigger};
use inlet_protocol as p;
use inlet_state::{AccountState, ChannelId, GuildId};

use super::autocomplete::{self, Candidate, Pick, Section, Sources};
use super::commands::{self, Parsed};
use super::drafts::{Draft, Drafts};
use super::edit_inline::InlineEdit;
use super::reply_bar::Reply;
use super::slowmode::Slowmode;
use crate::backend::composer::{Directory, Mentions, Outgoing, Person, Rights};
use crate::theme;

actions!(inlet_composer, [AcceptCompletion, Cancel]);

/// The key context of the composer, around the editor's own.
pub const KEY_CONTEXT: &str = "inlet_composer";

/// Guilds whose searched members are kept.
const MAX_GUILDS: usize = 32;
/// How often the UI tells the backend the person is typing (the protocol sends at most
/// one request per channel every 8 s).
const TYPING_EVERY: Duration = Duration::from_secs(1);
/// Wait after the last key before a member search goes out.
const SEARCH_DELAY: Duration = Duration::from_millis(250);
/// Wait after the last change before drafts are written to disk.
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// The composer's keys on top of the editor's. Call it after the app-wide bindings.
///
/// The composer's own keys act in an editor inside the composer (the box or the edit
/// box). gpui ranks a binding by how deep its context matches, and a binding with
/// no context counts as matching at the focused element, so the app-wide Escape (mark
/// as read) and Tab (next field) would outrank a binding on the composer itself. Bound
/// at the editor and after the app-wide keys, these come first; their handlers pass the
/// key on when there is nothing to do.
pub fn bind_keys(cx: &mut App) {
    inlet_editor::bind_keys(cx);
    let context = format!("{KEY_CONTEXT} > {}", inlet_editor::KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("tab", AcceptCompletion, Some(context.as_str())),
        KeyBinding::new("escape", Cancel, Some(context.as_str())),
    ]);
}

/// The open channel, as the composer needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelContext {
    pub channel_id: String,
    /// `None` in direct messages.
    pub guild_id: Option<String>,
    /// "Message #general".
    pub placeholder: String,
    pub slowmode_secs: u32,
}

/// What the composer asks of the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerEvent {
    Send(Outgoing),
    Edit {
        channel_id: String,
        message_id: String,
        content: String,
        mentions: Mentions,
    },
    Typing {
        channel_id: String,
    },
    SearchMembers {
        guild_id: String,
        query: String,
        nonce: String,
    },
    SetNick {
        guild_id: String,
        nick: Option<String>,
    },
    DirectMessage {
        user_id: String,
        message: Outgoing,
    },
    /// Up in an empty composer: the app finds the account's last message in the channel
    /// and calls [`ComposerView::start_edit`].
    EditLast {
        channel_id: String,
    },
}

/// State the app shares with the composer: the drafts survive the view, and sign-out
/// (which has no window at hand) clears them here.
#[derive(Default)]
pub struct Shared {
    pub drafts: Drafts,
}

impl Shared {
    /// Sign-out: the account's drafts are deleted at once; the view forgets the rest
    /// (reply, edit, failed sends) when it opens the next channel.
    pub fn sign_out(&mut self) {
        self.drafts.sign_out();
    }
}

/// The open autocomplete list.
pub struct AutocompleteState {
    pub section: Section,
    pub rows: Vec<Candidate>,
    pub selected: usize,
    /// Where the trigger starts, to keep the selection while the query grows.
    start: usize,
}

pub struct ComposerView {
    pub(super) editor: Entity<Editor>,
    pub(super) max_len: usize,
    pub(super) channel: Option<ChannelContext>,
    pub(super) shared: Rc<RefCell<Shared>>,
    seen_generation: u64,
    /// The account state on the UI thread: roles, channels, emoji and rights.
    account: Option<WeakEntity<AccountState>>,
    /// Autocomplete data of the open channel, from the account state.
    pub(super) directory: Option<(String, Directory)>,
    /// Members per guild: from the account state and from searches.
    members: HashMap<String, Vec<Person>>,
    pub(super) autocomplete: Option<AutocompleteState>,
    /// The trigger start the person closed the list at; it stays closed there.
    dismissed_at: Option<usize>,
    pub(super) reply: Option<Reply>,
    pub(super) edit: Option<InlineEdit>,
    pub(super) slowmode: Slowmode,
    /// A sentence under the box (too long, a command that needs a guild).
    pub(super) error: Option<String>,
    clock: Instant,
    /// Moves the clock forward (tests only; 0 otherwise).
    skew_ms: u64,
    last_typing: Option<Instant>,
    last_search: Option<(String, String)>,
    /// Text the composer put in itself (a restored draft): not typing.
    quiet_text: Option<String>,
    /// The text of the last `/msg`, put back if it fails.
    dm_backup: Option<(String, Draft)>,
    search_task: Option<Task<()>>,
    ticker: Option<Task<()>>,
    save_task: Option<Task<()>>,
    _editor_events: Subscription,
}

impl EventEmitter<ComposerEvent> for ComposerView {}

impl Focusable for ComposerView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

pub(super) fn editor_style() -> EditorStyle {
    EditorStyle {
        text: theme::text(),
        placeholder: theme::text_tertiary(),
        selection: theme::selection(),
        caret: theme::text_strong(),
        pill_background: theme::mention_fill(),
        pill_text: theme::mention_text(),
        pill_radius: px(4.),
        max_height_fraction: 0.5,
    }
}

/// An editor with the composer's look. `max_len` is the instance's limit.
pub(super) fn new_editor(max_len: usize, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::new(max_len, cx);
        editor.set_style(editor_style(), cx);
        editor
    })
}

impl ComposerView {
    /// `max_len` is the message length limit until the instance's own is known.
    pub fn new(
        shared: Rc<RefCell<Shared>>,
        max_len: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = new_editor(max_len, cx);
        let sub = Self::watch(&editor, window, cx);
        let seen_generation = shared.borrow().drafts.generation();
        Self {
            editor,
            max_len,
            channel: None,
            shared,
            seen_generation,
            account: None,
            directory: None,
            members: HashMap::new(),
            autocomplete: None,
            dismissed_at: None,
            reply: None,
            edit: None,
            slowmode: Slowmode::default(),
            error: None,
            clock: Instant::now(),
            skew_ms: 0,
            last_typing: None,
            last_search: None,
            quiet_text: None,
            dm_backup: None,
            search_task: None,
            ticker: None,
            save_task: None,
            _editor_events: sub,
        }
    }

    fn watch(editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(
            editor,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                match event {
                    EditorEvent::Changed => {
                        this.store_draft(cx);
                        // Text the composer put in itself is no typing and keeps the error
                        // line (a failed /msg puts its text back).
                        let text = this.editor.read(cx).buffer().text();
                        if this.quiet_text.as_deref() == Some(text) {
                            this.quiet_text = None;
                        } else {
                            this.quiet_text = None;
                            this.error = None;
                            this.typing(cx);
                        }
                        this.update_autocomplete(cx);
                    }
                    EditorEvent::SelectionChanged => this.update_autocomplete(cx),
                    EditorEvent::Submit => this.submit(window, cx),
                }
                cx.notify();
            },
        )
    }

    pub(super) fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64 + self.skew_ms
    }

    pub(super) fn directory(&self) -> Option<&Directory> {
        let (channel, directory) = self.directory.as_ref()?;
        (Some(channel) == self.channel.as_ref().map(|c| &c.channel_id)).then_some(directory)
    }

    /// The account state to read roles, channels, emoji and rights from.
    pub fn set_account(
        &mut self,
        account: Option<WeakEntity<AccountState>>,
        cx: &mut Context<Self>,
    ) {
        self.account = account;
        self.refresh_directory(cx);
    }

    /// Reads the open channel's autocomplete data and rights from the account state.
    pub(super) fn refresh_directory(&mut self, cx: &mut Context<Self>) {
        let account = self.account.as_ref().and_then(WeakEntity::upgrade);
        let ids = self.channel.as_ref().and_then(|ch| {
            let guild = ch.guild_id.as_deref()?;
            Some((
                guild.to_owned(),
                ch.channel_id.clone(),
                GuildId::parse(guild)?,
                ChannelId::parse(&ch.channel_id)?,
            ))
        });
        let (Some(account), Some((guild, channel, g, c))) = (account, ids) else {
            self.directory = None;
            return;
        };
        let (directory, members) = autocomplete::from_account_state(account.read(cx), g, c);
        if self.members.len() >= MAX_GUILDS && !self.members.contains_key(&guild) {
            self.members.clear();
        }
        autocomplete::remember_members(self.members.entry(guild).or_default(), members);
        self.directory = Some((channel, directory));
    }

    pub(super) fn known_members(&self) -> &[Person] {
        self.channel
            .as_ref()
            .and_then(|c| c.guild_id.as_ref())
            .and_then(|g| self.members.get(g))
            .map_or(&[], Vec::as_slice)
    }

    /// The account's rights in the open channel.
    pub(super) fn rights(&self) -> Rights {
        match (self.directory(), &self.channel) {
            (Some(d), Some(ch)) => d.rights(&ch.channel_id),
            _ => Rights::default(),
        }
    }

    pub(super) fn bypasses_slowmode(&self) -> bool {
        self.rights().bypass_slowmode
    }

    /// Seconds left in the open channel's slowmode.
    pub(super) fn slowmode_left(&self) -> u32 {
        let Some(ch) = &self.channel else {
            return 0;
        };
        let rate = if ch.guild_id.is_some() {
            ch.slowmode_secs
        } else {
            0
        };
        self.slowmode.remaining_secs(
            &ch.channel_id,
            rate,
            self.bypasses_slowmode(),
            self.now_ms(),
        )
    }

    // --- channel, session and sign-out -----------------------------------------------

    /// After a sign-out, forget everything of the old account.
    fn check_generation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.shared.borrow().drafts.generation();
        if generation == self.seen_generation {
            return;
        }
        self.seen_generation = generation;
        self.channel = None;
        self.directory = None;
        self.members.clear();
        self.autocomplete = None;
        self.dismissed_at = None;
        self.reply = None;
        self.edit = None;
        self.slowmode.clear();
        self.error = None;
        self.last_search = None;
        self.search_task = None;
        self.ticker = None;
        self.save_task = None;
        self.editor.update(cx, |e, cx| e.clear(window, cx));
    }

    /// Opens a channel: keeps the draft of the one before and brings back this one's.
    pub fn set_channel(
        &mut self,
        ctx: ChannelContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_generation(window, cx);
        if self.channel.as_ref().map(|c| &c.channel_id) == Some(&ctx.channel_id) {
            // The same channel again (it changed, for example its slowmode).
            self.editor
                .update(cx, |e, cx| e.set_placeholder(ctx.placeholder.clone(), cx));
            self.channel = Some(ctx);
            self.refresh_directory(cx);
            self.start_ticker(cx);
            cx.notify();
            return;
        }
        self.store_draft(cx);
        self.autocomplete = None;
        self.dismissed_at = None;
        self.reply = None;
        self.edit = None;
        self.error = None;
        let draft = self.shared.borrow().drafts.get(&ctx.channel_id).cloned();
        self.editor.update(cx, |e, cx| {
            match &draft {
                Some(d) => e.set_wire(&d.wire, d, window, cx),
                None => e.clear(window, cx),
            }
            e.set_placeholder(ctx.placeholder.clone(), cx);
        });
        self.quiet_text = Some(self.editor.read(cx).buffer().text().to_owned());
        self.channel = Some(ctx);
        self.refresh_directory(cx);
        self.start_ticker(cx);
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// The instance's limit is known (and, with the account, where drafts are kept).
    pub fn set_session(
        &mut self,
        max_len: usize,
        drafts_file: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_generation(window, cx);
        if let Some(path) = drafts_file {
            self.shared.borrow_mut().drafts.attach(path);
        }
        if max_len != self.max_len {
            // The limit is fixed when an editor is made: make a new one with the text.
            let draft = Draft::of(self.editor.read(cx).buffer());
            let editor = new_editor(max_len, cx);
            editor.update(cx, |e, cx| {
                if let Some(d) = &draft {
                    e.set_wire(&d.wire, d, window, cx);
                }
                if let Some(ch) = &self.channel {
                    e.set_placeholder(ch.placeholder.clone(), cx);
                }
            });
            self.quiet_text = Some(editor.read(cx).buffer().text().to_owned());
            self._editor_events = Self::watch(&editor, window, cx);
            let focused = self.editor.focus_handle(cx).is_focused(window);
            self.editor = editor;
            self.max_len = max_len;
            if focused {
                window.focus(&self.editor.focus_handle(cx), cx);
            }
        }
        cx.notify();
    }

    /// Keeps the text of the open channel as its draft.
    fn store_draft(&mut self, cx: &mut Context<Self>) {
        let Some(ch) = &self.channel else {
            return;
        };
        if self.shared.borrow().drafts.generation() != self.seen_generation {
            return;
        }
        let draft = Draft::of(self.editor.read(cx).buffer());
        self.shared.borrow_mut().drafts.put(&ch.channel_id, draft);
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.save_task.is_some() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.save_task = None;
                if let Some(write) = this.shared.borrow_mut().drafts.prepare_write() {
                    cx.background_spawn(async move { write.run() }).detach();
                }
            });
        }));
    }

    // --- typing and autocomplete -------------------------------------------------------

    fn typing(&mut self, cx: &mut Context<Self>) {
        let Some(ch) = &self.channel else {
            return;
        };
        let text = self.editor.read(cx).buffer().text();
        if text.trim().is_empty() || text.starts_with('/') || self.edit.is_some() {
            return;
        }
        if self.last_typing.is_some_and(|t| t.elapsed() < TYPING_EVERY) {
            return;
        }
        self.last_typing = Some(Instant::now());
        cx.emit(ComposerEvent::Typing {
            channel_id: ch.channel_id.clone(),
        });
    }

    pub(super) fn update_autocomplete(&mut self, cx: &mut Context<Self>) {
        let trigger = self.editor.read(cx).trigger();
        let Some((range, trigger)) = trigger else {
            self.autocomplete = None;
            self.dismissed_at = None;
            return;
        };
        if self.dismissed_at == Some(range.start) {
            self.autocomplete = None;
            return;
        }
        self.dismissed_at = None;
        if self.autocomplete.is_none() {
            // A list opens: take the account state as it is now.
            self.refresh_directory(cx);
        }
        let guild = self.channel.as_ref().and_then(|c| c.guild_id.clone());
        if let (Trigger::Mention(q), Some(g)) = (&trigger, &guild) {
            self.search_members(g.clone(), q.clone(), cx);
        }
        let src = Sources {
            directory: self.directory(),
            members: self.known_members(),
            in_guild: guild.is_some(),
            rights: self.rights(),
        };
        let (section, rows) = autocomplete::candidates(&trigger, &src);
        let selected = match &self.autocomplete {
            Some(open) if open.start == range.start && open.section == section => {
                open.selected.min(rows.len().saturating_sub(1))
            }
            _ => 0,
        };
        self.autocomplete = Some(AutocompleteState {
            section,
            rows,
            selected,
            start: range.start,
        });
    }

    fn search_members(&mut self, guild_id: String, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_owned();
        if query.is_empty() || self.last_search == Some((guild_id.clone(), query.clone())) {
            return;
        }
        self.last_search = Some((guild_id.clone(), query.clone()));
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let _ = this.update(cx, |_, cx| {
                // A short nonce for the gateway (at most 32 characters).
                let nonce = p::new_nonce();
                cx.emit(ComposerEvent::SearchMembers {
                    guild_id,
                    query,
                    nonce,
                });
            });
        }));
    }

    fn autocomplete_open(&self, cx: &App) -> bool {
        self.autocomplete
            .as_ref()
            .is_some_and(|a| !a.rows.is_empty())
            && self.editor.read(cx).marked_range().is_none()
    }

    fn move_selection(&mut self, down: bool) {
        if let Some(open) = &mut self.autocomplete {
            let n = open.rows.len();
            if n > 0 {
                open.selected = if down {
                    (open.selected + 1) % n
                } else {
                    (open.selected + n - 1) % n
                };
            }
        }
    }

    /// Puts row `ix` of the list into the message.
    pub(super) fn pick(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.autocomplete.take() else {
            return;
        };
        let Some(row) = open.rows.into_iter().nth(ix) else {
            return;
        };
        self.editor.update(cx, |e, cx| match row.pick {
            Pick::Token(token, label) => {
                if e.complete_trigger(token, &label, window, cx) {
                    let _ = e.insert_text(" ", window, cx);
                }
            }
            Pick::Text(text) => {
                let Some((range, _)) = e.trigger() else {
                    return;
                };
                let full = e.buffer().text();
                let start = full[..range.start].encode_utf16().count();
                let end = start + full[range].encode_utf16().count();
                e.replace_text_in_range(Some(start..end), &text, window, cx);
            }
        });
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    // --- keys --------------------------------------------------------------------------

    fn on_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.autocomplete_open(cx) {
            self.move_selection(false);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let editor = self.editor.read(cx);
        if editor.buffer().is_empty() && editor.marked_range().is_none() && self.edit.is_none() {
            if let Some(ch) = &self.channel {
                cx.emit(ComposerEvent::EditLast {
                    channel_id: ch.channel_id.clone(),
                });
                cx.stop_propagation();
            }
        }
    }

    fn on_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.autocomplete_open(cx) {
            self.move_selection(true);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_submit_key(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.autocomplete_open(cx) {
            let ix = self.autocomplete.as_ref().map_or(0, |a| a.selected);
            self.pick(ix, window, cx);
            cx.stop_propagation();
        }
    }

    fn on_tab(&mut self, _: &AcceptCompletion, window: &mut Window, cx: &mut Context<Self>) {
        if self.autocomplete_open(cx) {
            let ix = self.autocomplete.as_ref().map_or(0, |a| a.selected);
            self.pick(ix, window, cx);
        } else {
            cx.propagate();
        }
    }

    /// Escape closes what the composer has open: the list, an edit, a reply. Only when
    /// nothing is open does it go on to the app (which marks the channel read); while an
    /// input method composes, or an edit is open or saving, it never does.
    fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        let composing = self.editor.read(cx).marked_range().is_some()
            || self
                .edit
                .as_ref()
                .is_some_and(|e| e.editor.read(cx).marked_range().is_some());
        if composing {
            // The input method's key: nothing here acts on it.
            return;
        }
        if let Some(open) = self.autocomplete.take() {
            self.dismissed_at = Some(open.start);
        } else if let Some(edit) = &self.edit {
            if edit.saving {
                // A saved edit stays until the server answered, so its text is not lost.
                return;
            }
            self.cancel_edit(window, cx);
        } else if self.reply.is_some() {
            self.reply = None;
        } else {
            cx.propagate();
            return;
        }
        cx.notify();
    }

    // --- sending -------------------------------------------------------------------------

    /// Enter: a command, or a message.
    pub(super) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ch) = self.channel.clone() else {
            return;
        };
        let (wire, text, over, allowed) = {
            let buffer = self.editor.read(cx).buffer();
            (
                buffer.to_wire(),
                buffer.text().to_owned(),
                buffer.is_over_limit(),
                buffer.allowed_mentions(),
            )
        };
        let empty = text.trim().is_empty();
        if empty {
            return;
        }
        if over {
            self.error = Some(t!("composer-too-long", max = self.max_len.to_string()));
            window.play_system_bell();
            return;
        }
        let content = match commands::parse(&wire) {
            Parsed::Nick(_) => {
                // The nickname is what the person sees, not the wire text with its
                // escapes and tags.
                let nick = commands::nick_from_display(&text);
                match ch.guild_id.clone() {
                    Some(guild_id) => {
                        cx.emit(ComposerEvent::SetNick { guild_id, nick });
                        self.sent(window, cx);
                    }
                    None => self.error = Some(t!("composer-nick-guild-only")),
                }
                return;
            }
            Parsed::Msg { user_id, message } => {
                let nonce = p::new_nonce();
                if let Some(draft) = Draft::of(self.editor.read(cx).buffer()) {
                    self.dm_backup = Some((nonce.clone(), draft));
                }
                cx.emit(ComposerEvent::DirectMessage {
                    user_id,
                    message: Outgoing {
                        channel_id: String::new(),
                        nonce,
                        content: message,
                        mentions: Mentions::from_editor(&allowed, None),
                        reply_to: None,
                        silent: false,
                    },
                });
                self.sent(window, cx);
                return;
            }
            Parsed::Saved => {
                // Deviation: `/saved` needs the saved media picker, which is not built yet.
                self.error = Some(t!("composer-saved-not-yet"));
                return;
            }
            Parsed::Wrapped(text) => text,
            Parsed::Text => wire.trim().to_owned(),
        };
        let (content, silent) = match p::models::strip_silent_prefix(&content) {
            Some(rest) => (rest.trim().to_owned(), true),
            None => (content.trim().to_owned(), false),
        };
        if content.is_empty() {
            return;
        }
        if self.slowmode_left() > 0 {
            window.play_system_bell();
            return;
        }
        let reply = self.reply.take();
        let outgoing = Outgoing {
            channel_id: ch.channel_id.clone(),
            nonce: p::new_nonce(),
            content,
            mentions: Mentions::from_editor(&allowed, reply.as_ref().map(|r| r.mention)),
            reply_to: reply.map(|r| r.message_id),
            silent,
        };
        // The app shows it in the message list as pending (by its nonce) and sends it.
        cx.emit(ComposerEvent::Send(outgoing));
        self.count_send(&ch, cx);
        self.sent(window, cx);
    }

    /// Starts the slowmode countdown after a send in `ch`, unless the account may skip it.
    fn count_send(&mut self, ch: &ChannelContext, cx: &mut Context<Self>) {
        if ch.guild_id.is_some() && ch.slowmode_secs > 0 && !self.bypasses_slowmode() {
            let now = self.now_ms();
            self.slowmode.record_send(&ch.channel_id, now);
            self.start_ticker(cx);
        }
    }

    /// Empties the composer after a send or a command.
    fn sent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.autocomplete = None;
        self.reply = None;
        self.editor.update(cx, |e, cx| e.clear(window, cx));
        if let Some(ch) = &self.channel {
            self.shared.borrow_mut().drafts.put(&ch.channel_id, None);
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// A message of `channel_id` goes out again (Retry in the message list): the
    /// slowmode countdown starts again.
    pub fn on_resend(&mut self, channel_id: &str, cx: &mut Context<Self>) {
        if let Some(ch) = self.channel.clone().filter(|c| c.channel_id == channel_id) {
            self.count_send(&ch, cx);
        }
        cx.notify();
    }

    /// Ticks once a second while a countdown runs.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() || self.slowmode_left() == 0 {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let running = this
                .update(cx, |this, cx| {
                    cx.notify();
                    let left = this.slowmode_left();
                    if left == 0 {
                        this.ticker = None;
                    }
                    left > 0
                })
                .unwrap_or(false);
            if !running {
                return;
            }
        }));
    }

    // --- answers from the backend ---------------------------------------------------------

    /// The server refused a send and asked to wait: the countdown takes it.
    pub fn on_slowmode_wait(&mut self, channel_id: &str, secs: u64, cx: &mut Context<Self>) {
        let now = self.now_ms();
        self.slowmode.server_wait(channel_id, secs, now);
        self.start_ticker(cx);
        cx.notify();
    }

    pub fn on_members(&mut self, guild_id: String, members: Vec<Person>, cx: &mut Context<Self>) {
        if self.members.len() >= MAX_GUILDS && !self.members.contains_key(&guild_id) {
            self.members.clear();
        }
        autocomplete::remember_members(self.members.entry(guild_id).or_default(), members);
        self.update_autocomplete(cx);
        cx.notify();
    }

    /// A `/msg` failed: say why, and put its text back if the box is still empty.
    pub fn on_direct_message_failed(
        &mut self,
        nonce: &str,
        reason: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = Some(t!("composer-msg-failed", error = reason));
        let backup = self.dm_backup.take().filter(|(n, _)| n == nonce);
        if let Some((_, draft)) = backup {
            if self.editor.read(cx).buffer().is_empty() {
                self.editor
                    .update(cx, |e, cx| e.set_wire(&draft.wire, &draft, window, cx));
                self.quiet_text = Some(self.editor.read(cx).buffer().text().to_owned());
            }
        }
        cx.notify();
    }
}

impl Render for ComposerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.edit.is_some();
        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_cancel))
            .flex_none()
            .flex()
            .flex_col()
            .children(self.render_autocomplete(cx))
            .child(self.render_status_row(window, cx))
            .when(editing, |d| d.children(self.render_edit(cx)))
            .when(!editing, |d| {
                d.children(self.render_reply_bar(cx))
                    .child(self.render_box(cx))
            })
    }
}

impl ComposerView {
    /// The text box: 58 px high at rest, 8 px corners, 6 px from the sides and bottom.
    fn render_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let joined = self.reply.is_some();
        div()
            .mx(px(6.))
            .mb(px(6.))
            .min_h(px(58.))
            .px_4()
            .py(px(18.))
            .flex()
            .items_center()
            .when(joined, |d| d.rounded_b(px(8.)))
            .when(!joined, |d| d.rounded(px(8.)))
            .bg(theme::bg_textarea())
            .border_1()
            .border_color(theme::ring())
            .text_color(theme::text())
            .text_size(px(16.))
            .line_height(px(22.))
            .capture_action(cx.listener(Self::on_up))
            .capture_action(cx.listener(Self::on_down))
            .capture_action(cx.listener(Self::on_submit_key))
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    struct Host {
        view: Entity<ComposerView>,
        events: Vec<ComposerEvent>,
        /// Escape reached the app's "mark as read".
        marked_read: usize,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(600.))
                .on_action(
                    cx.listener(|h, _: &crate::ui::notifications::MarkChannelRead, _, _| {
                        h.marked_read += 1
                    }),
                )
                .child(self.view.clone())
        }
    }

    /// A window with the composer, and the app's key bindings in the app's order.
    fn host(
        cx: &mut TestAppContext,
    ) -> (Entity<Host>, Entity<ComposerView>, &mut VisualTestContext) {
        cx.update(crate::ui::notifications::bind_keys);
        cx.update(bind_keys);
        let (host, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| ComposerView::new(Rc::default(), 2000, window, cx));
            cx.subscribe(&view, |host: &mut Host, _, event: &ComposerEvent, _| {
                host.events.push(event.clone());
            })
            .detach();
            Host {
                view,
                events: Vec::new(),
                marked_read: 0,
            }
        });
        let view = host.read_with(cx, |h, _| h.view.clone());
        (host, view, cx)
    }

    fn open(view: &Entity<ComposerView>, id: &str, cx: &mut VisualTestContext) {
        view.update_in(cx, |v, window, cx| {
            v.set_channel(
                ChannelContext {
                    channel_id: id.into(),
                    guild_id: Some("100".into()),
                    placeholder: format!("#{id}"),
                    slowmode_secs: 0,
                },
                window,
                cx,
            )
        });
    }

    fn type_text(view: &Entity<ComposerView>, text: &str, cx: &mut VisualTestContext) {
        let editor = view.read_with(cx, |v, _| v.editor.clone());
        editor.update_in(cx, |e, window, cx| {
            e.replace_text_in_range(None, text, window, cx)
        });
    }

    fn sends(host: &Entity<Host>, cx: &mut VisualTestContext) -> Vec<Outgoing> {
        host.read_with(cx, |h, _| {
            h.events
                .iter()
                .filter_map(|e| match e {
                    ComposerEvent::Send(o) => Some(o.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    #[gpui::test]
    fn enter_sends_shift_enter_breaks_line(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        open(&view, "1", cx);
        type_text(&view, "hello", cx);
        cx.simulate_keystrokes("shift-enter");
        type_text(&view, "world", cx);
        assert!(sends(&host, cx).is_empty(), "Shift+Enter does not send");
        let text = view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned());
        assert_eq!(text, "hello\nworld");
        cx.simulate_keystrokes("enter");
        let sent = sends(&host, cx);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "hello\nworld");
        assert_eq!(sent[0].channel_id, "1");
        assert_eq!(
            sent[0].mentions,
            Mentions::default(),
            "nothing picked, nothing pings"
        );
        let text = view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned());
        assert_eq!(text, "", "the composer is empty after sending");
        cx.simulate_keystrokes("enter");
        assert_eq!(sends(&host, cx).len(), 1, "an empty composer sends nothing");
    }

    fn edit_requests(host: &Entity<Host>, cx: &mut VisualTestContext) -> usize {
        host.read_with(cx, |h, _| {
            h.events
                .iter()
                .filter(|e| matches!(e, ComposerEvent::EditLast { .. }))
                .count()
        })
    }

    /// The account state of `edit_inline`'s tests, as the app holds it.
    fn account(
        messages: serde_json::Value,
        cx: &mut VisualTestContext,
    ) -> (Entity<AccountState>, ChannelId) {
        let (state, ch) = super::super::edit_inline::tests::state_with(messages);
        (cx.update(|_, cx| cx.new(|_| state)), ch)
    }

    fn edits(host: &Entity<Host>, cx: &mut VisualTestContext) -> Vec<(String, String, Mentions)> {
        host.read_with(cx, |h, _| {
            h.events
                .iter()
                .filter_map(|e| match e {
                    ComposerEvent::Edit {
                        message_id,
                        content,
                        mentions,
                        ..
                    } => Some((message_id.clone(), content.clone(), mentions.clone())),
                    _ => None,
                })
                .collect()
        })
    }

    #[gpui::test]
    fn up_arrow_edits_my_last_message_only(cx: &mut TestAppContext) {
        use super::super::edit_inline::tests::message;
        let (host, view, cx) = host(cx);
        open(&view, "301", cx);
        // With text in the composer, Up moves the caret and edits nothing.
        type_text(&view, "draft", cx);
        cx.simulate_keystrokes("up");
        assert_eq!(edit_requests(&host, cx), 0);
        view.update_in(cx, |v, window, cx| {
            v.editor.update(cx, |e, cx| e.clear(window, cx))
        });
        // Empty: Up asks for the last own message.
        cx.simulate_keystrokes("up");
        assert_eq!(edit_requests(&host, cx), 1);
        // The app's handler opens the newest message of the account (10) from the account
        // state, never someone else's, even when theirs is newer.
        let (account, ch) = account(
            serde_json::json!([
                message(3, "10", "older"),
                message(4, "10", "old <@5>"),
                message(5, "20", "theirs"),
            ]),
            cx,
        );
        cx.update(|window, cx| crate::ui::composer::edit_last(&view, &account, ch, window, cx));
        let mine = (4u64 << 22).to_string();
        let editing = view.read_with(cx, |v, _| v.edit.as_ref().map(|e| e.message_id.clone()));
        assert_eq!(editing.as_deref(), Some(mine.as_str()));
        let edit_editor = view.read_with(cx, |v, _| v.edit.as_ref().map(|e| e.editor.clone()));
        edit_editor
            .expect("an edit is open")
            .update_in(cx, |e, window, cx| {
                e.replace_text_in_range(None, " new", window, cx)
            });
        cx.simulate_keystrokes("enter");
        let edits = edits(&host, cx);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, mine);
        assert_eq!(edits[0].1, "old <@5> new");
        assert_eq!(
            edits[0].2.users,
            [5],
            "the edit carries allowed_mentions too"
        );
        assert_eq!(
            edits[0].2.replied_user,
            Some(false),
            "an edit never adds the replied author"
        );
        assert!(
            sends(&host, cx).is_empty(),
            "saving an edit sends no message"
        );
        // The box stays until the server took the edit.
        assert!(view.read_with(cx, |v, _| v.edit.as_ref().is_some_and(|e| e.saving)));
        view.update_in(cx, |v, window, cx| v.on_edited(&mine, window, cx));
        assert!(view.read_with(cx, |v, _| v.edit.is_none()));
    }

    #[gpui::test]
    fn draft_survives_channel_switch_and_is_cleared_on_sign_out(cx: &mut TestAppContext) {
        let (_host, view, cx) = host(cx);
        let dir = std::env::temp_dir().join(format!(
            "inlet-composer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = super::super::drafts::path_for(&dir, "fluxer.example", "7").unwrap();
        view.update_in(cx, |v, window, cx| {
            v.set_session(2000, Some(file.clone()), window, cx)
        });
        open(&view, "1", cx);
        view.update(cx, |v, cx| {
            v.on_members(
                "100".into(),
                vec![Person {
                    id: 42,
                    name: "ada".into(),
                    username: "ada".into(),
                }],
                cx,
            )
        });
        type_text(&view, "half a thought @ad", cx);
        cx.simulate_keystrokes("tab");
        open(&view, "2", cx);
        let text = |cx: &mut VisualTestContext| {
            view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned())
        };
        assert_eq!(text(cx), "", "the other channel has no draft");
        type_text(&view, "second", cx);
        open(&view, "1", cx);
        assert_eq!(
            text(cx),
            "half a thought @ada ",
            "the draft and its pill are back"
        );
        let pills = view.read_with(cx, |v, cx| v.editor.read(cx).buffer().tokens().len());
        assert_eq!(pills, 1);
        // On disk, until sign-out.
        let shared = view.read_with(cx, |v, _| v.shared.clone());
        if let Some(write) = shared.borrow_mut().drafts.prepare_write() {
            write.run();
        }
        assert!(file.exists());
        // The sign-out the app runs when the account signs out or its session ends.
        shared.borrow_mut().sign_out();
        assert!(!file.exists(), "sign-out deletes the drafts file");
        open(&view, "2", cx);
        assert_eq!(text(cx), "", "no draft of the old account");
        open(&view, "1", cx);
        assert_eq!(text(cx), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_picked_mention_is_a_pill_and_the_only_allowed_mention(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        open(&view, "1", cx);
        view.update(cx, |v, cx| {
            v.on_members(
                "100".into(),
                vec![Person {
                    id: 42,
                    name: "ada".into(),
                    username: "ada".into(),
                }],
                cx,
            )
        });
        type_text(&view, "hi <@7> @ad", cx);
        cx.simulate_keystrokes("tab");
        type_text(&view, "!", cx);
        cx.simulate_keystrokes("enter");
        let sent = sends(&host, cx);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "hi \\<@7> <@42> !");
        assert_eq!(sent[0].mentions.users, [42]);
        assert!(sent[0].mentions.roles.is_empty() && !sent[0].mentions.everyone);
    }

    #[gpui::test]
    fn slowmode_holds_the_next_message_back(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        view.update_in(cx, |v, window, cx| {
            v.set_channel(
                ChannelContext {
                    channel_id: "5".into(),
                    guild_id: Some("100".into()),
                    placeholder: String::new(),
                    slowmode_secs: 60,
                },
                window,
                cx,
            )
        });
        type_text(&view, "one", cx);
        cx.simulate_keystrokes("enter");
        type_text(&view, "two", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            sends(&host, cx).len(),
            1,
            "the second waits for the countdown"
        );
        let left = view.read_with(cx, |v, _| v.slowmode_left());
        assert!(left > 55 && left <= 60);
        let text = view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned());
        assert_eq!(text, "two", "the text stays");
        // At zero, sending works again.
        view.update(cx, |v, _| v.skew_ms += 61_000);
        cx.simulate_keystrokes("enter");
        assert_eq!(sends(&host, cx).len(), 2, "the countdown is over");
    }

    fn typings(host: &Entity<Host>, cx: &mut VisualTestContext) -> usize {
        host.read_with(cx, |h, _| {
            h.events
                .iter()
                .filter(|e| matches!(e, ComposerEvent::Typing { .. }))
                .count()
        })
    }

    #[gpui::test]
    fn opening_a_channel_with_a_draft_is_not_typing(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        open(&view, "1", cx);
        type_text(&view, "half a thought", cx);
        assert_eq!(typings(&host, cx), 1, "the person typed");
        open(&view, "2", cx);
        open(&view, "1", cx);
        let text = view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned());
        assert_eq!(text, "half a thought", "the draft is back");
        assert_eq!(typings(&host, cx), 1, "restoring the draft sent no typing");
        // A real edit after that counts again (once the one-second spacing has passed).
        view.update(cx, |v, _| v.last_typing = None);
        type_text(&view, "!", cx);
        assert_eq!(typings(&host, cx), 2);
    }

    #[gpui::test]
    fn a_failed_direct_message_says_so_and_keeps_the_text(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        open(&view, "1", cx);
        view.update(cx, |v, cx| {
            v.on_members(
                "100".into(),
                vec![Person {
                    id: 42,
                    name: "ada".into(),
                    username: "ada".into(),
                }],
                cx,
            )
        });
        type_text(&view, "/msg @ad", cx);
        cx.simulate_keystrokes("tab");
        type_text(&view, "the meeting moved", cx);
        cx.simulate_keystrokes("enter");
        let dm = host.read_with(cx, |h, _| {
            h.events.iter().find_map(|e| match e {
                ComposerEvent::DirectMessage { user_id, message } => {
                    Some((user_id.clone(), message.clone()))
                }
                _ => None,
            })
        });
        let (user_id, message) = dm.expect("a direct message was asked for");
        assert_eq!(user_id, "42");
        assert_eq!(message.content, "the meeting moved");
        let text = |cx: &mut VisualTestContext| {
            view.read_with(cx, |v, cx| v.editor.read(cx).buffer().text().to_owned())
        };
        assert_eq!(text(cx), "");
        // The server refused it: the reason shows and the text comes back with its pill.
        view.update_in(cx, |v, window, cx| {
            v.on_direct_message_failed(&message.nonce, "blocked".into(), window, cx)
        });
        assert_eq!(text(cx), "/msg @ada the meeting moved");
        let error = view.read_with(cx, |v, _| v.error.clone());
        assert!(error.is_some_and(|e| e.contains("blocked")));
    }

    #[gpui::test]
    fn a_refused_edit_stays_open_with_its_text(cx: &mut TestAppContext) {
        use super::super::edit_inline::tests::message;
        let (host, view, cx) = host(cx);
        open(&view, "301", cx);
        let (account, ch) = account(serde_json::json!([message(4, "10", "typo")]), cx);
        cx.update(|window, cx| crate::ui::composer::edit_last(&view, &account, ch, window, cx));
        let editor = view
            .read_with(cx, |v, _| v.edit.as_ref().map(|e| e.editor.clone()))
            .expect("an edit is open");
        editor.update_in(cx, |e, window, cx| {
            e.replace_text_in_range(None, "s", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(edits(&host, cx).len(), 1);
        // Escape while the server has not answered keeps the text, and never marks the
        // channel read.
        cx.simulate_keystrokes("escape");
        assert!(view.read_with(cx, |v, _| v.edit.is_some()));
        assert_eq!(host.read_with(cx, |h, _| h.marked_read), 0);
        let id = (4u64 << 22).to_string();
        view.update(cx, |v, cx| v.on_edit_failed(&id, "too fast", cx));
        let (open_edit, text, error) = view.read_with(cx, |v, cx| {
            (
                v.edit.as_ref().is_some_and(|e| !e.saving),
                v.edit
                    .as_ref()
                    .map(|e| e.editor.read(cx).buffer().text().to_owned()),
                v.error.clone(),
            )
        });
        assert!(open_edit, "the edit is open again for another try");
        assert_eq!(text.as_deref(), Some("typos"));
        assert!(error.is_some_and(|e| e.contains("too fast")));
        // Enter sends it again.
        cx.simulate_keystrokes("enter");
        assert_eq!(edits(&host, cx).len(), 2);
    }

    #[gpui::test]
    fn text_typed_while_an_edit_saves_is_kept(cx: &mut TestAppContext) {
        use super::super::edit_inline::tests::message;
        let (host, view, cx) = host(cx);
        open(&view, "301", cx);
        let (account, ch) = account(serde_json::json!([message(4, "10", "typo")]), cx);
        cx.update(|window, cx| crate::ui::composer::edit_last(&view, &account, ch, window, cx));
        let editor = view
            .read_with(cx, |v, _| v.edit.as_ref().map(|e| e.editor.clone()))
            .expect("an edit is open");
        editor.update_in(cx, |e, window, cx| {
            e.replace_text_in_range(None, "s", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(edits(&host, cx).len(), 1);
        // More text while the server has not answered yet.
        editor.update_in(cx, |e, window, cx| {
            e.replace_text_in_range(None, "!", window, cx)
        });
        let id = (4u64 << 22).to_string();
        view.update_in(cx, |v, window, cx| v.on_edited(&id, window, cx));
        let (open_edit, text) = view.read_with(cx, |v, cx| {
            (
                v.edit.as_ref().is_some_and(|e| !e.saving),
                v.edit
                    .as_ref()
                    .map(|e| e.editor.read(cx).buffer().text().to_owned()),
            )
        });
        assert!(open_edit, "the box stays open for the newer text");
        assert_eq!(text.as_deref(), Some("typos!"));
        cx.simulate_keystrokes("enter");
        let sent = edits(&host, cx);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].1, "typos!");
        view.update_in(cx, |v, window, cx| v.on_edited(&id, window, cx));
        assert!(view.read_with(cx, |v, _| v.edit.is_none()));
    }

    #[gpui::test]
    fn a_refused_edit_of_a_closed_box_is_not_shown_in_another_channel(cx: &mut TestAppContext) {
        use super::super::edit_inline::tests::message;
        let (_host, view, cx) = host(cx);
        open(&view, "301", cx);
        let (account, ch) = account(serde_json::json!([message(4, "10", "typo")]), cx);
        cx.update(|window, cx| crate::ui::composer::edit_last(&view, &account, ch, window, cx));
        let editor = view
            .read_with(cx, |v, _| v.edit.as_ref().map(|e| e.editor.clone()))
            .expect("an edit is open");
        editor.update_in(cx, |e, window, cx| {
            e.replace_text_in_range(None, "s", window, cx)
        });
        cx.simulate_keystrokes("enter");
        // The person moves on to another channel; the edit box closes there.
        open(&view, "302", cx);
        let id = (4u64 << 22).to_string();
        let shown = view.update(cx, |v, cx| v.on_edit_failed(&id, "too fast", cx));
        assert!(!shown, "the app says it in the notice bar instead");
        assert!(view.read_with(cx, |v, _| v.error.is_none()));
    }

    #[gpui::test]
    fn escape_cancels_a_reply_before_it_marks_the_channel_read(cx: &mut TestAppContext) {
        let (host, view, cx) = host(cx);
        open(&view, "301", cx);
        view.update_in(cx, |v, window, cx| {
            v.start_reply("55".into(), "ada".into(), window, cx)
        });
        cx.simulate_keystrokes("escape");
        assert!(
            view.read_with(cx, |v, _| v.reply.is_none()),
            "the reply closes"
        );
        assert_eq!(host.read_with(cx, |h, _| h.marked_read), 0);
        // Nothing left to cancel: Escape goes on to the app.
        cx.simulate_keystrokes("escape");
        assert_eq!(host.read_with(cx, |h, _| h.marked_read), 1);
    }
}
