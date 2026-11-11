// SPDX-License-Identifier: AGPL-3.0-or-later
//! The one budget for animated images (GIFs, animated attachments and embeds, animated
//! avatars, custom emoji and stickers), and the rules for when they move:
//!
//! - **Memory.** Animations keep every frame decoded, so the ones a frame draws share
//!   [`ANIMATION_BUDGET_BYTES`]; one that does not fit shows its still (see
//!   `Inlet::moving`). Each is capped on its own by the client (`ANIMATION_MAX_BYTES`).
//! - **Count.** At most [`MAX_PLAYING`] animations move at once on screen; the others
//!   show their first frame.
//! - **Focus.** Everything stops on its current frame while the window does not have
//!   the focus, and goes on from there when it gets it back.
//! - **Settings.** With animated emoji (or stickers) switched off they show their first
//!   frame, and only the still is fetched ([`MotionPrefs`], kept in `settings.json`).
//!
//! The views draw an animation with [`animated`]: it picks the frame from a
//! [`Playback`] per image, counts it, and asks for a redraw when the next frame is due
//! (not on every display refresh). Off-screen animations are neither counted nor drawn.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    canvas, prelude::*, px, AnyElement, App, Bounds, Corners, ImageId, ObjectFit, Pixels,
    RenderImage, Task, Window,
};
use inlet_media::{MediaRequest, Playback};
use serde::{Deserialize, Serialize};

use crate::backend::platform::prefs;

/// Decoded bytes the animations drawn in one frame may hold together (the GPU copies
/// of the frames they have shown come on top).
pub const ANIMATION_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// Animations that move at once; the others on screen show their first frame.
pub const MAX_PLAYING: usize = 32;
/// Animations whose decoded size is remembered, so one over the budget is not loaded
/// again every frame.
const MAX_KNOWN_ANIMATIONS: usize = 512;
/// Shortest wait before a redraw for the next frame (50 frames a second at most, as
/// browsers clamp GIF delays).
const MIN_FRAME_GAP: Duration = Duration::from_millis(20);
/// Frames due this soon are shown now, so animations whose frames change at nearly
/// the same time share one redraw.
const LOOKAHEAD: Duration = Duration::from_millis(8);
/// The key of the media settings in `settings.json`.
const SETTINGS_KEY: &str = "media";

/// What an animation is, for the settings that switch kinds of animation off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// GIFs and other animated attachments and embeds, and the image viewer.
    Gif,
    /// Animated avatars (only while the pointer is over the message).
    Avatar,
    /// Custom emoji, in text and in reactions.
    Emoji,
    Sticker,
}

// Deviation: with animated emoji (or stickers) off, Fluxer still plays one while the
// pointer is over it; here it always shows its first frame and only the still is
// fetched. A 33rd animation on screen shows its first frame (Fluxer has no such cap).
/// The media settings, under `"media"` in `settings.json`. As in Fluxer, animated
/// emoji and stickers move by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionPrefs {
    /// Animated custom emoji move; off shows their first frame.
    pub animate_emoji: bool,
    /// Animated stickers move; off shows their first frame.
    pub animate_stickers: bool,
}

impl Default for MotionPrefs {
    fn default() -> Self {
        MotionPrefs {
            animate_emoji: true,
            animate_stickers: true,
        }
    }
}

impl MotionPrefs {
    /// Whether animations of this kind may move.
    pub fn allows(&self, motion: Motion) -> bool {
        match motion {
            Motion::Gif | Motion::Avatar => true,
            Motion::Emoji => self.animate_emoji,
            Motion::Sticker => self.animate_stickers,
        }
    }

    /// The settings in `path`; defaults for a missing or broken file.
    pub fn load(path: &Path) -> Self {
        prefs::load_section(path, SETTINGS_KEY)
    }

