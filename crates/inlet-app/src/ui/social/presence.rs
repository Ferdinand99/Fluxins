// SPDX-License-Identifier: AGPL-3.0-or-later
//! Presence in the UI: the status this session shows (the chosen one, or idle while the
//! person is away), the status dots, and the watch on the system's idle time.
//!
//! The shown status goes to the backend only when it changes (see
//! `backend/social/presence` for the budget). Away means no keyboard or mouse input on
//! the whole system for [`IDLE_AFTER`], as in Fluxer's desktop app; the first input
//! after that ends it. The idle time is read off the UI thread, every
//! [`POLL_ACTIVE`], and every [`POLL_IDLE`] while away, so input brings the status back
//! within a moment.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, img, prelude::*, px, svg, AnyElement, Context, Hsla, Image, ImageFormat, Task};
use inlet_protocol::models::CustomStatus;
use inlet_state::presence::normalize;

use crate::app::Inlet;
use crate::backend::social::presence::Cmd;
use crate::theme;

/// No input for this long counts as away (Fluxer's `IDLE_DURATION_MS`, 10 minutes).
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
/// How often the idle time is read while the person is there.
pub const POLL_ACTIVE: Duration = Duration::from_secs(30);
/// How often it is read while away, so input ends it quickly.
pub const POLL_IDLE: Duration = Duration::from_secs(2);
/// Size of a status dot (Fluxer's `StatusIndicator` default).
pub const DOT: f32 = 12.;

/// Whether the person is away, from the system's idle time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleTracker {
    idle: bool,
}

impl IdleTracker {
    /// The system's idle time was read (`None`: the system cannot tell, so never away).
    /// Returns whether away or back changed.
    pub fn observe(&mut self, system_idle: Option<Duration>) -> bool {
        let idle = system_idle.is_some_and(|d| d >= IDLE_AFTER);
        std::mem::replace(&mut self.idle, idle) != idle
    }

    /// Input in the window: back at once. Returns whether that changed anything.
    pub fn input(&mut self) -> bool {
        std::mem::replace(&mut self.idle, false)
    }

    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// When to read the idle time again.
    pub fn poll_every(&self) -> Duration {
        if self.idle {
            POLL_IDLE
        } else {
            POLL_ACTIVE
        }
    }
}

/// The status this session shows and whether it is away (`afk`): an `online` account
/// shows `idle` while away; `idle`, `dnd` and `invisible` stay as chosen.
pub fn shown(chosen: &str, idle: bool) -> (&'static str, bool) {
    let status = match normalize(chosen) {
        "online" if idle => "idle",
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "invisible",
    };
    (status, idle)
}

