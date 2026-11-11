// SPDX-License-Identifier: AGPL-3.0-or-later
//! The emoji, GIF, sticker and meme pickers: one popover with a tab per kind, opened
//! from the buttons at the right of the composer, with Ctrl+E (emoji), Ctrl+G (GIFs) and
//! Ctrl+S (stickers) (Cmd on macOS), and from "Add reaction" (emoji only).
//!
//! - **A layer.** The picker is one of the layers over the chat (`desktop::OpenLayers`):
//!   while it is the topmost one its search field has the keyboard, so typing never
//!   reaches the composer behind it, and Escape closes it first. A click outside closes
//!   it too. Animations in it move while the chat under it stands still (the one
//!   animation budget, see `ui/media/animation_budget.rs`).
//! - **Per account.** Everything here (the preferences, the instance's GIF setup, the
//!   memes, the GIF answers) belongs to the account it was loaded for and is dropped
//!   when another account is shown.
//! - **Picking.** In the composer an emoji goes in at the caret (a Unicode emoji as text,
//!   a custom emoji as a pill), a GIF as its link, the way Fluxer sends it; a sticker or a
//!   meme is sent at once as a message of its own. Shift keeps the picker open. In a
//!   reaction picker the emoji reacts. A right click makes an emoji or a GIF a
//!   favourite, or no longer one.
//! - **Locked emoji.** Custom emoji (and stickers) of another community are shown but
//!   cannot be used unless the account may: Fluxer's rule needs the right to use
//!   external emoji in the channel and the instance's `feature_global_expressions`.

// Deviation: Fluxer's picker has a context menu per item (favourite, copy, info), a
// category sidebar and a skin tone popover; Inlet makes an item a favourite with a right
// click, jumps between sections with a bar under the grid, and steps through the skin
// tones with one button. A GIF always goes into the composer as its link (Fluxer's
// optional "send GIFs at once" setting is not read yet), and a meme is always sent at
// once by its ID (Fluxer puts its link into the composer unless that setting is on).

mod emoji;
mod gif;
mod meme;
pub mod prefs;
mod sticker;

use std::time::{Duration, Instant};

use gpui::{
    actions, anchored, deferred, div, point, prelude::*, px, AnyElement, App, Context, Entity,
    FocusHandle, Focusable, FontWeight, KeyBinding, MouseDownEvent, Pixels, Point, SharedString,
    Svg, UniformListScrollHandle, Window,
};
use inlet_protocol as p;
use inlet_state::{ChannelId, GuildId, MessageId};

use super::editor::ComposerView;
use crate::app::{Inlet, Screen};
use crate::backend::accounts::AccountKey;
use crate::backend::composer::gif::{Cmd, Ev, PrefEdit, Setup};
use crate::backend::composer::Cmd as ComposerCmd;
use crate::desktop::Layer;
use crate::text_input::TextInput;
use crate::theme;

#[cfg(test)]
pub(crate) use self::gif::SearchDebounce;
use self::prefs::PickerPrefs;

actions!(
    inlet_pickers,
    [ToggleEmojiPicker, ToggleGifPicker, ToggleStickerPicker]
);

/// The picker's size, in logical pixels.
pub const PICKER_WIDTH: f32 = 424.;
pub const PICKER_HEIGHT: f32 = 460.;
/// Where the composer's picker sits: above the composer, at the right of the window.
const COMPOSER_RIGHT: f32 = 16.;
const COMPOSER_BOTTOM: f32 = 76.;
/// A click outside closes the picker; a toggle by the same button within this time is
/// that click, not a new one.
const REOPEN_GUARD: Duration = Duration::from_millis(300);
/// Longest search text.
const MAX_QUERY_CHARS: usize = 100;

/// Ctrl+E, Ctrl+G and Ctrl+S (Cmd on macOS), Fluxer's keys, for the whole window: no
/// other binding uses them.
pub fn bind_keys(cx: &mut App) {
    // "secondary" needs an OS context in this gpui version, so pick the modifier here.
    let [e, g, s] = if cfg!(target_os = "macos") {
        ["cmd-e", "cmd-g", "cmd-s"]
    } else {
        ["ctrl-e", "ctrl-g", "ctrl-s"]
    };
    cx.bind_keys([
        KeyBinding::new(e, ToggleEmojiPicker, None),
        KeyBinding::new(g, ToggleGifPicker, None),
        KeyBinding::new(s, ToggleStickerPicker, None),
    ]);
}

