// SPDX-License-Identifier: AGPL-3.0-or-later
//! The settings view: a column of tabs on the left, the open tab on the right.
//!
//! Every tab is its own file under `tabs/` and implements [`SettingsTab`]; `tabs::all`
//! lists them. The gear in the user panel opens the view over the whole window; it
//! hides the chat, so nothing is read behind it (see `chat_is_visible`).

pub mod tabs;

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, FontWeight, SharedString, Window};

use crate::app::Inlet;
use crate::backend;
use crate::theme;
use crate::ui::widgets::button;

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

    /// Opens the tab with this ID, if there is one.
    pub fn select(&mut self, id: &str) {
        if let Some(ix) = self.tabs.iter().position(|t| t.id() == id) {
            self.selected = ix;
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
pub struct State {
    /// The open settings view, if any.
    pub view: Option<Entity<SettingsView>>,
}

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

    /// Opens the settings view over the window, at the tab `tab` if given.
    pub(crate) fn open_settings(&mut self, tab: Option<&str>, cx: &mut Context<Self>) {
        let view = cx.new(|_| {
            let mut view = SettingsView::new();
            if let Some(tab) = tab {
                view.select(tab);
            }
            view
        });
        self.settings.view = Some(view);
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.view = None;
        cx.notify();
    }

    /// The settings view over the whole window, with a button to close it.
    pub(crate) fn render_settings_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.settings.view.clone() else {
            return div().into_any_element();
        };
        let overlay = div().id("settings-overlay");
        // Under an open link prompt, the prompt has the keyboard.
        let overlay = if self.overlays.link_prompt.is_none() {
            self.layer_keys(overlay, cx)
        } else {
            overlay
        };
        overlay
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg_chat())
            .child(div().flex_1().min_h_0().child(view))
            .child(
                div().absolute().top(px(16.)).right(px(16.)).child(
                    button("settings-close", t!("settings-close"), false)
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
                ),
            )
            .into_any_element()
    }
}