/// The domain's view state.
#[derive(Default)]
pub struct PresenceState {
    pub idle: IdleTracker,
    /// The presence last sent, with the session it was sent for.
    sent: Option<(String, &'static str, bool)>,
    /// The own status before the last change shown ahead of the server, to show it
    /// again when the save fails.
    pub before_save: Option<(String, Option<CustomStatus>)>,
    watch: Option<Task<()>>,
}

impl PresenceState {
    /// Starts the idle watch (not in unit tests, which have no system to ask).
    pub fn start(&mut self, cx: &mut Context<Inlet>) {
        if cfg!(test) {
            return;
        }
        self.watch = Some(cx.spawn(async move |this, cx| loop {
            let Ok(every) = this.read_with(cx, |this, _| this.social.presence.idle.poll_every())
            else {
                break;
            };
            cx.background_executor().timer(every).await;
            let idle = cx
                .background_executor()
                .spawn(async { inlet_platform::idle_time() })
                .await;
            let alive = this.update(cx, |this, cx| {
                if this.social.presence.idle.observe(idle) {
                    this.sync_presence(cx);
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }));
    }

    /// Forgets what was sent (sign-out): the next session sends its presence again.
    pub fn reset(&mut self) {
        self.sent = None;
        self.before_save = None;
    }
}

impl Inlet {
    /// The account's chosen status (`online` before READY).
    pub(crate) fn own_status(&self, cx: &gpui::App) -> &'static str {
        match self
            .account
            .as_ref()
            .map(|a| a.read(cx).presences().own().status())
        {
            Some("idle") => "idle",
            Some("dnd") => "dnd",
            Some("invisible") => "invisible",
            _ => "online",
        }
    }

    /// The status the account shows here: the chosen one, or idle while away.
    pub(crate) fn shown_status(&self, cx: &gpui::App) -> &'static str {
        shown(self.own_status(cx), self.social.presence.idle.is_idle()).0
    }

    /// Sends this session's presence when it changed: after a pick, away or back, a new
    /// READY or an account switch. Cheap to call often.
    pub(crate) fn sync_presence(&mut self, cx: &gpui::App) {
        let Some(account) = self.account.as_ref() else {
            return;
        };
        let session = account.read(cx).session_id().to_owned();
        let (status, afk) = shown(self.own_status(cx), self.social.presence.idle.is_idle());
        let next = (session, status, afk);
        if self.social.presence.sent.as_ref() == Some(&next) {
            return;
        }
        self.backend.send(Cmd::Presence {
            status: status.to_owned(),
            afk,
        });
        self.social.presence.sent = Some(next);
    }
}

/// The colour of a status (invisible and unknown read as offline).
pub fn status_color(status: &str) -> Hsla {
    match status {
        "online" => theme::status_online(),
        "idle" => theme::status_idle(),
        "dnd" => theme::status_dnd(),
        _ => theme::status_offline(),
    }
}

/// A status dot, `size` square.
pub fn status_dot(status: &str, size: f32) -> impl IntoElement {
    svg()
        .data(crate::assets::status_svg(status))
        .size(px(size))
        .flex_none()
        .text_color(status_color(status))
}

/// A status dot on the bottom-right of an avatar, in a ring of `ring` (the colour
/// behind the avatar), as Fluxer cuts it out of the avatar.
pub fn avatar_status(status: &str, ring: Hsla) -> AnyElement {
    let pad = 3.;
    div()
        .absolute()
        .right(px(-pad))
        .bottom(px(-pad))
        .size(px(DOT + 2. * pad))
        .rounded_full()
        .bg(ring)
        .flex()
        .items_center()
        .justify_center()
        .child(status_dot(status, DOT))
        .into_any_element()
}

/// The label of a status.
pub fn status_label(status: &str) -> String {
    match status {
        "online" => t!("presence-online"),
        "idle" => t!("presence-idle"),
        "dnd" => t!("presence-dnd"),
        "invisible" => t!("presence-invisible"),
        _ => t!("presence-offline"),
    }
}

/// Twemoji images of status emoji, drawn from the bundled drawings only. Bounded: the
/// picker's few emoji and the ones of custom statuses seen; past the cap an emoji shows
/// as text.
const MAX_EMOJI_IMAGES: usize = 32;

thread_local! {
    static EMOJI: std::cell::RefCell<HashMap<String, Option<Arc<Image>>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// A unicode emoji, `size` square: its Twemoji drawing, or the glyph as text when there
/// is none (or the cache is full).
pub fn emoji(glyph: &str, size: f32) -> AnyElement {
    let image = EMOJI.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(found) = cache.get(glyph) {
            return found.clone();
        }
        if cache.len() >= MAX_EMOJI_IMAGES {
            return None;
        }
        let made = inlet_emoji::twemoji_svg(glyph).map(|svg| {
            Arc::new(Image::from_bytes(
                ImageFormat::Svg,
                crate::assets::sized_svg(&svg, 64),
            ))
        });
        cache.insert(glyph.to_owned(), made.clone());
        made
    });
    match image {
        Some(image) => img(image).size(px(size)).flex_none().into_any_element(),
        None => div()
            .flex_none()
            .text_size(px(size * 0.85))
            .child(glyph.to_owned())
            .into_any_element(),
    }
}

/// A custom status in one line: its emoji and its text (cleaned, shortened by the
/// caller's layout).
pub fn custom_status_line(c: &CustomStatus, text_size: f32) -> AnyElement {
    let emoji_el = c
        .emoji_name
        .as_deref()
        .filter(|e| !e.is_empty() && c.emoji_id.is_none())
        .map(|e| emoji(e, text_size + 3.));
    let text = c
        .text
        .as_deref()
        .map(crate::model::clean_name)
        .filter(|t| !t.is_empty());
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .overflow_hidden()
        .children(emoji_el)
        .children(text.map(|t| div().min_w_0().text_ellipsis().child(t)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_switches_back_on_input() {
        let mut idle = IdleTracker::default();
        assert!(!idle.observe(Some(Duration::from_secs(60))));
        assert_eq!(idle.poll_every(), POLL_ACTIVE);
        // Ten minutes without input: away, and an online account shows idle.
        assert!(idle.observe(Some(IDLE_AFTER)));
        assert!(idle.is_idle());
        assert_eq!(shown("online", idle.is_idle()), ("idle", true));
        assert_eq!(idle.poll_every(), POLL_IDLE);
        // Still away: nothing to send.
        assert!(!idle.observe(Some(IDLE_AFTER * 2)));
        // A key press or a mouse move anywhere resets the system's idle time: back.
        assert!(idle.observe(Some(Duration::from_millis(400))));
        assert!(!idle.is_idle());
        assert_eq!(shown("online", idle.is_idle()), ("online", false));
        // Input in the window ends it at once, before the next read.
        idle.observe(Some(IDLE_AFTER));
        assert!(idle.input());
        assert!(!idle.is_idle());
        assert!(!idle.input(), "nothing changes when not away");
        // A system that cannot tell is never away.
        assert!(!idle.observe(None));
        assert!(!idle.is_idle());
    }

    #[test]
    fn only_online_turns_idle_while_away() {
        assert_eq!(shown("dnd", true), ("dnd", true));
        assert_eq!(shown("invisible", true), ("invisible", true));
        assert_eq!(shown("idle", false), ("idle", false));
        assert_eq!(shown("offline", false), ("invisible", false));
        assert_eq!(shown("", false), ("online", false));
    }
}