/// The picker's tabs, in Fluxer's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Gifs,
    Memes,
    Stickers,
    Emoji,
}

/// What a pick goes to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// The main composer.
    Composer,
    /// A reaction on a message; the picker opens at `at`.
    Reaction {
        ch: ChannelId,
        id: MessageId,
        at: Point<Pixels>,
    },
}

/// The tabs a picker for `target` shows: a reaction picker only has emoji, and GIFs
/// only show when the instance has a GIF service (unknown counts as none).
pub fn visible_tabs(target: &Target, setup: Option<&Setup>) -> Vec<Tab> {
    match target {
        Target::Reaction { .. } => vec![Tab::Emoji],
        Target::Composer => {
            let gifs = setup.is_some_and(|s| s.gif_enabled);
            [Tab::Gifs, Tab::Memes, Tab::Stickers, Tab::Emoji]
                .into_iter()
                .filter(|t| *t != Tab::Gifs || gifs)
                .collect()
        }
    }
}

/// Whether a custom emoji or sticker of `owner` (a guild) can be used in a channel of
/// `channel_guild` (`None`: a direct message). Fluxer's rule: its own community's
/// always; another's only with the right to use external ones in the channel and the
/// instance's `feature_global_expressions` for the account.
pub fn usable(
    owner: &str,
    channel_guild: Option<&str>,
    external_right: bool,
    global: bool,
) -> bool {
    match channel_guild {
        Some(g) if g == owner => true,
        Some(_) => external_right && global,
        None => global,
    }
}

/// The open picker.
pub(crate) struct Open {
    pub tab: Tab,
    pub target: Target,
    search: Entity<TextInput>,
    /// The search text the picker last worked with.
    query: String,
    list: UniformListScrollHandle,
    /// The GIF tab's view.
    gif_view: gif::GifView,
}

/// The pickers' state, for the account in `account`.
#[derive(Default)]
pub struct PickersState {
    open: Option<Open>,
    /// The account everything here belongs to.
    account: Option<AccountKey>,
    pub(crate) prefs: PickerPrefs,
    setup: Option<Setup>,
    setup_asked: bool,
    gifs: gif::GifState,
    /// The saved memes; `None` until READY or a load brought them.
    memes: Option<Vec<p::Meme>>,
    memes_asked: bool,
    /// Closed by a click outside on this tab, at this time.
    closed_by_click: Option<(Tab, Instant)>,
    #[cfg(feature = "demo")]
    demo_done: bool,
}

impl PickersState {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn close(&mut self) {
        self.open = None;
        self.gifs.cancel_search();
    }

    /// The search field of the open picker, which has the keyboard while the picker
    /// is the topmost layer.
    pub fn search_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.open.as_ref().map(|o| o.search.focus_handle(cx))
    }

    /// Forgets everything of the account it was for.
    fn reset(&mut self, account: Option<AccountKey>) {
        *self = PickersState {
            account,
            ..PickersState::default()
        };
    }
}

/// An icon of the pickers (Phosphor, MIT).
#[derive(Clone, Copy)]
pub(crate) enum PickerIcon {
    Gif,
    Sticker,
    Smiley,
    Lock,
}

impl PickerIcon {
    pub fn el(self, size: f32, color: gpui::Hsla) -> Svg {
        let data: &'static [u8] = match self {
            PickerIcon::Gif => include_bytes!("../../../../assets/icons/gif.svg"),
            PickerIcon::Sticker => include_bytes!("../../../../assets/icons/sticker.svg"),
            PickerIcon::Smiley => include_bytes!("../../../../assets/icons/smiley.svg"),
            PickerIcon::Lock => include_bytes!("../../../../assets/icons/lock-simple.svg"),
        };
        gpui::svg()
            .data(data)
            .size(px(size))
            .flex_none()
            .text_color(color)
    }
}

