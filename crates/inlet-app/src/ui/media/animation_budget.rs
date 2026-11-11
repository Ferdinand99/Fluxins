// SPDX-License-Identifier: AGPL-3.0-or-later
//! The one budget for animated images (GIFs, animated attachments and embeds, animated
//! avatars, custom emoji and stickers), and the rules for when they move:
//!
//! - **Memory.** Animations keep every frame decoded, so the ones a frame draws share
//!   [`ANIMATION_BUDGET_BYTES`]; one that does not fit shows its still (see
//!   `Inlet::moving`). Each is capped on its own by the client (`ANIMATION_MAX_BYTES`).
//!   Only an image admitted to that budget this frame moves; a still that happens to
//!   decode with several frames shows its first.
//! - **Count.** At most [`MAX_PLAYING`] animations move at once on screen; the others
//!   show their first frame. The image viewer reserves its place before the chat is
//!   drawn, and nothing under an open layer moves.
//! - **One clock.** Every animation takes its frame from one clock that runs only while
//!   the window has the focus, the user has touched it within [`IDLE_AFTER`], and the
//!   system does not ask for reduced motion (then every animation shows its first
//!   frame). Stopped animations keep their frame and go on from there.
//! - **Redraws** come only when a frame is due, rounded up to a shared grid of the clock
//!   ([`GIF_GRID`] for GIFs and the viewer, [`DECORATION_GRID`] for emoji, stickers and
//!   avatars), so animations of any pace share them; none while nothing moves.
//! - **Settings.** With animated emoji (or stickers) switched off they show their first
//!   frame, and only the still is fetched ([`MotionPrefs`], kept in `settings.json`).
//!
//! The views draw an animation with [`animated`]. Off-screen animations are neither
//! counted nor drawn.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    canvas, prelude::*, px, AnyElement, App, Bounds, Corners, ImageId, ObjectFit, Pixels,
    RenderImage, Task, Window,
};
use inlet_media::MediaRequest;
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
/// Redraws for GIFs and the image viewer fall on this grid of the clock (30 a second
/// at most).
pub const GIF_GRID: Duration = Duration::from_nanos(33_333_333);
/// Redraws for emoji, stickers and avatars fall on this grid (20 a second at most).
pub const DECORATION_GRID: Duration = Duration::from_millis(50);
/// Animations stop after this long without keyboard or mouse input in the window, and
/// go on with the next input.
pub const IDLE_AFTER: Duration = Duration::from_secs(5 * 60);
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

/// What the window is like this frame, for the animations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage {
    /// The window has the focus.
    pub active: bool,
    /// No input for [`IDLE_AFTER`].
    pub idle: bool,
    /// The system asks for reduced motion: every animation shows its first frame.
    pub reduce_motion: bool,
    /// A layer (the image viewer, settings, a prompt, the account switcher) lies over
    /// the chat: only the animation the layer reserved moves.
    pub layer_open: bool,
}

impl Stage {
    /// A focused, used window with nothing over the chat.
    pub const LIVE: Stage = Stage {
        active: true,
        idle: false,
        reduce_motion: false,
        layer_open: false,
    };

    /// Whether the animation clock runs.
    fn running(self) -> bool {
        self.active && !self.idle && !self.reduce_motion
    }
}

struct Player {
    /// Frame lengths (at least 1 ms each) and their sum.
    delays: Vec<Duration>,
    total: Duration,
    /// The frame it showed last.
    shown: usize,
    /// The frame that last drew it.
    used: u64,
}

impl Player {
    fn new(delays: Vec<Duration>, used: u64) -> Self {
        let delays: Vec<Duration> = delays
            .into_iter()
            .map(|d| d.max(Duration::from_millis(1)))
            .collect();
        let total = delays.iter().sum();
        Player {
            delays,
            total,
            shown: 0,
            used,
        }
    }

    /// The frame at `clock` and how long until the next one (`None` for a still).
    fn at(&self, clock: Duration) -> (usize, Option<Duration>) {
        if self.delays.len() < 2 || self.total.is_zero() {
            return (0, None);
        }
        let nanos = clock.as_nanos() % self.total.as_nanos();
        let mut into = Duration::from_nanos(nanos as u64);
        for (i, d) in self.delays.iter().enumerate() {
            if into < *d {
                return (i, Some(*d - into));
            }
            into -= *d;
        }
        (0, Some(self.delays[0]))
    }
}

/// Rounds `t` up to a whole number of `grid`.
fn round_up(t: Duration, grid: Duration) -> Duration {
    let g = grid.as_nanos().max(1);
    let n = t.as_nanos().div_ceil(g) * g;
    Duration::from_nanos(n as u64)
}

