// SPDX-License-Identifier: AGPL-3.0-or-later
//! Messages with files on their way: a row per upload in the open channel, above the
//! box, with the file names, how much is sent, a progress bar and Cancel. The rows
//! follow the backend's upload events; the message list shows the message itself as
//! pending (and Retry if it fails).
//!
//! Deviation: Fluxer draws the progress inside the pending message; Inlet draws it in
//! the composer, which owns uploads, and the pending message lists the file names.

use gpui::{div, prelude::*, px, relative, AnyElement, Context, FontWeight, SharedString};

use super::editor::{ComposerEvent, ComposerView};
use crate::theme;

/// Most upload rows kept (older ones go first).
const MAX_ROWS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRow {
    pub nonce: String,
    pub channel_id: String,
    pub names: Vec<String>,
    pub sent: u64,
    pub total: u64,
}

/// The uploads the backend reported as running.
#[derive(Default)]
pub struct UploadQueue {
    rows: Vec<UploadRow>,
}

impl UploadQueue {
    pub fn started(&mut self, nonce: String, channel_id: String, names: Vec<String>, total: u64) {
        self.rows.retain(|r| r.nonce != nonce);
        if self.rows.len() >= MAX_ROWS {
            self.rows.remove(0);
        }
        self.rows.push(UploadRow {
            nonce,
            channel_id,
            names,
            sent: 0,
            total,
        });
    }

    pub fn progress(&mut self, nonce: &str, sent: u64, total: u64) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.nonce == nonce) {
            r.sent = sent.min(total);
            r.total = total;
        }
    }

    /// The upload ended or was cancelled. Whether it was shown.
    pub fn done(&mut self, nonce: &str) -> bool {
        let before = self.rows.len();
        self.rows.retain(|r| r.nonce != nonce);
        self.rows.len() != before
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn of_channel<'a>(&'a self, channel_id: &'a str) -> impl Iterator<Item = &'a UploadRow> {
        self.rows.iter().filter(move |r| r.channel_id == channel_id)
    }
}

/// "a.png", or "a.png and 2 more".
fn label(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => t!(
            "composer-upload-names-more",
            first = first.as_str(),
            more = rest.len()
        ),
    }
}

impl ComposerView {
    pub fn on_upload_started(
        &mut self,
        nonce: String,
        channel_id: String,
        names: Vec<String>,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        self.uploads.started(nonce, channel_id, names, total);
        cx.notify();
    }

    pub fn on_upload_progress(
        &mut self,
        nonce: &str,
        sent: u64,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        self.uploads.progress(nonce, sent, total);
        cx.notify();
    }

    pub fn on_upload_done(&mut self, nonce: &str, cx: &mut Context<Self>) {
        if self.uploads.done(nonce) {
            cx.notify();
        }
    }

    /// Cancel on an upload row: the backend stops it, the row and the message's pending
    /// row go.
    fn cancel_upload(&mut self, nonce: String, cx: &mut Context<Self>) {
        if self.uploads.done(&nonce) {
            cx.emit(ComposerEvent::CancelUpload { nonce });
        }
        cx.notify();
    }

    pub(super) fn render_upload_queue(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let channel = self.channel.as_ref()?;
        let rows: Vec<AnyElement> = self
            .uploads
            .of_channel(&channel.channel_id)
            .map(|r| {
                let nonce = r.nonce.clone();
                let fraction = if r.total == 0 {
                    1.
                } else {
                    (r.sent as f64 / r.total as f64).clamp(0., 1.) as f32
                };
                let size = crate::ui::media::file_size;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::accent_border())
                    .bg(theme::panel_bg())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_strong())
                                    .child(label(&r.names)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_tertiary())
                                    .child(t!(
                                        "composer-upload-progress",
                                        sent = size(r.sent),
                                        total = size(r.total)
                                    )),
                            )
                            .child(
                                div()
                                    .mt(px(6.))
                                    .h(px(4.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(theme::bg_app())
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(fraction))
                                            .rounded_full()
                                            .bg(theme::brand()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("upload-cancel-{nonce}")))
                            .flex_none()
                            .px_2()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(theme::text_tertiary())
                            .hover(|s| s.text_color(theme::text_strong()))
                            .child(t!("composer-upload-cancel"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_upload(nonce.clone(), cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(
            div()
                .mx(px(6.))
                .mb_2()
                .flex()
                .flex_col()
                .gap_2()
                .children(rows)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_follows_the_backend() {
        let mut q = UploadQueue::default();
        q.started("1".into(), "301".into(), vec!["a.png".into()], 100);
        q.started("2".into(), "302".into(), vec!["b.png".into()], 10);
        q.progress("1", 40, 100);
        let rows: Vec<_> = q.of_channel("301").collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sent, 40);
        assert!(q.done("1"));
        assert!(!q.done("1"), "once");
        assert_eq!(q.of_channel("301").count(), 0);
        q.clear();
        assert_eq!(q.of_channel("302").count(), 0);
    }
}
