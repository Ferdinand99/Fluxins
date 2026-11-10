// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending a failed message again ("Retry"), with the nonce it was first sent with.
//! Fluxer's `sendOncePerNonce` drops a second send with the same nonce, but only within
//! 5 minutes, so a copy that reached the server the first time is not posted twice when
//! Retry comes within that time (later retries are the composer's to handle). The row
//! shows "sending" until the REST answer or the gateway's MESSAGE_CREATE with that nonce
//! removes it, or shows the new reason if it fails again. One request per click.

use super::{parse, Ev, Pending};
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal};

impl Backend {
    pub(super) fn resend(&mut self, channel_id: String, nonce: String, text: String) {
        match &mut self.active {
            #[cfg(feature = "demo")]
            Some(Active::Demo(m)) => m.resend(channel_id, nonce, text),
            Some(Active::Real(r)) => {
                let (session, events, internal, gen) = (
                    r.session.clone(),
                    self.events.clone(),
                    self.internal.clone(),
                    r.gen,
                );
                let names = r.names.clone();
                self.spawn_guarded("resend", None, async move {
                    match session
                        .client()
                        .send_message(&channel_id, &text, Some(&nonce))
                        .await
                    {
                        Ok(message) => {
                            let _ = events.unbounded_send(Ev::Sent { nonce });
                            let _ = events.unbounded_send(Ev::MessageCreated(parse::to_message(
                                &message, &names,
                            )));
                        }
                        Err(err) => {
                            let failure = Failure::from(err);
                            if failure.kind == FailureKind::SessionEnded {
                                let _ = internal.send(Internal::Ended {
                                    gen,
                                    message: failure.message,
                                });
                            } else {
                                // The row shows the reason; no error bar on top.
                                let _ = events.unbounded_send(Ev::Pending(Pending {
                                    channel_id,
                                    nonce,
                                    text,
                                    failed: Some(failure.message),
                                }));
                            }
                        }
                    }
                });
            }
            None => {}
        }
    }
}
