//! The guild list in the sidebar: order and folders, stored in the user
//! settings (`guild_folders`) and shared with every Fluxer client.
//!
//! Read it from [`crate::UserSettings::guild_folders`] (READY,
//! USER_SETTINGS_UPDATE). Guilds outside a folder sit in folders with
//! [`UNCATEGORIZED_FOLDER_ID`], one run of guilds each, in sidebar order, the
//! way Fluxer's client writes them. Write the whole list back with
//! [`crate::Client::set_guild_folders`]. Which folders are open is not here:
//! that is `guild_folders.expanded_folder_ids` in the synced preferences
//! ([`crate::api::preferences`]).

use reqwest::Method;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::models::{GuildFolder, UserSettings};
use crate::Client;

/// The folder ID of guilds that are not in a folder.
pub const UNCATEGORIZED_FOLDER_ID: i64 = -1;
/// Most folders Fluxer stores.
pub const MAX_FOLDERS: usize = 100;
/// Most guild IDs in one folder entry (Fluxer's own cap is the account's guild limit).
pub const MAX_FOLDER_GUILDS: usize = 10_000;

/// Values of [`GuildFolder::flags`].
pub mod guild_folder_flags {
    /// Show the folder icon instead of guild previews when collapsed.
    pub const SHOW_ICON_WHEN_COLLAPSED: u64 = 1 << 0;
}

/// Values of [`GuildFolder::icon`].
pub mod folder_icon {
    /// The default.
    pub const FOLDER: &str = "folder";
    /// Star.
    pub const STAR: &str = "star";
    /// Heart.
    pub const HEART: &str = "heart";
    /// Bookmark.
    pub const BOOKMARK: &str = "bookmark";
    /// Game controller.
    pub const GAME_CONTROLLER: &str = "game_controller";
    /// Shield.
    pub const SHIELD: &str = "shield";
    /// Music note.
    pub const MUSIC_NOTE: &str = "music_note";
    /// Every value.
    pub const ALL: &[&str] = &[
        FOLDER,
        STAR,
        HEART,
        BOOKMARK,
        GAME_CONTROLLER,
        SHIELD,
        MUSIC_NOTE,
    ];
}

impl GuildFolder {
    /// A folder with a name and guilds, default icon and colour.
    pub fn new(id: i64, name: Option<&str>, guild_ids: Vec<String>) -> Self {
        Self {
            id: Some(id),
            name: name.map(str::to_owned),
            color: None,
            flags: 0,
            icon: folder_icon::FOLDER.to_owned(),
            guild_ids,
        }
    }

    /// Guilds outside any folder, in sidebar order.
    pub fn uncategorized(guild_ids: Vec<String>) -> Self {
        Self::new(UNCATEGORIZED_FOLDER_ID, None, guild_ids)
    }

    /// Whether this entry holds guilds outside any folder.
    pub fn is_uncategorized(&self) -> bool {
        self.id.is_none_or(|id| id == UNCATEGORIZED_FOLDER_ID)
    }

    fn to_json(&self) -> Result<Value> {
        let id = self.id.unwrap_or(UNCATEGORIZED_FOLDER_ID);
        if id < UNCATEGORIZED_FOLDER_ID || id > i64::from(i32::MAX) {
            return Err(Error::InvalidInput("folder ID out of range".into()));
        }
        let name = self.name.as_deref().filter(|n| !n.trim().is_empty());
        if let Some(n) = name {
            crate::api::text(n, 100, "folder name")?;
        }
        if self.color.is_some_and(|c| c > 0xFF_FFFF) {
            return Err(Error::InvalidInput("folder colour: 24-bit RGB".into()));
        }
        if self.flags > u64::from(u32::MAX >> 1) {
            return Err(Error::InvalidInput("folder flags out of range".into()));
        }
        let icon = if self.icon.is_empty() {
            folder_icon::FOLDER
        } else {
            self.icon.as_str()
        };
        if !folder_icon::ALL.contains(&icon) {
            return Err(Error::InvalidInput(format!("unknown folder icon {icon}")));
        }
        crate::api::id_list(&self.guild_ids, MAX_FOLDER_GUILDS, "folder guilds")?;
        Ok(json!({
            "id": id,
            "name": name,
            "color": self.color.unwrap_or(0),
            "flags": self.flags,
            "icon": icon,
            "guild_ids": self.guild_ids,
        }))
    }
}

/// The guild IDs in sidebar order, folders flattened.
pub fn guild_order(folders: &[GuildFolder]) -> Vec<&str> {
    folders
        .iter()
        .flat_map(|f| f.guild_ids.iter().map(String::as_str))
        .collect()
}

impl Client {
    /// `PATCH /users/@me/settings` with `guild_folders`: replaces the
    /// sidebar order and folders (at most 100 entries; every field is checked
    /// before the request). Returns the new settings; USER_SETTINGS_UPDATE
    /// follows on the gateway.
    pub async fn set_guild_folders(&self, folders: &[GuildFolder]) -> Result<UserSettings> {
        if folders.len() > MAX_FOLDERS {
            return Err(Error::InvalidInput(format!(
                "at most {MAX_FOLDERS} guild folders"
            )));
        }
        let list = folders
            .iter()
            .map(GuildFolder::to_json)
            .collect::<Result<Vec<_>>>()?;
        self.raw(
            Method::PATCH,
            "/users/@me/settings",
            Some(&json!({ "guild_folders": list })),
        )
        .await?
        .json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_serialize_like_fluxer_and_are_checked() {
        let mut games = GuildFolder::new(3, Some("Games"), vec!["11".into(), "12".into()]);
        games.color = Some(0x3BA55C);
        games.icon = folder_icon::GAME_CONTROLLER.into();
        let loose = GuildFolder::uncategorized(vec!["10".into()]);
        assert_eq!(
            loose.to_json().unwrap(),
            json!({"id": -1, "name": null, "color": 0, "flags": 0, "icon": "folder",
                   "guild_ids": ["10"]})
        );
        assert_eq!(games.to_json().unwrap()["color"], json!(0x3BA55C));
        assert_eq!(
            guild_order(&[loose.clone(), games.clone()]),
            ["10", "11", "12"]
        );
        assert!(loose.is_uncategorized() && !games.is_uncategorized());

        let mut bad = games.clone();
        bad.icon = "skull".into();
        assert!(bad.to_json().is_err());
        let mut bad = games.clone();
        bad.guild_ids.push("../1".into());
        assert!(bad.to_json().is_err());
        let mut bad = games;
        bad.id = Some(-2);
        assert!(bad.to_json().is_err());
    }
}