    /// Writes the settings into `path`, keeping the file's other keys. For the
    /// settings tab, which comes later.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        prefs::save_section(path, SETTINGS_KEY, self)
    }

    /// The settings of this installation. Demo runs read nothing and use the defaults,
    /// except that `INLET_DEMO_ANIMATE_EMOJI=0` switches animated emoji off.
    pub fn current() -> Self {
        let demo = cfg!(feature = "demo") && std::env::var_os("INLET_DEMO").is_some();
        if demo {
            return MotionPrefs {
                animate_emoji: std::env::var("INLET_DEMO_ANIMATE_EMOJI").as_deref() != Ok("0"),
                ..Self::default()
            };
        }
        match prefs::settings_path() {
            Some(path) => Self::load(&path),
            None => Self::default(),
        }
    }
}

struct Player {
    playback: Playback,
    /// The frame that last drew it.
    used: u64,
}

/// The animations of one frame: their bytes against [`ANIMATION_BUDGET_BYTES`], how
/// many move, and which frame each shows.
pub struct AnimationBudget {
    budget: u64,
    used: u64,
    /// Decoded sizes of animations seen, by request.
    known: HashMap<MediaRequest, u64>,
    max_playing: usize,
    /// Animations that moved this frame.
    playing: usize,
    /// The window has the focus.
    active: bool,
    frame: u64,
    /// Every animation counts its frames from this time (the first frame of the
    /// session), so animations with the same frame lengths change frames together and
    /// share their redraws.
    epoch: Option<Instant>,
    /// Where each image drawn last frame is in its animation. Images that show the
    /// same animation share one (they move together).
    players: HashMap<ImageId, Player>,
    /// When the earliest next frame of this frame's animations is due.
    wake: Option<Instant>,
    /// The redraw asked for, and when it comes.
    scheduled: Option<(Instant, Task<()>)>,
    pub prefs: MotionPrefs,
}

impl Default for AnimationBudget {
    fn default() -> Self {
        Self::new(ANIMATION_BUDGET_BYTES)
    }
}

impl AnimationBudget {
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            used: 0,
            known: HashMap::new(),
            max_playing: MAX_PLAYING,
            playing: 0,
            active: true,
            frame: 0,
            epoch: None,
            players: HashMap::new(),
            wake: None,
            scheduled: None,
            prefs: MotionPrefs::default(),
        }
    }

    /// Forgets the last session's animations (sizes by URL, players, the redraw asked
    /// for), on sign-out or an account switch. The settings stay.
    pub fn reset(&mut self) {
        let prefs = self.prefs;
        *self = Self::new(self.budget);
        self.prefs = prefs;
    }

    /// A new frame: nothing plays yet. `active` is whether the window has the focus;
    /// losing it stops every animation where it is, getting it back goes on from there.
    pub fn next_frame(&mut self, active: bool, now: Instant) {
        self.used = 0;
        self.playing = 0;
        self.wake = None;
        self.frame += 1;
        self.epoch.get_or_insert(now);
        if active != self.active {
            self.active = active;
            for p in self.players.values_mut() {
                if active {
                    p.playback.resume(now);
                } else {
                    p.playback.pause(now);
                }
            }
        }
        let frame = self.frame;
        self.players
            .retain(|_, p| p.used.saturating_add(1) >= frame);
    }

    /// Whether an animation of `bytes` still fits this frame.
    pub fn fits(&self, bytes: u64) -> bool {
        self.used.saturating_add(bytes) <= self.budget
    }

    /// Counts a playing animation of `bytes` if it fits; `false` when it does not.
    pub fn admit(&mut self, bytes: u64) -> bool {
        if !self.fits(bytes) {
            return false;
        }
        self.used = self.used.saturating_add(bytes);
        true
    }

    /// Bytes of the animations drawn this frame.
    #[cfg(test)]
    pub fn used(&self) -> u64 {
        self.used
    }

    pub(super) fn known(&self, req: &MediaRequest) -> Option<u64> {
        self.known.get(req).copied()
    }

    pub(super) fn remember(&mut self, req: &MediaRequest, bytes: u64) {
        if self.known.len() >= MAX_KNOWN_ANIMATIONS && !self.known.contains_key(req) {
            self.known.clear();
        }
        self.known.insert(req.clone(), bytes);
    }

    /// The frame to draw of the animation `id` (with these frame `delays`) at `now`.
    /// Counts it as moving; past [`MAX_PLAYING`], or when the settings stop this kind,
    /// it is the first frame. While the window has no focus it is the frame the
    /// animation stopped on.
    pub fn frame_to_draw(
        &mut self,
        id: ImageId,
        motion: Motion,
        delays: impl FnOnce() -> Vec<Duration>,
        now: Instant,
    ) -> usize {
        if !self.prefs.allows(motion) || self.playing >= self.max_playing {
            return 0;
        }
        self.playing += 1;
        let (frame, active, epoch) = (self.frame, self.active, self.epoch);
        let player = self.players.entry(id).or_insert_with(|| {
            let mut playback = Playback::new(delays(), epoch.unwrap_or(now));
            if !active {
                playback.pause(now);
            }
            Player {
                playback,
                used: frame,
            }
        });
        player.used = frame;
        let soon = now + LOOKAHEAD;
        let shown = player.playback.frame_at(soon);
        if let Some(next) = player.playback.time_to_next_frame(soon) {
            let at = soon + next.max(MIN_FRAME_GAP);
            self.wake = Some(self.wake.map_or(at, |w| w.min(at)));
        }
        shown
    }

    /// Animations that moved this frame.
    #[cfg(any(test, feature = "demo"))]
    pub fn playing(&self) -> usize {
        self.playing
    }

    /// When this frame's animations want the next redraw; `None` when nothing moves
    /// (the window has no focus, or no animation is on screen).
    #[cfg(test)]
    pub fn wake(&self) -> Option<Instant> {
        self.wake
    }
}