/// What a pick puts into the composer.
pub(crate) enum Insert {
    Text(String),
    CustomEmoji {
        id: u64,
        name: String,
        animated: bool,
    },
}

/// The composer's picker buttons.
#[derive(Clone, Copy, Debug, Default)]
pub struct PickerButtons {
    /// The instance has GIFs.
    pub gif: bool,
}

impl ComposerView {
    /// Puts a pick into the composer at the caret. Nothing happens without an open
    /// channel, or while a message is edited inline (the pickers write into the box).
    pub(crate) fn insert_picked(
        &mut self,
        pick: Insert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.channel.is_none() || self.edit.is_some() {
            return;
        }
        self.editor.update(cx, |e, cx| match pick {
            Insert::Text(text) => {
                let _ = e.insert_text(&text, window, cx);
            }
            Insert::CustomEmoji { id, name, animated } => {
                let label = format!(":{name}:");
                let token = inlet_editor::Token::CustomEmoji {
                    id: inlet_editor::Id(id),
                    name,
                    animated,
                };
                e.insert_token(token, &label, window, cx);
            }
        });
        cx.notify();
    }

    /// The buttons at the right of the composer box (GIF, sticker, emoji), 32 px each with
    /// 22 px icons. Only the main composer has them.
    pub(super) fn render_picker_buttons(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let buttons = self.pickers?;
        let button = |id: &'static str, icon: PickerIcon, action: Box<dyn gpui::Action>| {
            div()
                .id(id)
                .size(px(32.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()))
                .child(icon.el(22., theme::text_tertiary()))
                .on_click(cx.listener(move |_, _, window, cx| {
                    window.dispatch_action(action.boxed_clone(), cx);
                }))
        };
        Some(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .ml(px(8.))
                .my(px(-5.))
                .when(buttons.gif, |d| {
                    d.child(button(
                        "composer-gif-button",
                        PickerIcon::Gif,
                        Box::new(ToggleGifPicker),
                    ))
                })
                .child(button(
                    "composer-sticker-button",
                    PickerIcon::Sticker,
                    Box::new(ToggleStickerPicker),
                ))
                .child(button(
                    "composer-emoji-button",
                    PickerIcon::Smiley,
                    Box::new(ToggleEmojiPicker),
                ))
                .into_any_element(),
        )
    }
}

