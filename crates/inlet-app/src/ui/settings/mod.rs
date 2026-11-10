// SPDX-License-Identifier: AGPL-3.0-or-later
//! The settings view: a column of tabs on the left, the open tab on the right.
//!
//! Every tab is its own file under `tabs/` and implements [`SettingsTab`]; `tabs::all`
//! lists them. Nothing opens the view yet: the tasks that add the first tabs also add
//! the way in (the gear in the user panel).

// The frame is in place before its first tab and its entry point.
#![allow(dead_code)]

pub mod tabs;

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString, Window};

use crate::app::Inlet;
use crate::backend;
use crate::theme;

/// One tab of the settings view.
pub trait SettingsTab {
    /// Stable ID, also used for element IDs (`settings-tab-<id>`).
    fn id(&self) -> &'static str;
    /// The tab's name in the list, from the catalogue.
    fn title(&self) -> SharedString;
    /// The tab's content.
    fn render(&mut self, cx: &mut Context<SettingsView>) -> AnyElement;
}

/// The settings view with its tabs.
pub struct SettingsView {
    tabs: Vec<Box<dyn SettingsTab>>,
    selected: usize,
}

impl SettingsView {
    pub fn new() -> Self {
        Self {
            tabs: tabs::all(),
            selected: 0,
        }
    }
}

impl Default for SettingsView {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let list = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let active = ix == selected;
            div()
                .id(SharedString::from(format!("settings-tab-{}", tab.id())))
                .px_2()
                .py_1()
                .rounded(px(6.))
                .cursor_pointer()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if active {
                    theme::text_strong()
                } else {
                    theme::text_channel()
                })
                .when(active, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .child(tab.title())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = ix;
                    cx.notify();
                }))
        });
        let list: Vec<_> = list.collect();
        let content = self
            .tabs
            .get_mut(selected)
            .map(|tab| tab.render(cx))
            .unwrap_or_else(|| div().into_any_element());
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme::bg_chat())
            .child(
                div()
                    .w(px(232.))
                    .h_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p_2()
                    .bg(theme::bg_app())
                    .children(list),
            )
            .child(div().flex_1().h_full().p_8().child(content))
    }
}

/// The settings domain's view state.
#[derive(Default)]
pub struct State {}

impl Inlet {
    /// Events from the backend's settings domain.
    pub(crate) fn on_settings_event(
        &mut self,
        event: backend::settings::Ev,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match event {}
    }
}
