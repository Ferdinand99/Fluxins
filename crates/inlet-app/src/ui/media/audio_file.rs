// SPDX-License-Identifier: AGPL-3.0-or-later
//! Audio attachments: a file card with the length and a play button that opens the file
//! in the browser (Inlet has no player of its own yet), and the save button.

use gpui::{prelude::*, AnyElement, Context};

use super::attachment::icon_button;
use super::{duration, file_size, MediaIcon};
use crate::app::Inlet;
use crate::model::Attachment;

impl Inlet {
    pub(super) fn audio_file(
        &self,
        key: &str,
        a: &Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = match a.duration_secs {
            Some(secs) => t!(
                "media-audio-detail",
                size = file_size(a.size),
                length = duration(secs)
            ),
            None => file_size(a.size),
        };
        let url = self
            .media
            .setup
            .as_ref()
            .and_then(|s| super::requests::attachment_original(&s.proxy, a))
            .map(String::from);
        let play = url.map(|url| {
            icon_button(format!("audio-play-{key}"), MediaIcon::Play)
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, false, cx)))
                .into_any_element()
        });
        self.file_row(key, a, MediaIcon::FileAudio, detail, play, cx)
    }
}