/// The animations of one frame: their bytes against [`ANIMATION_BUDGET_BYTES`], which
/// ones may move, how many move, and which frame each shows.
pub struct AnimationBudget {
    budget: u64,
    used: u64,
    /// Decoded sizes of animations seen, by request.
    known: HashMap<MediaRequest, u64>,
    /// Images admitted to the byte budget this frame: only these may move. A still that
    /// happens to decode with several frames shows its first.
    admitted: HashSet<ImageId>,
    /// Images the topmost layer draws itself this frame (a picker's grid): they move
    /// while the layer is open, and count against [`MAX_PLAYING`] like any other.
    in_top_layer: HashSet<ImageId>,
    /// The animation of the open layer (the image viewer): it always moves, and takes
    /// one of the [`MAX_PLAYING`] places before the chat is drawn.
    reserved: Option<ImageId>,
    max_playing: usize,
    /// Animations that moved this frame.
    playing: usize,
    stage: Stage,
    /// The animation clock: it runs only while the window is focused, used and not
    /// asked to reduce motion, so every animation stops where it is and goes on from
    /// there, and all of them keep one phase.
    clock: Duration,
    /// When the clock was last read.
    clock_at: Option<Instant>,
    frame: u64,
    /// The animations drawn last frame. Images that show the same animation share one.
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
            admitted: HashSet::new(),
            in_top_layer: HashSet::new(),
            reserved: None,
            max_playing: MAX_PLAYING,
            playing: 0,
            stage: Stage::LIVE,
            clock: Duration::ZERO,
            clock_at: None,
            frame: 0,
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

