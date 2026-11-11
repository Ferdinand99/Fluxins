// SPDX-License-Identifier: AGPL-3.0-or-later
//! The desktop settings, kept under `"desktop"` in `settings.json` in Inlet's config
//! folder: closing to the tray, starting hidden, the zoom level and where the window
//! was. Other keys of the file are kept as they are, so other settings live there too
//! ([`load_section`], [`save_section`]; the media settings under `"media"`). Whether
//! Inlet starts at login is not stored here: the operating system's own login item is
//! the truth (`inlet_platform::autostart`).

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::window_state::WindowMemory;

/// The zoom steps Ctrl + and Ctrl − move through, as in a browser.
pub const ZOOM_STEPS: [f32; 11] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// The desktop settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopPrefs {
    /// Closing the window keeps Inlet running in the tray (only while a tray icon is
    /// shown; without one the window closes and Inlet quits).
    pub close_to_tray: bool,
    /// The login item starts Inlet hidden in the tray.
    pub start_minimized: bool,
    /// The zoom level, one of [`ZOOM_STEPS`].
    pub zoom: f32,
    /// Where the window was.
    pub window: WindowMemory,
}

impl Default for DesktopPrefs {
    fn default() -> Self {
        DesktopPrefs {
            close_to_tray: true,
            start_minimized: false,
            zoom: 1.0,
            window: WindowMemory::default(),
        }
    }
}

impl DesktopPrefs {
    /// Repairs values a hand-edited file may carry.
    fn checked(mut self) -> Self {
        self.zoom = nearest_step(self.zoom);
        self
    }
}

/// The step nearest to `zoom`; 1.0 for a value that is not a number.
pub fn nearest_step(zoom: f32) -> f32 {
    if !zoom.is_finite() {
        return 1.0;
    }
    ZOOM_STEPS
        .iter()
        .copied()
        .min_by(|a, b| (a - zoom).abs().total_cmp(&(b - zoom).abs()))
        .unwrap_or(1.0)
}

/// One step up (`+1`) or down (`-1`) from `zoom`, stopping at the ends.
pub fn zoom_step(zoom: f32, direction: i32) -> f32 {
    let at = ZOOM_STEPS
        .iter()
        .position(|s| *s == nearest_step(zoom))
        .unwrap_or(5) as i32;
    let to = (at + direction.signum()).clamp(0, ZOOM_STEPS.len() as i32 - 1);
    ZOOM_STEPS[to as usize]
}

/// `settings.json` in Inlet's config folder.
pub fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("inlet").join("settings.json"))
}

/// Reads the desktop settings from `path`; defaults for a missing or broken file.
pub fn load(path: &Path) -> DesktopPrefs {
    load_section::<DesktopPrefs>(path, "desktop").checked()
}

/// Writes the desktop settings into `path`, keeping the file's other keys.
pub fn save(path: &Path, prefs: &DesktopPrefs) -> std::io::Result<()> {
    save_section(path, "desktop", prefs)
}

/// Reads the settings under `key` from `path`; defaults for a missing or broken file
/// or section.
pub fn load_section<T: DeserializeOwned + Default>(path: &Path, key: &str) -> T {
    let Ok(text) = std::fs::read_to_string(path) else {
        return T::default();
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            log::warn!("settings.json could not be read, using defaults: {err}");
            return T::default();
        }
    };
    value
        .get(key)
        .cloned()
        .and_then(|d| serde_json::from_value::<T>(d).ok())
        .unwrap_or_default()
}

/// Writes `value` under `key` into `path`, keeping the file's other keys, through a
/// temporary file so a crash never leaves half a file.
pub fn save_section<T: Serialize>(path: &Path, key: &str, value: &T) -> std::io::Result<()> {
    let mut root = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    root[key] = serde_json::to_value(value).map_err(std::io::Error::other)?;
    let text = serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("settings path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".settings.json.{}.tmp", std::process::id()));
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::window_state::Rect;
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inlet-desktop-prefs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.json")
    }

    #[test]
    fn zoom_and_window_survive_a_restart_and_other_keys_stay() {
        let path = temp_file("prefs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"appearance": {"theme": "dark"}}"#).unwrap();
        let mut prefs = load(&path);
        assert_eq!(prefs, DesktopPrefs::default());
        prefs.zoom = zoom_step(zoom_step(prefs.zoom, 1), 1);
        prefs.close_to_tray = false;
        prefs.window.remember(
            "monitor-1",
            Rect {
                x: 10.0,
                y: 20.0,
                w: 900.0,
                h: 700.0,
            },
            false,
        );
        save(&path, &prefs).unwrap();

        let again = load(&path);
        assert_eq!(again, prefs);
        assert_eq!(again.zoom, 1.25);
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["appearance"]["theme"], "dark");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn broken_or_odd_values_fall_back() {
        let path = temp_file("broken");
        assert_eq!(load(&path), DesktopPrefs::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), DesktopPrefs::default());
        std::fs::write(
            &path,
            r#"{"desktop": {"zoom": 7.5, "close_to_tray": false}}"#,
        )
        .unwrap();
        let prefs = load(&path);
        assert_eq!(prefs.zoom, 2.0);
        assert!(!prefs.close_to_tray);
        // A broken file is replaced by a whole one on save.
        std::fs::write(&path, "{not json").unwrap();
        save(&path, &prefs).unwrap();
        assert_eq!(load(&path), prefs);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn zoom_steps_stop_at_the_ends() {
        assert_eq!(zoom_step(1.0, 1), 1.1);
        assert_eq!(zoom_step(1.0, -1), 0.9);
        assert_eq!(zoom_step(2.0, 1), 2.0);
        assert_eq!(zoom_step(0.5, -1), 0.5);
        assert_eq!(nearest_step(f32::NAN), 1.0);
        assert_eq!(nearest_step(1.12), 1.1);
    }
}