impl Inlet {
    /// The picker's keys on the window's root element.
    pub(crate) fn picker_keys<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        element
            .on_action(cx.listener(|this, _: &ToggleEmojiPicker, window, cx| {
                this.toggle_picker(Tab::Emoji, Target::Composer, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleGifPicker, window, cx| {
                this.toggle_picker(Tab::Gifs, Target::Composer, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleStickerPicker, window, cx| {
                this.toggle_picker(Tab::Stickers, Target::Composer, window, cx)
            }))
    }

    /// Drops the pickers' state when another account is shown (or none).
    fn pickers_follow_account(&mut self, cx: &mut Context<Self>) {
        if self.composer.pickers.account != self.switcher.active {
            let account = self.switcher.active.clone();
            self.composer.pickers.reset(account);
            self.set_picker_buttons(cx);
        }
    }

    fn set_picker_buttons(&mut self, cx: &mut Context<Self>) {
        let gif = self
            .composer
            .pickers
            .setup
            .as_ref()
            .is_some_and(|s| s.gif_enabled);
        self.composer.view.update(cx, |v, cx| {
            if let Some(b) = &mut v.pickers {
                if b.gif != gif {
                    b.gif = gif;
                    cx.notify();
                }
            }
        });
    }

    /// "Add reaction" on a message: the emoji picker at `at`.
    pub(crate) fn open_reaction_picker(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_picker(Tab::Emoji, Target::Reaction { ch, id, at }, window, cx);
    }

    /// Opens the picker on `tab` for `target`, switches the open one to `tab`, or closes
    /// it when it already shows `tab` for the same target.
    pub(crate) fn toggle_picker(
        &mut self,
        tab: Tab,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        self.pickers_follow_account(cx);
        let pickers = &mut self.composer.pickers;
        if let Some(open) = &mut pickers.open {
            let same_target = match (&open.target, &target) {
                (Target::Composer, Target::Composer) => true,
                (Target::Reaction { ch: a, id: b, .. }, Target::Reaction { ch: c, id: d, .. }) => {
                    a == c && b == d
                }
                _ => false,
            };
            if same_target && open.tab == tab {
                pickers.close();
                cx.notify();
                return;
            }
            if same_target && visible_tabs(&target, pickers.setup.as_ref()).contains(&tab) {
                open.tab = tab;
                self.picker_tab_opened(tab, cx);
                cx.notify();
                return;
            }
        }
        // The click outside that closed it was this toggle's own (its button).
        if pickers
            .closed_by_click
            .take()
            .is_some_and(|(t, at)| t == tab && at.elapsed() < REOPEN_GUARD)
        {
            return;
        }
        // Only over the chat with a channel open, and not under another layer.
        let top = self.open_layers().topmost();
        let ready = self.screen == Screen::Main
            && self.account.is_some()
            && self.selected_channel.is_some()
            && matches!(top, None | Some(Layer::Picker));
        let shown = visible_tabs(&target, self.composer.pickers.setup.as_ref()).contains(&tab);
        if !ready || !shown {
            return;
        }
        self.open_picker(tab, target, cx);
    }

    fn open_picker(&mut self, tab: Tab, target: Target, cx: &mut Context<Self>) {
        let search =
            cx.new(|cx| TextInput::new(t!("picker-search"), cx).max_chars(MAX_QUERY_CHARS));
        self.composer.pickers.open = Some(Open {
            tab,
            target,
            search,
            query: String::new(),
            list: UniformListScrollHandle::new(),
            gif_view: gif::GifView::Home,
        });
        self.pickers_load(cx);
        self.picker_tab_opened(tab, cx);
        cx.notify();
    }

    /// Asks once per account for what every tab needs: the instance's GIF setup and the
    /// preferences, when READY did not bring them.
    fn pickers_load(&mut self, cx: &mut Context<Self>) {
        if !self.composer.pickers.setup_asked {
            self.composer.pickers.setup_asked = true;
            let traits = self
                .account
                .as_ref()
                .map(|a| a.read(cx).me().traits())
                .unwrap_or_default();
            self.backend
                .send(ComposerCmd::Pickers(Cmd::Setup { traits }));
        }
        let prefs = &mut self.composer.pickers.prefs;
        if !prefs.known() && !prefs.asked {
            prefs.asked = true;
            self.backend
                .send(ComposerCmd::Pickers(Cmd::LoadPreferences));
        }
    }

    /// A tab became visible: a fresh search field with the tab's hint, and what the tab
    /// needs is asked for once.
    fn picker_tab_opened(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let placeholder = match tab {
            Tab::Gifs => t!("picker-search-gifs"),
            Tab::Memes => t!("picker-search-memes"),
            Tab::Stickers => t!("picker-search-stickers"),
            Tab::Emoji => t!("picker-search"),
        };
        if let Some(open) = &mut self.composer.pickers.open {
            open.list = UniformListScrollHandle::new();
            open.search = cx.new(|cx| TextInput::new(placeholder, cx).max_chars(MAX_QUERY_CHARS));
            open.query.clear();
            open.gif_view = gif::GifView::Home;
        }
        self.composer.pickers.gifs.cancel_search();
        match tab {
            Tab::Memes => {
                let pickers = &mut self.composer.pickers;
                if pickers.memes.is_none() && !pickers.memes_asked {
                    pickers.memes_asked = true;
                    self.backend.send(ComposerCmd::Pickers(Cmd::LoadMemes));
                }
            }
            Tab::Gifs => self.gif_home_needed(),
            Tab::Stickers | Tab::Emoji => {}
        }
    }

    /// Closes the picker after a pick, unless Shift kept it open.
    fn picked(&mut self, keep_open: bool, cx: &mut Context<Self>) {
        if !keep_open {
            self.composer.pickers.close();
        }
        cx.notify();
    }

    /// Sends a preference edit (shown at once).
    fn edit_prefs(&mut self, edit: PrefEdit) {
        let cmd = self.composer.pickers.prefs.edit(edit);
        self.backend.send(ComposerCmd::Pickers(cmd));
    }

    /// The open channel as the pickers see it: its guild, and whether the account may
    /// use external emoji and external stickers there.
    fn picker_channel(&self, cx: &App) -> (Option<String>, bool, bool) {
        let (Some(account), Some(ch)) = (self.account.as_ref(), self.open_channel_id()) else {
            return (None, false, false);
        };
        let state = account.read(cx);
        let Some(g) = state.channels().guild_of(ch) else {
            return (None, false, false);
        };
        let perms = state.permissions(g, ch);
        (
            Some(g.to_string()),
            perms.has(p::permissions::USE_EXTERNAL_EMOJIS),
            perms.has(p::permissions::USE_EXTERNAL_STICKERS),
        )
    }

    /// The backend's answers for the pickers.
    pub(crate) fn on_pickers_event(&mut self, ev: Ev, cx: &mut Context<Self>) {
        self.pickers_follow_account(cx);
        let pickers = &mut self.composer.pickers;
        match ev {
            Ev::Setup(setup) => {
                pickers.setup = Some(setup);
                // A GIF tab of an instance without GIFs is not shown.
                if let Some(open) = &mut pickers.open {
                    let tabs = visible_tabs(&open.target, pickers.setup.as_ref());
                    if !tabs.contains(&open.tab) {
                        open.tab = Tab::Emoji;
                    }
                }
                self.set_picker_buttons(cx);
            }
            Ev::Gifs { nonce, result } => pickers.gifs.answered_gifs(nonce, result),
            Ev::Categories { nonce, result } => pickers.gifs.answered_categories(nonce, result),
            Ev::Preferences(stored) => pickers.prefs.set_stored(*stored),
            Ev::Edited(stored) => pickers.prefs.edited(Some(*stored)),
            Ev::PreferencesFailed(reason) => {
                pickers.prefs.edited(None);
                self.chat.notice = Some(t!("picker-prefs-failed", error = reason));
            }
            Ev::Memes(memes) => pickers.memes = Some(memes),
            Ev::SendFailed(reason) => {
                self.chat.notice = Some(t!("picker-send-failed", error = reason));
            }
        }
        cx.notify();
    }

    /// Gateway events the pickers keep: the preferences and the saved memes of READY and
    /// later updates (only the shown account's events come here).
    pub(crate) fn pickers_on_dispatch(&mut self, d: &p::Dispatch, cx: &mut Context<Self>) {
        match d {
            p::Dispatch::Ready(ready) => {
                // The shown account's READY (also after a reconnect): its preferences and
                // memes as they are now.
                self.pickers_follow_account(cx);
                let pickers = &mut self.composer.pickers;
                if let Some(Ok(prefs)) = ready.user_settings().and_then(|s| s.synced_preferences())
                {
                    pickers.prefs.set_stored(prefs);
                }
                if ready.extra.contains_key("favorite_memes") {
                    pickers.memes = Some(ready.favorite_memes());
                }
                self.set_picker_buttons(cx);
            }
            p::Dispatch::UserSettingsUpdate(settings) => {
                if let Some(Ok(prefs)) = settings.synced_preferences() {
                    self.composer.pickers.prefs.set_stored(prefs);
                }
            }
            p::Dispatch::FavoriteMemeCreate(m) | p::Dispatch::FavoriteMemeUpdate(m) => {
                if let Some(memes) = &mut self.composer.pickers.memes {
                    memes.retain(|x| x.id != m.id);
                    if memes.len() < p::api::expressions::MAX_MEMES {
                        memes.push((**m).clone());
                    }
                }
            }
            p::Dispatch::FavoriteMemeDelete(gone) => {
                if let Some(memes) = &mut self.composer.pickers.memes {
                    memes.retain(|x| x.id != gone.meme_id);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    /// The open picker, for the window's overlays.
    pub(crate) fn render_picker(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.pickers_follow_account(cx);
        #[cfg(feature = "demo")]
        self.demo_pickers(cx);
        let (tab, target, query) = {
            let open = self.composer.pickers.open.as_ref()?;
            (open.tab, open.target, open.query.clone())
        };
        // The channel went away (signed out, another channel): nothing to pick for.
        let gone = match target {
            Target::Reaction { ch, .. } => self.open_channel_id() != Some(ch),
            Target::Composer => self.selected_channel.is_none(),
        };
        if self.screen != Screen::Main || gone {
            self.composer.pickers.close();
            return None;
        }
        let search = self.composer.pickers.open.as_ref()?.search.clone();
        let text = search.read(cx).text().to_string();
        if text != query {
            self.picker_query_changed(tab, text, cx);
        }
        let tabs = visible_tabs(&target, self.composer.pickers.setup.as_ref());
        let body = match tab {
            Tab::Emoji => self.render_emoji_tab(cx),
            Tab::Gifs => self.render_gif_tab(cx),
            Tab::Stickers => self.render_sticker_tab(cx),
            Tab::Memes => self.render_meme_tab(cx),
        };
        let header = (tabs.len() > 1).then(|| self.render_picker_tabs(&tabs, tab, target, cx));
        let tools = (tab == Tab::Emoji).then(|| self.render_skin_tone_button(cx));
        let footer = (tab == Tab::Gifs)
            .then(|| {
                self.composer
                    .pickers
                    .setup
                    .as_ref()?
                    .gif_attribution
                    .clone()
            })
            .flatten()
            .map(|name| {
                div()
                    .px(px(12.))
                    .pb(px(8.))
                    .text_size(px(11.))
                    .text_color(theme::text_tertiary())
                    .child(t!("picker-gif-powered-by", provider = name))
            });
        let frame = div()
            .id("picker")
            .occlude()
            .w(px(PICKER_WIDTH))
            .h(px(PICKER_HEIGHT))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .bg(theme::floating_bg())
            .border_1()
            .border_color(theme::accent_border())
            .shadow(vec![gpui::BoxShadow {
                color: theme::floating_shadow(),
                offset: point(px(0.), px(6.)),
                blur_radius: px(14.),
                spread_radius: px(0.),
                inset: false,
            }])
            .on_mouse_down_out(cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.composer.pickers.close();
                this.composer.pickers.closed_by_click = Some((tab, Instant::now()));
                cx.notify();
            }))
            .children(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(8.))
                    .child(
                        // The field draws its own box and focus ring.
                        div().flex_1().min_w_0().text_size(px(14.)).child(search),
                    )
                    .children(tools),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .children(footer);
        let frame = if self.is_top_layer(Layer::Picker) {
            self.layer_keys(frame, cx).into_any_element()
        } else {
            frame.into_any_element()
        };
        Some(match target {
            Target::Composer => div()
                .absolute()
                .right(px(COMPOSER_RIGHT))
                .bottom(px(COMPOSER_BOTTOM))
                .child(frame)
                .into_any_element(),
            Target::Reaction { at, .. } => deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.))
                    .child(frame),
            )
            .with_priority(1)
            .into_any_element(),
        })
    }

    fn render_picker_tabs(
        &self,
        tabs: &[Tab],
        selected: Tab,
        target: Target,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut row = div().flex().flex_row().gap(px(4.)).px(px(12.)).pt(px(10.));
        for &tab in tabs {
            let label = match tab {
                Tab::Gifs => t!("picker-tab-gifs"),
                Tab::Memes => t!("picker-tab-memes"),
                Tab::Stickers => t!("picker-tab-stickers"),
                Tab::Emoji => t!("picker-tab-emoji"),
            };
            let on = tab == selected;
            row = row.child(
                div()
                    .id(SharedString::from(format!("picker-tab-{tab:?}")))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(4.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| {
                        d.bg(theme::selected_bg()).text_color(theme::text_strong())
                    })
                    .when(!on, |d| {
                        d.text_color(theme::text_muted())
                            .hover(|s| s.bg(theme::floating_hover()))
                    })
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if tab != selected {
                            this.toggle_picker(tab, target, window, cx);
                        }
                    })),
            );
        }
        row.into_any_element()
    }

    /// The search text changed: the tab starts over at the top, and the GIF tab waits
    /// for the typing to stop before it searches.
    fn picker_query_changed(&mut self, tab: Tab, text: String, cx: &mut Context<Self>) {
        let Some(open) = &mut self.composer.pickers.open else {
            return;
        };
        open.query.clone_from(&text);
        open.list = UniformListScrollHandle::new();
        if tab == Tab::Gifs {
            self.gif_query_changed(text, cx);
        }
    }

    /// A pick for the composer: into the main composer at its caret.
    fn insert_into_composer(&mut self, pick: Insert, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .view
            .update(cx, |v, cx| v.insert_picked(pick, window, cx));
    }

    /// Sends a sticker or meme to the open channel.
    fn send_to_channel(&mut self, cmd: impl FnOnce(String, String) -> Cmd) {
        let Some(channel_id) = self.selected_channel.clone() else {
            return;
        };
        self.backend
            .send(ComposerCmd::Pickers(cmd(channel_id, p::new_nonce())));
    }

    /// Guild IDs in picker order: the open guild first, then the account's other guilds.
    fn picker_guild_order(&self, cx: &App) -> Vec<GuildId> {
        let Some(account) = self.account.as_ref() else {
            return Vec::new();
        };
        let current = self.selected_guild.as_deref().and_then(GuildId::parse);
        let state = account.read(cx);
        let mut ids: Vec<GuildId> = current.into_iter().collect();
        ids.extend(state.guilds().map(|g| g.id).filter(|g| Some(*g) != current));
        ids
    }

    /// Demo scenes `picker-emoji` and `picker-gif`: the picker opens on its tab once the
    /// channel is open (the GIF scene waits for the instance's GIF setup).
    #[cfg(feature = "demo")]
    fn demo_pickers(&mut self, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        let tab = match current() {
            Scene::PickerEmoji => Tab::Emoji,
            Scene::PickerGif => Tab::Gifs,
            _ => return,
        };
        if self.composer.pickers.demo_done
            || self.account.is_none()
            || self.selected_channel.is_none()
        {
            return;
        }
        self.pickers_load(cx);
        if tab == Tab::Gifs && self.composer.pickers.setup.is_none() {
            return;
        }
        self.composer.pickers.demo_done = true;
        self.open_picker(tab, Target::Composer, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(gif_enabled: bool) -> Setup {
        Setup {
            gif_enabled,
            ..Setup::default()
        }
    }

    #[test]
    fn picker_hides_gif_when_the_instance_disables_it() {
        let composer = Target::Composer;
        assert_eq!(
            visible_tabs(&composer, Some(&setup(true))),
            [Tab::Gifs, Tab::Memes, Tab::Stickers, Tab::Emoji]
        );
        for off in [Some(&setup(false)), None] {
            let tabs = visible_tabs(&composer, off);
            assert_eq!(tabs, [Tab::Memes, Tab::Stickers, Tab::Emoji]);
        }
        let reaction = Target::Reaction {
            ch: ChannelId::parse("1").unwrap(),
            id: MessageId::parse("2").unwrap(),
            at: point(px(0.), px(0.)),
        };
        assert_eq!(visible_tabs(&reaction, Some(&setup(true))), [Tab::Emoji]);
        // The setup comes from the discovery document: no `gif_enabled`, no GIFs.
        let discovery: p::Discovery = serde_json::from_value(serde_json::json!({
            "api_code_version": 1,
            "endpoints": {"api_public": "https://a.example/api", "gateway": "wss://g.example"},
            "features": {"gif_enabled": false}
        }))
        .unwrap();
        let s = Setup::of(&discovery, &[]);
        assert_eq!(
            visible_tabs(&composer, Some(&s)),
            [Tab::Memes, Tab::Stickers, Tab::Emoji]
        );
    }

    #[test]
    fn custom_emoji_of_other_communities_are_locked_as_in_fluxer() {
        // Its own community: always.
        assert!(usable("1", Some("1"), false, false));
        // Another community: the right in the channel and the global feature.
        assert!(!usable("2", Some("1"), true, false));
        assert!(!usable("2", Some("1"), false, true));
        assert!(usable("2", Some("1"), true, true));
        // A direct message: the global feature.
        assert!(!usable("2", None, true, false));
        assert!(usable("2", None, false, true));
    }
}
