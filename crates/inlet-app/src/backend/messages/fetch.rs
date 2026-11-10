// SPDX-License-Identifier: AGPL-3.0-or-later
//! Loading what a view opens: the channels of a guild (or the DM list) and the latest
//! messages of a channel. Opening another aborts the previous load.

use super::parse;
use super::Ev;
use crate::backend::{report, Active, Backend};
use crate::model::{Channel, Message};

impl Backend {
    pub(super) fn open_guild(&mut self, guild_id: Option<String>) {
        if let Some(task) = self.open_guild.take() {
            task.abort();
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.open_guild(guild_id),
            Some(Active::Real(r)) => {
                if let Some(previous) = r.active_guild.take() {
                    r.gateway.set_guild_active(&previous, false);
                }
                if let Some(id) = &guild_id {
                    // Guilds with more than 250 members are passive until marked active.
                    r.gateway.set_guild_active(id, true);
                    r.active_guild = Some(id.clone());
                }
                let cached = r.cache.lock().unwrap().get(&guild_id).cloned();
                let (session, cache, gen) = (r.session.clone(), r.cache.clone(), r.gen);
                let names = r.names.clone();
                let (events, internal) = (self.events.clone(), self.internal.clone());
                if let Some(channels) = cached {
                    self.emit(Ev::Channels { guild_id, channels });
                    return;
                }
                self.open_guild = Some(self.spawn_guarded("open guild", None, async move {
                    let result = match &guild_id {
                        Some(id) => session.client().channels(id).await,
                        None => session.client().dm_channels().await,
                    };
                    match result {
                        Ok(list) => {
                            let channels: Vec<Channel> =
                                list.iter().map(parse::to_channel).collect();
                            parse::remember_channels(&names, &channels);
                            cache
                                .lock()
                                .unwrap()
                                .insert(guild_id.clone(), channels.clone());
                            let _ = events.unbounded_send(Ev::Channels { guild_id, channels });
                        }
                        Err(err) => report(
                            &events,
                            &internal,
                            gen,
                            |e| t!("error-load-channels", error = e),
                            err,
                        ),
                    }
                }));
            }
            None => {}
        }
    }

    pub(super) fn open_channel(&mut self, channel_id: String) {
        if let Some(task) = self.open_channel.take() {
            task.abort();
        }
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.open_channel(channel_id),
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let names = r.names.clone();
                self.open_channel = Some(self.spawn_guarded("open channel", None, async move {
                    match session.client().messages(&channel_id, None, 50).await {
                        Ok(list) => {
                            // The API returns newest first; the view wants oldest first.
                            let mut messages: Vec<Message> =
                                list.iter().map(|m| parse::to_message(m, &names)).collect();
                            messages.sort_by(|a, b| {
                                parse::snowflake_key(&a.id).cmp(&parse::snowflake_key(&b.id))
                            });
                            let _ = events.unbounded_send(Ev::Messages {
                                channel_id,
                                messages,
                            });
                        }
                        Err(err) => report(
                            &events,
                            &internal,
                            gen,
                            |e| t!("error-load-messages", error = e),
                            err,
                        ),
                    }
                }));
            }
            None => {}
        }
    }
}
