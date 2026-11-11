// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where the main window was, per monitor.
//!
//! The window's size and place are kept for each monitor it was on (keyed by the
//! monitor's stable id), plus which monitor it was on last. At start-up [`place`] puts
//! it back there when that monitor is still connected and the window's middle is on
//! it; otherwise (monitor unplugged, resolution changed) the window is centred on the
//! main monitor at the default size.
//!
//! Positions are in the coordinates gpui gives for the window: global on Windows and
//! Linux, relative to the monitor on macOS. The caller passes the monitors' bounds in
//! the same coordinates.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The smallest window that is restored as it was; anything smaller is a bad value.
pub const MIN_SIZE: (f32, f32) = (400.0, 300.0);
/// How many monitors are remembered.
const MAX_MONITORS: usize = 8;

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn is_sane(&self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite())
            && self.w >= MIN_SIZE.0
            && self.h >= MIN_SIZE.1
    }

    fn contains_middle_of(&self, other: &Rect) -> bool {
        let (cx, cy) = (other.x + other.w / 2.0, other.y + other.h / 2.0);
        cx >= self.x && cx < self.x + self.w && cy >= self.y && cy < self.y + self.h
    }

    /// `self` made to fit inside `area`: no larger, and moved in where it sticks out.
    fn fitted_into(&self, area: &Rect) -> Rect {
        let w = self.w.min(area.w);
        let h = self.h.min(area.h);
        let x = self.x.clamp(area.x, area.x + area.w - w);
        let y = self.y.clamp(area.y, area.y + area.h - h);
        Rect { x, y, w, h }
    }
}

/// The window on one monitor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub rect: Rect,
    #[serde(default)]
    pub maximized: bool,
}

/// What is kept on disk.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowMemory {
    /// The monitor the window was on last.
    #[serde(default)]
    pub last: Option<String>,
    /// Per monitor id.
    #[serde(default)]
    pub monitors: BTreeMap<String, SavedWindow>,
}

impl WindowMemory {
    /// Notes where the window is now. Returns whether anything changed.
    pub fn remember(&mut self, monitor: &str, rect: Rect, maximized: bool) -> bool {
        if !rect.is_sane() {
            return false;
        }
        let saved = SavedWindow { rect, maximized };
        if self.last.as_deref() == Some(monitor) && self.monitors.get(monitor) == Some(&saved) {
            return false;
        }
        if !self.monitors.contains_key(monitor) && self.monitors.len() >= MAX_MONITORS {
            if let Some(drop) = self
                .monitors
                .keys()
                .find(|k| Some(k.as_str()) != self.last.as_deref())
                .cloned()
            {
                self.monitors.remove(&drop);
            }
        }
        self.monitors.insert(monitor.to_string(), saved);
        self.last = Some(monitor.to_string());
        true
    }
}

/// A connected monitor: its stable id and its usable area, in window coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub id: String,
    pub area: Rect,
}

/// Where to open the window.
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// As it was, on this monitor.
    Restore {
        monitor: String,
        rect: Rect,
        maximized: bool,
    },
    /// Centred on the main monitor, this big.
    Centre { w: f32, h: f32 },
}

