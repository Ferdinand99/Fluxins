// SPDX-License-Identifier: AGPL-3.0-or-later
//! The settings tabs, one file each. Empty for now: a task that adds a tab adds its
//! file here and one line to [`all`].

use super::SettingsTab;

/// Every tab, in the order the list shows them.
pub fn all() -> Vec<Box<dyn SettingsTab>> {
    Vec::new()
}
