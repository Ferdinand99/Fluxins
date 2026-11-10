// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending a message.

use inlet_protocol as p;

use crate::backend::messages::{self, parse};
use crate::backend::{report, Active, Backend};

impl Backend {
    pub(super) fn send(&mut self, channel_id: String, text: String) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.send(channel_id, text),
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let names = r.names.clone();
                self.spawn_guarded("send", None, async move {
                    let nonce = p::new_nonce();
                    match session
                        .client()
                        .send_message(&channel_id, &text, Some(&nonce))
                        .await
                    {
                        Ok(message) => {
                            let _ = events.unbounded_send(messages::Ev::MessageCreated(
                                parse::to_message(&message, &names),
                            ));
                        }
                        Err(err) => report(
                            &events,
                            &internal,
                            gen,
                            |e| t!("error-send", error = e),
                            err,
                        ),
                    }
                });
            }
            None => {}
        }
    }
}