/// Puts the window back on the monitor it was on last, or centres it on the main one.
/// `main` is the size of the main monitor's area, if known; the default size is cut to
/// 90 % of it so small screens fit.
pub fn place(
    memory: &WindowMemory,
    monitors: &[Monitor],
    main: Option<(f32, f32)>,
    default: (f32, f32),
) -> Placement {
    let restored = memory.last.as_ref().and_then(|id| {
        let saved = memory.monitors.get(id)?;
        let monitor = monitors.iter().find(|m| &m.id == id)?;
        if !saved.rect.is_sane() || !monitor.area.contains_middle_of(&saved.rect) {
            return None;
        }
        Some(Placement::Restore {
            monitor: id.clone(),
            rect: saved.rect.fitted_into(&monitor.area),
            maximized: saved.maximized,
        })
    });
    restored.unwrap_or_else(|| {
        let (mut w, mut h) = default;
        if let Some((mw, mh)) = main {
            w = w.min(mw * 0.9);
            h = h.min(mh * 0.9);
        }
        Placement::Centre { w, h }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    fn monitors() -> Vec<Monitor> {
        vec![
            Monitor {
                id: "left".into(),
                area: rect(0.0, 0.0, 1920.0, 1040.0),
            },
            Monitor {
                id: "right".into(),
                area: rect(1920.0, 0.0, 2560.0, 1400.0),
            },
        ]
    }

    #[test]
    fn window_state_restores_on_the_same_monitor_or_centres() {
        let mut memory = WindowMemory::default();
        assert!(memory.remember("left", rect(100.0, 80.0, 1000.0, 700.0), false));
        assert!(memory.remember("right", rect(2200.0, 100.0, 1400.0, 900.0), true));
        let default = (1200.0, 780.0);

        // The same monitor is there: back where it was, on that monitor.
        assert_eq!(
            place(&memory, &monitors(), Some((1920.0, 1040.0)), default),
            Placement::Restore {
                monitor: "right".into(),
                rect: rect(2200.0, 100.0, 1400.0, 900.0),
                maximized: true,
            }
        );
        // Each monitor keeps its own place: back on the left one, its own size.
        memory.remember("left", rect(100.0, 80.0, 1000.0, 700.0), false);
        assert_eq!(
            place(&memory, &monitors(), None, default),
            Placement::Restore {
                monitor: "left".into(),
                rect: rect(100.0, 80.0, 1000.0, 700.0),
                maximized: false,
            }
        );
        assert_eq!(memory.monitors.len(), 2);

        // That monitor is gone: centred on the main one, cut to fit it.
        let only_small = vec![Monitor {
            id: "laptop".into(),
            area: rect(0.0, 0.0, 1024.0, 740.0),
        }];
        assert_eq!(
            place(&memory, &only_small, Some((1024.0, 740.0)), default),
            Placement::Centre {
                w: 1024.0 * 0.9,
                h: 740.0 * 0.9
            }
        );

        // The monitor is there but smaller now and the window's middle is off it.
        let shrunk = vec![Monitor {
            id: "left".into(),
            area: rect(0.0, 0.0, 600.0, 500.0),
        }];
        assert!(matches!(
            place(&memory, &shrunk, None, default),
            Placement::Centre { .. }
        ));

        // The middle is on the monitor but an edge sticks out: moved in and cut to fit.
        let mut memory = WindowMemory::default();
        memory.remember("left", rect(1300.0, 500.0, 1000.0, 700.0), false);
        assert_eq!(
            place(&memory, &monitors(), None, default),
            Placement::Restore {
                monitor: "left".into(),
                rect: rect(920.0, 340.0, 1000.0, 700.0),
                maximized: false,
            }
        );

        // Nothing saved: centred at the default size.
        assert_eq!(
            place(&WindowMemory::default(), &monitors(), None, default),
            Placement::Centre {
                w: 1200.0,
                h: 780.0
            }
        );
    }

    #[test]
    fn bad_values_are_not_kept() {
        let mut memory = WindowMemory::default();
        assert!(!memory.remember("left", rect(f32::NAN, 0.0, 1000.0, 700.0), false));
        assert!(!memory.remember("left", rect(0.0, 0.0, 10.0, 10.0), false));
        assert_eq!(memory, WindowMemory::default());
        assert!(memory.remember("a", rect(0.0, 0.0, 800.0, 600.0), false));
        assert!(!memory.remember("a", rect(0.0, 0.0, 800.0, 600.0), false));
        for i in 0..20 {
            memory.remember(&format!("m{i}"), rect(0.0, 0.0, 800.0, 600.0), false);
        }
        assert!(memory.monitors.len() <= MAX_MONITORS);
        assert_eq!(memory.last.as_deref(), Some("m19"));
    }
}
