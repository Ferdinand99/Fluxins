// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scene files of the visual checks (`tests/visual/scenes/*.toml`): what the demo build
//! opens and how big, and how `inlet-visual` compares the screenshot with its baseline.
//!
//! ```toml
//! name = "chat-groups"          # also the screenshot's file name
//! window = [1280, 860]          # inner size in logical pixels
//! scale = 1.0                   # device pixels per logical pixel the baseline has
//! theme = "dark"                # the only theme so far
//! ignore = [[0, 0, 72, 48]]     # [x, y, w, h] in logical pixels, not compared
//! demo = true                   # false: no demo account (the sign-in screen)
//! scene = "default"             # a demo scene (`scenes.rs`)
//! guild = "g1"                  # guild and channel to open (demo IDs)
//! channel = "c-general"
//! ```
//!
//! Optional too: `settings` (a settings tab to open), `deep_link` (an `inlet://` link to
//! offer), `accent` (an instance accent such as `"#ff7e00"`), and `tolerance`
//! (`{ channel = 2, pixels = 0.0 }`: how far a colour channel may be off before a pixel
//! counts as different, and the share of pixels that may differ). CI draws every scene
//! pixel for pixel the same, so the defaults are strict; a scene may loosen them up to
//! `channel = 16` and `pixels = 0.001`, and may leave out at most half of its window.
//! Use `ignore` for an area that really changes from run to run.

use std::path::Path;

use serde::Deserialize;

/// Per-channel difference (0–255) up to which a pixel counts as equal. The selected
/// channel row differs from the sidebar by 24 and the chat area by 5, so this must stay
/// small for the check to see the palette.
pub const DEFAULT_CHANNEL_TOLERANCE: u8 = 2;
/// Share of compared pixels that may differ before a screenshot fails.
pub const DEFAULT_PIXEL_TOLERANCE: f64 = 0.0;
/// The loosest per-channel tolerance a scene may ask for.
pub const MAX_CHANNEL_TOLERANCE: u8 = 16;
/// The largest share of differing pixels a scene may allow.
pub const MAX_PIXEL_TOLERANCE: f64 = 0.001;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SceneFile {
    pub name: String,
    pub window: [u32; 2],
    pub scale: f32,
    pub theme: String,
    #[serde(default)]
    pub ignore: Vec<[u32; 4]>,
    #[serde(default)]
    pub tolerance: Tolerance,
    #[serde(default = "yes")]
    pub demo: bool,
    pub scene: Option<String>,
    pub guild: Option<String>,
    pub channel: Option<String>,
    pub settings: Option<String>,
    pub deep_link: Option<String>,
    pub accent: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default = "default_pixels")]
    pub pixels: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            channel: DEFAULT_CHANNEL_TOLERANCE,
            pixels: DEFAULT_PIXEL_TOLERANCE,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_channel() -> u8 {
    DEFAULT_CHANNEL_TOLERANCE
}

fn default_pixels() -> f64 {
    DEFAULT_PIXEL_TOLERANCE
}

impl SceneFile {
    pub fn load(path: &Path) -> Result<SceneFile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::parse(&text)
    }

    /// Parses and checks a scene file.
    pub fn parse(text: &str) -> Result<SceneFile, String> {
        let file: SceneFile = toml::from_str(text).map_err(|e| e.to_string())?;
        file.check()?;
        Ok(file)
    }

    fn check(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !name_ok {
            return Err(format!(
                "name {:?}: use letters, digits, '-' and '_' only",
                self.name
            ));
        }
        let [w, h] = self.window;
        if !(320..=3840).contains(&w) || !(240..=2160).contains(&h) {
            return Err(format!("window {w}x{h} is outside 320x240 to 3840x2160"));
        }
        if !(self.scale.is_finite() && (0.5..=4.0).contains(&self.scale)) {
            return Err(format!("scale {} is outside 0.5 to 4", self.scale));
        }
        if self.theme != "dark" {
            return Err(format!("theme {:?}: only \"dark\" exists", self.theme));
        }
        let mut ignored = 0u64;
        for r in &self.ignore {
            let [x, y, rw, rh] = *r;
            if rw == 0 || rh == 0 || x.saturating_add(rw) > w || y.saturating_add(rh) > h {
                return Err(format!("ignore {r:?} is empty or outside the window"));
            }
            ignored += u64::from(rw) * u64::from(rh);
        }
        // Overlapping boxes count twice: the limit errs on the strict side.
        if ignored * 2 > u64::from(w) * u64::from(h) {
            return Err("ignore leaves out more than half of the window".into());
        }
        if self.tolerance.channel > MAX_CHANNEL_TOLERANCE {
            return Err(format!(
                "tolerance.channel {} is above {MAX_CHANNEL_TOLERANCE}",
                self.tolerance.channel
            ));
        }
        if !(self.tolerance.pixels.is_finite()
            && (0.0..=MAX_PIXEL_TOLERANCE).contains(&self.tolerance.pixels))
        {
            return Err(format!(
                "tolerance.pixels is a share from 0 to {MAX_PIXEL_TOLERANCE}"
            ));
        }
        if !self.demo
            && (self.scene.is_some()
                || self.guild.is_some()
                || self.channel.is_some()
                || self.settings.is_some()
                || self.deep_link.is_some()
                || self.accent.is_some())
        {
            return Err("demo = false shows the sign-in screen; it takes no demo fields".into());
        }
        if self.channel.is_some() && self.guild.is_none() {
            return Err("channel needs guild".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scene_file_parses_with_defaults_and_bad_ones_are_refused() {
        let file = SceneFile::parse(
            r#"
            name = "chat-groups"
            window = [1280, 860]
            scale = 1.0
            theme = "dark"
            ignore = [[0, 0, 10, 10]]
            scene = "chat-cozy"
            guild = "g1"
            channel = "c-general"
            "#,
        )
        .unwrap();
        assert_eq!(file.window, [1280, 860]);
        assert!(file.demo);
        assert_eq!(file.tolerance, Tolerance::default());
        assert_eq!(file.ignore, vec![[0, 0, 10, 10]]);

        let base = "name = \"x\"\nwindow = [1280, 860]\nscale = 1.0\n";
        for bad in [
            format!("{base}theme = \"light\""),
            format!("{base}theme = \"dark\"\ncolour = 1"),
            format!("{base}theme = \"dark\"\nignore = [[1200, 0, 100, 10]]"),
            format!("{base}theme = \"dark\"\ndemo = false\nguild = \"g1\""),
            format!("{base}theme = \"dark\"\nchannel = \"c-general\""),
            format!("{base}theme = \"dark\"\ntolerance = {{ channel = 17 }}"),
            format!("{base}theme = \"dark\"\ntolerance = {{ pixels = 0.01 }}"),
            format!("{base}theme = \"dark\"\nignore = [[0, 0, 1280, 500]]"),
            "name = \"../x\"\nwindow = [1280, 860]\nscale = 1.0\ntheme = \"dark\"".into(),
        ] {
            assert!(SceneFile::parse(&bad).is_err(), "{bad}");
        }
    }
}