/// The frame delays of a gpui image.
fn delays(image: &RenderImage) -> Vec<Duration> {
    (0..image.frame_count())
        .map(|i| Duration::from(image.delay(i)))
        .collect()
}

/// An animated image, `size` logical pixels, drawn with the frame the budget gives.
pub(crate) fn animated(
    image: Arc<RenderImage>,
    (w, h): (f32, f32),
    radius: f32,
    fit: ObjectFit,
    motion: Motion,
    budget: Rc<RefCell<AnimationBudget>>,
) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            paint_animated(
                &image, bounds, bounds, radius, fit, motion, &budget, window, cx,
            );
        },
    )
    .w(px(w))
    .h(px(h))
    .flex_none()
    .into_any_element()
}

/// Paints the current frame of `image` into `bounds` (fitted, clipped to `clip`), if
/// it is on screen. Used inside other elements' paint (emoji over their placeholders).
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_animated(
    image: &Arc<RenderImage>,
    bounds: Bounds<Pixels>,
    clip: Bounds<Pixels>,
    radius: f32,
    fit: ObjectFit,
    motion: Motion,
    budget: &Rc<RefCell<AnimationBudget>>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = image.frame_count();
    if count == 0 || !window.content_mask().bounds.intersects(&bounds) {
        return;
    }
    let frame = if count == 1 {
        0
    } else {
        budget
            .borrow_mut()
            .frame_to_draw(image.id, motion, || delays(image), Instant::now())
            .min(count - 1)
    };
    let image_bounds = fit.get_bounds(bounds, image.size(frame));
    let _ = window.paint_image(
        clip,
        image_bounds,
        Corners::all(px(radius)),
        image.clone(),
        frame,
        false,
    );
    schedule_wake(budget, window, cx);
}