    /// A new frame at `now`: nothing is admitted or plays yet. The clock moves on by
    /// the time since the last frame if it ran then.
    pub fn next_frame(&mut self, stage: Stage, now: Instant) {
        if let Some(at) = self.clock_at {
            if self.stage.running() {
                self.clock += now.saturating_duration_since(at);
            }
        }
        self.clock_at = Some(now);
        self.stage = stage;
        self.used = 0;
        self.playing = 0;
        self.admitted.clear();
        self.in_top_layer.clear();
        self.reserved = None;
        self.wake = None;
        self.frame += 1;
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

    /// [`admit`](Self::admit) for the image `id`, which may then move this frame.
    pub fn admit_image(&mut self, id: ImageId, bytes: u64) -> bool {
        let fits = self.admit(bytes);
        if fits {
            self.admitted.insert(id);
        }
        fits
    }

    /// Marks the admitted image `id` as drawn by the topmost layer itself (a picker):
    /// unlike what lies under the layer, it moves while the layer is open. Call it after
    /// [`admit_image`](Self::admit_image) admitted it this frame.
    pub fn in_top_layer(&mut self, id: ImageId) {
        if self.admitted.contains(&id) {
            self.in_top_layer.insert(id);
        }
    }

    /// The open layer's animation (the image viewer): it moves whatever the chat under
    /// it holds. Call it before the chat is drawn.
    pub fn reserve(&mut self, id: ImageId) {
        self.reserved = Some(id);
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

    /// Whether the last frame found the window idle (no input for a while).
    pub fn is_idle(&self) -> bool {
        self.stage.idle
    }

    /// The frame to draw of the animation `id` (with these frame `delays`). Only an
    /// image admitted this frame (or the layer's reserved one) moves, at most
    /// [`MAX_PLAYING`] at once; the others, and kinds the settings stop, show their
    /// first frame, as does everything when the system asks for reduced motion. Under
    /// an open layer an animation keeps the frame it showed. Without focus or input the
    /// clock stands, so every animation keeps its frame.
    pub fn frame_to_draw(
        &mut self,
        id: ImageId,
        motion: Motion,
        delays: impl FnOnce() -> Vec<Duration>,
    ) -> usize {
        if self.stage.reduce_motion || !self.prefs.allows(motion) {
            return 0;
        }
        let frame = self.frame;
        let reserved = self.reserved == Some(id);
        if !reserved {
            if !self.admitted.contains(&id) {
                return 0;
            }
            if self.stage.layer_open && !self.in_top_layer.contains(&id) {
                // Frozen under the layer: not counted, no redraw.
                return self.players.get_mut(&id).map_or(0, |p| {
                    p.used = frame;
                    p.shown
                });
            }
            let places = self.max_playing - usize::from(self.reserved.is_some());
            if self.playing >= places {
                return 0;
            }
            self.playing += 1;
        }
        let player = self
            .players
            .entry(id)
            .or_insert_with(|| Player::new(delays(), frame));
        player.used = frame;
        let (shown, next) = player.at(self.clock);
        player.shown = shown;
        if let (Some(next), true) = (next, self.stage.running()) {
            // The next frame, on a shared grid of the clock, so animations of different
            // paces share their redraws.
            let grid = match motion {
                Motion::Gif => GIF_GRID,
                Motion::Avatar | Motion::Emoji | Motion::Sticker => DECORATION_GRID,
            };
            let due = round_up(self.clock + next, grid);
            let at = self.clock_at.unwrap_or_else(Instant::now) + (due - self.clock);
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
    /// (no focus, no input, reduced motion, or no animation on screen).
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
            .frame_to_draw(image.id, motion, || delays(image))
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
    // A little late rather than early: a timer that fires before the frame is due
    // would only draw the same frame again.
    let wait = at.saturating_duration_since(now) + Duration::from_millis(2);
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

    const LIVE: Stage = Stage::LIVE;
    const UNFOCUSED: Stage = Stage {
        active: false,
        ..Stage::LIVE
    };

    /// A new frame at `t` in which the images `ids` were admitted to the byte budget.
    fn frame(
        b: &mut AnimationBudget,
        stage: Stage,
        t: Instant,
        ids: impl IntoIterator<Item = usize>,
    ) {
        b.next_frame(stage, t);
        for n in ids {
            assert!(b.admit_image(ImageId(n), 1));
        }
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
        budget.next_frame(LIVE, Instant::now());
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
        let draw = |b: &mut AnimationBudget, id| b.frame_to_draw(id, Motion::Emoji, three);
        frame(&mut b, LIVE, t0, [1]);
        assert_eq!(draw(&mut b, id), 0);
        assert_eq!(b.wake(), Some(t0 + ms(100)));
        frame(&mut b, LIVE, t0 + ms(150), [1]);
        assert_eq!(draw(&mut b, id), 1);
        assert_eq!(b.wake(), Some(t0 + ms(200)));

        // The window loses the focus 50 ms into the second frame: it stays there, and
        // no redraw is asked for, however long the window stays in the background.
        frame(&mut b, UNFOCUSED, t0 + ms(150), [1, 2]);
        assert_eq!(draw(&mut b, id), 1);
        let fresh_first = draw(&mut b, ImageId(2));
        for later in [ms(160), ms(1_000), ms(3_600_150)] {
            frame(&mut b, UNFOCUSED, t0 + later, [1, 2]);
            assert_eq!(draw(&mut b, id), 1);
            // One that first shows while unfocused stands still too.
            assert_eq!(draw(&mut b, ImageId(2)), fresh_first);
            assert_eq!(b.wake(), None, "no redraw while unfocused");
        }

        // Focus back: it goes on from where it stopped, the time away does not count.
        let back = t0 + ms(7_200_000);
        frame(&mut b, LIVE, back, [1]);
        assert_eq!(draw(&mut b, id), 1);
        assert_eq!(b.wake(), Some(back + ms(50)));
        frame(&mut b, LIVE, back + ms(60), [1]);
        assert_eq!(draw(&mut b, id), 2);
    }

    #[test]
    fn at_most_32_animations_run_at_once() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        // 100 different animations on screen.
        frame(&mut b, LIVE, t0, 0..100);
        for n in 0..100 {
            b.frame_to_draw(ImageId(n), Motion::Emoji, three);
        }
        assert_eq!(b.playing(), MAX_PLAYING);
        assert!(b.players.len() <= MAX_PLAYING);
        // A frame later the first 32 have moved on; the others still show frame one.
        let t1 = t0 + ms(120);
        frame(&mut b, LIVE, t1, 0..100);
        let frames: Vec<usize> = (0..100)
            .map(|n| b.frame_to_draw(ImageId(n), Motion::Emoji, three))
            .collect();
        assert!(frames[..MAX_PLAYING].iter().all(|f| *f == 1), "{frames:?}");
        assert!(frames[MAX_PLAYING..].iter().all(|f| *f == 0), "{frames:?}");
        assert_eq!(b.playing(), MAX_PLAYING);
        // The same emoji 100 times: 32 copies move, the rest show the first frame.
        frame(&mut b, LIVE, t1, [0]);
        let same: Vec<usize> = (0..100)
            .map(|_| b.frame_to_draw(ImageId(0), Motion::Emoji, three))
            .collect();
        assert_eq!(same.iter().filter(|f| **f == 1).count(), MAX_PLAYING);
        assert_eq!(same.iter().filter(|f| **f == 0).count(), 100 - MAX_PLAYING);
        assert_eq!(b.playing(), MAX_PLAYING);
        // Players of animations no longer drawn are let go.
        b.next_frame(LIVE, t1 + ms(10));
        b.next_frame(LIVE, t1 + ms(20));
        assert!(b.players.is_empty());
    }

    #[test]
    fn the_viewer_moves_over_a_full_chat_and_nothing_under_a_layer_does() {
        let t0 = Instant::now();
        let viewer = ImageId(1000);
        let all = || (0..40).chain([1000]);
        let mut b = AnimationBudget::default();
        // Without a layer: the reserved animation takes one place before the chat.
        frame(&mut b, LIVE, t0, all());
        b.reserve(viewer);
        for n in 0..40 {
            b.frame_to_draw(ImageId(n), Motion::Gif, three);
        }
        assert_eq!(b.playing(), MAX_PLAYING - 1);
        b.frame_to_draw(viewer, Motion::Gif, three);
        frame(&mut b, LIVE, t0 + ms(110), all());
        b.reserve(viewer);
        for n in 0..40 {
            b.frame_to_draw(ImageId(n), Motion::Gif, three);
        }
        assert_eq!(
            b.frame_to_draw(viewer, Motion::Gif, three),
            1,
            "the viewer moves"
        );

        // The viewer open over 40 animations in the chat (drawn first): they keep their
        // frame, are not counted and ask for no redraw; the viewer moves on.
        let open = Stage {
            layer_open: true,
            ..LIVE
        };
        frame(&mut b, open, t0 + ms(220), all());
        b.reserve(viewer);
        let under: Vec<usize> = (0..40)
            .map(|n| b.frame_to_draw(ImageId(n), Motion::Gif, three))
            .collect();
        assert_eq!(b.playing(), 0);
        assert_eq!(b.wake(), None, "nothing under the layer asks for a redraw");
        assert_eq!(b.frame_to_draw(viewer, Motion::Gif, three), 2);
        assert!(b.wake().is_some());
        frame(&mut b, open, t0 + ms(330), all());
        b.reserve(viewer);
        let later: Vec<usize> = (0..40)
            .map(|n| b.frame_to_draw(ImageId(n), Motion::Gif, three))
            .collect();
        assert_eq!(under, later, "frozen under the layer");
        assert_eq!(b.frame_to_draw(viewer, Motion::Gif, three), 0);
    }

    #[test]
    fn a_picker_layer_moves_its_own_animations_and_freezes_the_chat_under_it() {
        let t0 = Instant::now();
        let open = Stage {
            layer_open: true,
            ..LIVE
        };
        let mut b = AnimationBudget::default();
        // 40 chat animations (0..40) under the picker, 40 in the picker (100..140).
        let all = || (0..40).chain(100..140);
        let draw = |b: &mut AnimationBudget| -> (Vec<usize>, Vec<usize>) {
            let chat = (0..40)
                .map(|n| b.frame_to_draw(ImageId(n), Motion::Emoji, three))
                .collect();
            let picker = (100..140)
                .map(|n| b.frame_to_draw(ImageId(n), Motion::Emoji, three))
                .collect();
            (chat, picker)
        };
        frame(&mut b, open, t0, all());
        for n in 100..140 {
            b.in_top_layer(ImageId(n));
        }
        // An image that was not admitted this frame cannot be marked.
        b.in_top_layer(ImageId(999));
        let (chat0, picker0) = draw(&mut b);
        assert_eq!(
            b.playing(),
            MAX_PLAYING,
            "the picker's own count, capped at 32"
        );
        assert!(b.wake().is_some(), "the picker asks for its next frame");
        frame(&mut b, open, t0 + ms(110), all());
        for n in 100..140 {
            b.in_top_layer(ImageId(n));
        }
        let (chat1, picker1) = draw(&mut b);
        assert_eq!(chat0, chat1, "the chat under the picker stands still");
        assert!(chat1.iter().all(|f| *f == 0));
        assert_eq!(&picker1[..MAX_PLAYING], &[1; MAX_PLAYING][..], "they move");
        assert!(
            picker1[MAX_PLAYING..].iter().all(|f| *f == 0),
            "the 33rd does not"
        );
        assert_ne!(picker0, picker1);
        // Reduced motion and an unfocused window stop the picker too.
        let still = Stage {
            reduce_motion: true,
            ..open
        };
        frame(&mut b, still, t0 + ms(220), all());
        for n in 100..140 {
            b.in_top_layer(ImageId(n));
        }
        let (_, picker2) = draw(&mut b);
        assert!(picker2.iter().all(|f| *f == 0));
    }

    #[test]
    fn an_image_not_admitted_shows_its_first_frame() {
        // A still asked for without `animated`, that decodes with several frames anyway
        // (or the still shown when the budget is spent): it never moves.
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        for t in [0, 150, 250] {
            b.next_frame(LIVE, t0 + ms(t));
            assert_eq!(b.frame_to_draw(ImageId(7), Motion::Gif, three), 0);
            assert_eq!(b.playing(), 0);
            assert_eq!(b.used(), 0);
            assert_eq!(b.wake(), None);
        }
    }

    #[test]
    fn idle_windows_and_reduced_motion_stand_still() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        frame(&mut b, LIVE, t0, [1]);
        b.frame_to_draw(ImageId(1), Motion::Gif, three);
        // No input for a while: the clock stops where it was, no redraws.
        let idle = Stage { idle: true, ..LIVE };
        frame(&mut b, idle, t0 + ms(150), [1]);
        assert!(b.is_idle());
        assert_eq!(b.frame_to_draw(ImageId(1), Motion::Gif, three), 1);
        frame(&mut b, idle, t0 + ms(60_000), [1]);
        assert_eq!(b.frame_to_draw(ImageId(1), Motion::Gif, three), 1);
        assert_eq!(b.wake(), None);
        // Input again: on from there.
        frame(&mut b, LIVE, t0 + ms(60_010), [1]);
        assert!(!b.is_idle());
        assert_eq!(b.frame_to_draw(ImageId(1), Motion::Gif, three), 1);
        assert!(b.wake().is_some());
        // The system asks for reduced motion: the first frame, no redraws.
        let reduced = Stage {
            reduce_motion: true,
            ..LIVE
        };
        frame(&mut b, reduced, t0 + ms(60_200), [1]);
        assert_eq!(b.frame_to_draw(ImageId(1), Motion::Gif, three), 0);
        assert_eq!(b.wake(), None);
    }

    /// Redraws over `secs` seconds of animations with these frame lengths (all drawn
    /// every frame), following the redraws they ask for.
    fn redraws(paces: &[(u64, Motion)], secs: u64) -> usize {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        let mut now = t0;
        let mut count = 0;
        while now < t0 + Duration::from_secs(secs) {
            frame(&mut b, LIVE, now, 0..paces.len());
            for (n, (pace, motion)) in paces.iter().enumerate() {
                b.frame_to_draw(ImageId(n), *motion, || vec![ms(*pace); 8]);
            }
            count += 1;
            now = b.wake().expect("they move").max(now + ms(1));
        }
        count
    }

    #[test]
    fn animations_of_any_pace_share_their_redraws() {
        // One pace: one redraw per frame of it.
        let one = redraws(&[(90, Motion::Emoji); 30], 3);
        assert!(one <= 3_000 / 90 + 2, "{one}");
        // Mixed paces of emoji and stickers: at most the 50 ms grid, not one per pace.
        let mixed = redraws(
            &[
                (70, Motion::Emoji),
                (90, Motion::Emoji),
                (80, Motion::Sticker),
                (120, Motion::Sticker),
            ],
            3,
        );
        assert!(mixed <= 3_000 / 50 + 1, "{mixed}");
        // A GIF of 20 ms frames: at most the 33 ms grid, not 50 a second.
        let fast = redraws(&[(20, Motion::Gif)], 3);
        assert!(fast <= 3_000 / 33 + 1, "{fast}");
    }

    #[test]
    fn an_account_switch_forgets_the_last_sessions_animations() {
        let t0 = Instant::now();
        let mut b = AnimationBudget::default();
        b.prefs.animate_stickers = false;
        frame(&mut b, LIVE, t0, [1]);
        b.remember(&req(1), 5);
        b.frame_to_draw(ImageId(1), Motion::Gif, three);
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
        for later in [0, 150, 250, 1_000] {
            frame(&mut b, LIVE, t0 + ms(later), [1, 2]);
            assert_eq!(b.frame_to_draw(ImageId(1), Motion::Emoji, three), 0);
            assert_eq!(b.wake(), None);
        }
        assert_eq!(b.playing(), 0);
        // Other kinds still move.
        frame(&mut b, LIVE, t0 + ms(2_000), [2]);
        let before = b.frame_to_draw(ImageId(2), Motion::Gif, three);
        frame(&mut b, LIVE, t0 + ms(2_150), [2]);
        assert_ne!(b.frame_to_draw(ImageId(2), Motion::Gif, three), before);

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
