// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo scenes: a fixed state the app opens in, picked with `INLET_DEMO_SCENE` (demo
//! builds only). There is one scene, `default`; the visual checks add one file of data
//! per scene they need.

use std::collections::HashMap;

use super::{guilds, messages, social};
use crate::model::{Channel, Guild, Message};

/// A demo scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// Three guilds, a channel list with long and non-Latin names, messages of every
    /// kind, embeds and two DMs.
    Default,
}

impl Scene {
    pub fn from_name(name: &str) -> Option<Scene> {
        match name {
            "default" => Some(Scene::Default),
            _ => None,
        }
    }
}

/// The scene named in `INLET_DEMO_SCENE`; `default` when it is not set or unknown.
pub fn current() -> Scene {
    match std::env::var("INLET_DEMO_SCENE") {
        Ok(name) => Scene::from_name(name.trim()).unwrap_or_else(|| {
            log::warn!("unknown demo scene {name:?}, showing the default scene");
            Scene::Default
        }),
        Err(_) => Scene::Default,
    }
}

/// What a scene shows.
pub struct Data {
    pub guilds: Vec<Guild>,
    /// Channel lists per guild (`None` = direct messages).
    pub channels: HashMap<Option<String>, Vec<Channel>>,
    /// Messages per channel.
    pub messages: HashMap<String, Vec<Message>>,
}

pub fn data(scene: Scene) -> Data {
    match scene {
        Scene::Default => {
            let mut channels = HashMap::new();
            guilds::channels(&mut channels);
            social::dm_channels(&mut channels);
            let mut messages = HashMap::new();
            messages::messages(&mut messages);
            social::dm_messages(&mut messages);
            Data {
                guilds: guilds::guilds(),
                channels,
                messages,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_default_scene_exists() {
        assert_eq!(Scene::from_name("default"), Some(Scene::Default));
        assert_eq!(Scene::from_name("nope"), None);
        let data = data(Scene::Default);
        assert_eq!(data.guilds.len(), 3);
        assert!(data.channels.contains_key(&None));
        assert!(data.messages.contains_key("c-general"));
        assert!(data.messages.contains_key("dm-ada"));
    }
}