/// Asks for a redraw when the earliest next frame of this frame is due, unless one is
/// already asked for by then.
fn schedule_wake(budget: &Rc<RefCell<AnimationBudget>>, window: &mut Window, cx: &mut App) {
    let mut b = budget.borrow_mut();
    let Some(at) = b.wake else {
        return;
    };
    let now = Instant::now();
    if b.scheduled
        .as_ref()
        .is_some_and(|(when, _)| *when > now && *when <= at)
    {
        return;
    }
    let view = window.current_view();
    let wait = at.saturating_duration_since(now);
    let task = window.spawn(cx, async move |cx| {
        cx.background_executor().timer(wait).await;
        let _ = cx.update(move |_, cx| cx.notify(view));
    });
    b.scheduled = Some((at, task));
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_media::{Fit, MediaKind};

    fn req(n: u32) -> MediaRequest {
        let url =
            url::Url::parse(&format!("https://media.example.org/avatars/1/{n}.webp")).unwrap();
        MediaRequest::new(url, MediaKind::Avatar, Fit::square(40))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Three frames of 100 ms.
    fn three() -> Vec<Duration> {
        vec![ms(100); 3]
    }

    #[test]
    fn animations_share_one_budget_per_frame() {
        const MIB: u64 = 1024 * 1024;
        let mut budget = AnimationBudget::new(ANIMATION_BUDGET_BYTES);
        // A capped animation (24 MiB) plays; a second one that does not fit shows its
        // still; small ones fill what is left.
        assert!(budget.admit(24 * MIB));
        assert!(!budget.admit(24 * MIB));
        assert!(budget.admit(6 * MIB));
        assert!(budget.admit(2 * MIB));
        assert!(!budget.admit(1));
        assert!(budget.used() <= ANIMATION_BUDGET_BYTES);
        // Many on screen: never more than the budget plays.
        let mut budget = AnimationBudget::new(ANIMATION_BUDGET_BYTES);
        let playing = (0..100).filter(|_| budget.admit(3 * MIB)).count();
        assert_eq!(playing, 10);
        assert!(budget.used() <= ANIMATION_BUDGET_BYTES);
        // A new frame starts empty; sizes seen stay known, bounded.
        budget.next_frame(true, Instant::now());
        assert_eq!(budget.used(), 0);
        for n in 0..(MAX_KNOWN_ANIMATIONS as u32 + 10) {
            budget.remember(&req(n), 1);
        }
        assert!(budget.known.len() <= MAX_KNOWN_ANIMATIONS);
        assert_eq!(budget.known(&req(MAX_KNOWN_ANIMATIONS as u32 + 9)), Some(1));
    }

    #[test]
    fn animation_pauses_when_the_window_loses_focus() {
        let t0 = Instant::now();
        let id = ImageId(1);
        let mut b = AnimationBudget::default();
        b.next_frame(true, t0);
        assert_eq!(b.frame_to_draw(id, Motion::Gif, three, t0), 0);
        assert_eq!(b.wake(), Some(t0 + ms(100)));
        b.next_frame(true, t0 + ms(150));
        assert_eq!(b.frame_to_draw(id, Motion::Gif, three, t0 + ms(150)), 1);

        // The window loses the focus 50 ms into the second frame: it stays there, and
        // no redraw is asked for, however long the window stays in the background.
        b.next_frame(false, t0 + ms(150));
        assert_eq!(b.frame_to_draw(id, Motion::Gif, three, t0 + ms(150)), 1);
        for later in [ms(160), ms(1_000), ms(3_600_000)] {
            b.next_frame(false, t0 + later);
            assert_eq!(b.frame_to_draw(id, Motion::Gif, three, t0 + later), 1);
            assert_eq!(b.wake(), None, "no redraw while unfocused");
        }
        // An animation that first shows while unfocused does not move either.
        let fresh = ImageId(2);
        assert_eq!(
            b.frame_to_draw(fresh, Motion::Emoji, three, t0 + ms(3_600_000)),
            0
        );
        b.next_frame(false, t0 + ms(3_600_500));
        assert_eq!(
            b.frame_to_draw(fresh, Motion::Emoji, three, t0 + ms(3_600_500)),
            0
        );
        assert_eq!(
            b.frame_to_draw(id, Motion::Gif, three, t0 + ms(3_600_500)),
            1
        );

        // Focus back: it goes on from where it stopped, the time away does not count.
        let back = t0 + ms(7_200_000);
        b.next_frame(true, back);
        assert_eq!(b.frame_to_draw(id, Motion::Gif, three, back), 1);
        // It stopped 58 ms into the frame (frames due within 8 ms show early).
        assert_eq!(b.wake(), Some(back + ms(42)));
        b.next_frame(true, back + ms(60));
        assert_eq!(b.frame_to_draw(id, Motion::Gif, three, back + ms(60)), 2);
    }

    #[test]
    fn at_most_32_animations_run_at_once() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        b.next_frame(true, t0);
        // 100 different animations on screen.
        for n in 0..100 {
            b.frame_to_draw(ImageId(n), Motion::Emoji, three, t0);
        }
        assert_eq!(b.playing(), MAX_PLAYING);
        assert!(b.players.len() <= MAX_PLAYING);
        // A frame later the first 32 have moved on; the others still show frame one.
        let t1 = t0 + ms(120);
        b.next_frame(true, t1);
        let frames: Vec<usize> = (0..100)
            .map(|n| b.frame_to_draw(ImageId(n), Motion::Emoji, three, t1))
            .collect();
        assert!(frames[..MAX_PLAYING].iter().all(|f| *f == 1), "{frames:?}");
        assert!(frames[MAX_PLAYING..].iter().all(|f| *f == 0), "{frames:?}");
        assert_eq!(b.playing(), MAX_PLAYING);
        // The same emoji 100 times: 32 copies move, the rest show the first frame.
        b.next_frame(true, t1);
        let same: Vec<usize> = (0..100)
            .map(|_| b.frame_to_draw(ImageId(0), Motion::Emoji, three, t1))
            .collect();
        assert_eq!(same.iter().filter(|f| **f == 1).count(), MAX_PLAYING);
        assert_eq!(same.iter().filter(|f| **f == 0).count(), 100 - MAX_PLAYING);
        assert_eq!(b.playing(), MAX_PLAYING);
        // Players of animations no longer drawn are let go.
        b.next_frame(true, t1 + ms(10));
        b.next_frame(true, t1 + ms(20));
        assert!(b.players.is_empty());
    }

    #[test]
    fn animations_of_the_same_pace_share_their_redraws() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        let pace = || vec![ms(90); 8];
        b.next_frame(true, t0);
        // Thirty animations that show up at different times over the first second.
        let mut now = t0;
        let mut redraws = 0;
        let end = t0 + ms(3_000);
        while now < end {
            b.next_frame(true, now);
            let shown = ((now - t0).as_millis() / 33).min(30) as usize;
            for n in 0..shown {
                b.frame_to_draw(ImageId(n), Motion::Emoji, pace, now);
            }
            redraws += 1;
            // The next redraw: when asked for, else the next one an image load causes.
            now = b.wake().unwrap_or(now + ms(33)).max(now + ms(1));
        }
        // After the first second they all change frames together: about one redraw per
        // 90 ms frame, not one per animation.
        assert!(redraws <= 30 + 3_000 / 90 + 2, "{redraws} redraws");
    }

    #[test]
    fn an_account_switch_forgets_the_last_sessions_animations() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        b.prefs.animate_stickers = false;
        b.next_frame(true, t0);
        b.remember(&req(1), 5);
        b.frame_to_draw(ImageId(1), Motion::Gif, three, t0);
        b.reset();
        assert!(b.known.is_empty());
        assert!(b.players.is_empty());
        assert_eq!(b.wake(), None);
        assert!(!b.prefs.animate_stickers, "the settings stay");
    }

    #[test]
    fn animated_emoji_setting_off_shows_the_first_frame() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        b.prefs.animate_emoji = false;
        b.next_frame(true, t0);
        for later in [0, 150, 250, 1_000] {
            b.next_frame(true, t0 + ms(later));
            assert_eq!(
                b.frame_to_draw(ImageId(1), Motion::Emoji, three, t0 + ms(later)),
                0
            );
            assert_eq!(b.wake(), None);
        }
        assert_eq!(b.playing(), 0);
        // Other kinds still move.
        b.next_frame(true, t0 + ms(2_000));
        let before = b.frame_to_draw(ImageId(2), Motion::Gif, three, t0 + ms(2_000));
        assert_ne!(
            b.frame_to_draw(ImageId(2), Motion::Gif, three, t0 + ms(2_150)),
            before
        );

        // The setting lives in settings.json next to the other settings.
        let dir = std::env::temp_dir().join(format!("inlet-motion-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        assert_eq!(MotionPrefs::load(&path), MotionPrefs::default());
        assert!(MotionPrefs::default().animate_emoji);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"desktop": {"zoom": 1.25}}"#).unwrap();
        let off = MotionPrefs {
            animate_emoji: false,
            ..MotionPrefs::default()
        };
        off.save(&path).unwrap();
        assert_eq!(MotionPrefs::load(&path), off);
        assert_eq!(prefs::load(&path).zoom, 1.25);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
