// SPDX-License-Identifier: AGPL-3.0-or-later
//! The gpui side of the editor: [`Editor`] is the entity (state, actions, input method) and
//! [`EditorElement`] draws it: wrapped text, selection, caret and pills for tokens.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, AvailableSpace, Bounds,
    ClipboardItem, ContentMask, Context, Corners, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    GlobalElementId, Hsla, InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollEvent, SharedString, Style, TextAlign,
    TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};

use crate::buffer::{Buffer, TooLong};
use crate::selection::{self, Move, Selection};
use crate::tokens::{Resolve, Token, TokenSpan};
use crate::trigger::{trigger_at, Trigger};

actions!(
    inlet_editor,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        Home,
        End,
        DocStart,
        DocEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectHome,
        SelectEnd,
        SelectDocStart,
        SelectDocEnd,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Newline,
        Submit,
    ]
);

/// The key context of the editor. Bind your own keys against it.
pub const KEY_CONTEXT: &str = "InletEditor";

/// Binds the default keys. Enter submits, Shift+Enter inserts a line break.
pub fn bind_keys(cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    let mut bindings = Vec::new();
    macro_rules! bind {
        ($key:expr, $action:expr) => {{
            let key: &'static str = $key;
            bindings.push(KeyBinding::new(key, $action, Some(KEY_CONTEXT)));
        }};
    }
    // A key that is Command on macOS and Control elsewhere.
    macro_rules! bind_cmd {
        ($mac:literal, $other:literal, $action:expr) => {
            bind!(if mac { $mac } else { $other }, $action)
        };
    }
    bind!("backspace", Backspace);
    bind!("delete", Delete);
    bind!("left", Left);
    bind!("right", Right);
    bind!("up", Up);
    bind!("down", Down);
    bind!("shift-left", SelectLeft);
    bind!("shift-right", SelectRight);
    bind!("shift-up", SelectUp);
    bind!("shift-down", SelectDown);
    bind!("enter", Submit);
    bind!("shift-enter", Newline);
    // Moving by word uses Alt on macOS and Control elsewhere.
    bind_cmd!("alt-backspace", "ctrl-backspace", DeleteWordLeft);
    bind_cmd!("alt-delete", "ctrl-delete", DeleteWordRight);
    bind_cmd!("alt-left", "ctrl-left", WordLeft);
    bind_cmd!("alt-right", "ctrl-right", WordRight);
    bind_cmd!("shift-alt-left", "shift-ctrl-left", SelectWordLeft);
    bind_cmd!("shift-alt-right", "shift-ctrl-right", SelectWordRight);
    bind_cmd!("cmd-a", "ctrl-a", SelectAll);
    bind_cmd!("cmd-c", "ctrl-c", Copy);
    bind_cmd!("cmd-x", "ctrl-x", Cut);
    bind_cmd!("cmd-v", "ctrl-v", Paste);
    bind_cmd!("cmd-z", "ctrl-z", Undo);
    if mac {
        bind!("cmd-shift-z", Redo);
        bind!("cmd-left", Home);
        bind!("cmd-right", End);
        bind!("cmd-up", DocStart);
        bind!("cmd-down", DocEnd);
        bind!("shift-cmd-left", SelectHome);
        bind!("shift-cmd-right", SelectEnd);
        bind!("shift-cmd-up", SelectDocStart);
        bind!("shift-cmd-down", SelectDocEnd);
    } else {
        bind!("ctrl-y", Redo);
        bind!("ctrl-shift-z", Redo);
        bind!("home", Home);
        bind!("end", End);
        bind!("ctrl-home", DocStart);
        bind!("ctrl-end", DocEnd);
        bind!("shift-home", SelectHome);
        bind!("shift-end", SelectEnd);
        bind!("ctrl-shift-home", SelectDocStart);
        bind!("ctrl-shift-end", SelectDocEnd);
    }
    cx.bind_keys(bindings);
}

/// Colors and sizes of the pieces the editor draws. The app fills these from its theme.
#[derive(Clone, Debug)]
pub struct EditorStyle {
    pub text: Hsla,
    pub placeholder: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
    pub pill_background: Hsla,
    pub pill_text: Hsla,
    pub pill_radius: Pixels,
    /// The editor grows with its text up to this share of the window height, then scrolls.
    pub max_height_fraction: f32,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            text: gpui::hsla(0., 0., 0.93, 1.),
            placeholder: gpui::hsla(0., 0., 0.55, 1.),
            selection: gpui::hsla(0.6, 0.7, 0.5, 0.35),
            caret: gpui::hsla(0., 0., 0.93, 1.),
            pill_background: gpui::hsla(0.63, 0.5, 0.45, 0.3),
            pill_text: gpui::hsla(0.63, 0.9, 0.85, 1.),
            pill_radius: px(4.),
            max_height_fraction: 0.5,
        }
    }
}

/// What the editor tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// The text changed.
    Changed,
    /// The caret or selection moved.
    SelectionChanged,
    /// Enter was pressed.
    Submit,
}

/// The multi-line text field of the composer.
pub struct Editor {
    focus_handle: FocusHandle,
    buffer: Buffer,
    selection: Selection,
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    style: EditorStyle,
    last: Option<Painted>,
    scroll_y: Pixels,
    /// The x the caret keeps while moving up and down.
    preferred_x: Option<Pixels>,
    selecting: bool,
    autoscroll: bool,
}

impl EventEmitter<EditorEvent> for Editor {}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Editor {
    pub fn new(max_chars: usize, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: Buffer::new(max_chars),
            selection: Selection::default(),
            marked: None,
            placeholder: SharedString::default(),
            style: EditorStyle::default(),
            last: None,
            scroll_y: Pixels::ZERO,
            preferred_x: None,
            selecting: false,
            autoscroll: false,
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// The range of the text an input method is composing, if any.
    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Where the editor was drawn last, in window coordinates. `None` before the first paint.
    pub fn painted_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last.as_ref().map(|p| p.bounds)
    }

    /// How far the text is scrolled down, when it is taller than the editor.
    pub fn scroll_offset(&self) -> Pixels {
        self.scroll_y
    }

    /// The text to send.
    pub fn to_wire(&self) -> String {
        self.buffer.to_wire()
    }

    /// The autocomplete trigger at the caret, if any.
    pub fn trigger(&self) -> Option<(Range<usize>, Trigger)> {
        if !self.selection.is_empty() {
            return None;
        }
        trigger_at(&self.buffer, self.selection.head)
    }

    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = text.into();
        cx.notify();
    }

    pub fn set_style(&mut self, style: EditorStyle, cx: &mut Context<Self>) {
        self.style = style;
        cx.notify();
    }

    /// Replaces everything with wire text (to edit a message). Undo history starts over.
    pub fn set_wire(
        &mut self,
        wire: &str,
        names: &dyn Resolve,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer = Buffer::from_wire(wire, names).with_max_chars(self.buffer.max_chars());
        self.marked = None;
        self.selection = Selection::caret(self.buffer.len());
        self.after_edit(window, cx);
    }

    /// Empties the editor, for example after a message was sent.
    pub fn clear(&mut self, window: &Window, cx: &mut Context<Self>) {
        let max = self.buffer.max_chars();
        self.buffer = Buffer::new(max);
        self.marked = None;
        self.selection = Selection::default();
        self.scroll_y = Pixels::ZERO;
        self.after_edit(window, cx);
    }

    /// Inserts text at the caret, replacing the selection.
    pub fn insert_text(
        &mut self,
        text: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Result<(), TooLong> {
        let result = self.buffer.replace(self.selection.range(), text);
        let inserted = result?;
        self.selection = Selection::caret(inserted.end);
        self.after_edit(window, cx);
        Ok(())
    }

    /// Replaces the text of the trigger at the caret with a token. Returns false when there
    /// is no trigger or the token does not fit.
    pub fn complete_trigger(
        &mut self,
        token: Token,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((range, _)) = self.trigger() else {
            return false;
        };
        if !self.buffer.replace_with_token(range, token, label) {
            return false;
        }
        self.selection = Selection::caret(self.buffer.cursor_hint());
        self.after_edit(window, cx);
        true
    }

    /// Undoes the last edit. Returns false when there is nothing to undo.
    pub fn undo_edit(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let done = self.buffer.undo();
        if done {
            self.marked = None;
            self.selection = Selection::caret(self.buffer.cursor_hint());
            self.after_edit(window, cx);
        }
        done
    }

    /// Redoes the last undone edit. Returns false when there is nothing to redo.
    pub fn redo_edit(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let done = self.buffer.redo();
        if done {
            self.marked = None;
            self.selection = Selection::caret(self.buffer.cursor_hint());
            self.after_edit(window, cx);
        }
        done
    }

    // --- state changes -------------------------------------------------------------------

    fn after_edit(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.autoscroll = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(EditorEvent::Changed);
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    fn set_selection(&mut self, selection: Selection, window: &Window, cx: &mut Context<Self>) {
        self.selection = selection.normalized(&self.buffer);
        self.buffer.break_undo_group();
        self.autoscroll = true;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    fn move_caret(
        &mut self,
        movement: Move,
        extend: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.preferred_x = None;
        let next = self.selection.moved(&self.buffer, movement, extend);
        self.set_selection(next, window, cx);
    }

    fn move_vertical(&mut self, rows: i32, extend: bool, window: &Window, cx: &mut Context<Self>) {
        let Some(painted) = &self.last else {
            return;
        };
        let layout = &painted.layout;
        let caret = layout.position_for_index(self.selection.head);
        let x = self.preferred_x.unwrap_or(caret.x);
        let y = caret.y + layout.line_height * (rows as f32 + 0.5);
        let target = if y < Pixels::ZERO {
            0
        } else if y >= layout.height {
            self.buffer.len()
        } else {
            layout.index_for_point(point(x, y), self.buffer.len())
        };
        let next = self.selection.with_head(&self.buffer, target, extend);
        self.set_selection(next, window, cx);
        self.preferred_x = Some(x);
    }

    /// Home and End: the start or end of the visual row the caret is on.
    fn move_to_row_edge(
        &mut self,
        end: bool,
        extend: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let target = match &self.last {
            Some(painted) => {
                let (start, stop) = painted.layout.row_of(self.selection.head);
                if end {
                    stop
                } else {
                    start
                }
            }
            None if end => selection::line_end(&self.buffer, self.selection.head),
            None => selection::line_start(&self.buffer, self.selection.head),
        };
        self.preferred_x = None;
        let next = self.selection.with_head(&self.buffer, target, extend);
        self.set_selection(next, window, cx);
    }

    /// Deletes the selection, or the range from the caret to `edge` when nothing is selected.
    fn delete_to(&mut self, edge: usize, window: &Window, cx: &mut Context<Self>) {
        let range = if self.selection.is_empty() {
            let head = self.selection.head;
            head.min(edge)..head.max(edge)
        } else {
            self.selection.range()
        };
        if range.is_empty() {
            window.play_system_bell();
            return;
        }
        self.buffer.delete(range);
        self.marked = None;
        self.selection = Selection::caret(self.buffer.cursor_hint());
        self.after_edit(window, cx);
    }

    fn selected_text(&self) -> Option<String> {
        let range = self.selection.range();
        (!range.is_empty()).then(|| self.buffer.text()[range].to_owned())
    }

    // --- action handlers -----------------------------------------------------------------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let edge = selection::prev_grapheme(&self.buffer, self.selection.head);
        self.delete_to(edge, window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let edge = selection::next_grapheme(&self.buffer, self.selection.head);
        self.delete_to(edge, window, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edge = selection::prev_word(&self.buffer, self.selection.head);
        self.delete_to(edge, window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edge = selection::next_word(&self.buffer, self.selection.head);
        self.delete_to(edge, window, cx);
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::Left, false, window, cx);
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::Right, false, window, cx);
    }

    fn up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, false, window, cx);
    }

    fn down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, false, window, cx);
    }

    fn word_left(&mut self, _: &WordLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::WordLeft, false, window, cx);
    }

    fn word_right(&mut self, _: &WordRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::WordRight, false, window, cx);
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_row_edge(false, false, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_row_edge(true, false, window, cx);
    }

    fn doc_start(&mut self, _: &DocStart, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::DocStart, false, window, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::DocEnd, false, window, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::Left, true, window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::Right, true, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, true, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, true, window, cx);
    }

    fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_caret(Move::WordLeft, true, window, cx);
    }

    fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_caret(Move::WordRight, true, window, cx);
    }

    fn select_home(&mut self, _: &SelectHome, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_row_edge(false, true, window, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_row_edge(true, true, window, cx);
    }

    fn select_doc_start(
        &mut self,
        _: &SelectDocStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_caret(Move::DocStart, true, window, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(Move::DocEnd, true, window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.set_selection(Selection::select_all(&self.buffer), window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            let edge = self.selection.head;
            self.delete_to(edge, window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            if self.insert_text(&text, window, cx).is_err() {
                window.play_system_bell();
            }
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.undo_edit(window, cx);
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.redo_edit(window, cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.insert_text("\n", window, cx).is_err() {
            window.play_system_bell();
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Submit);
    }

    // --- mouse ---------------------------------------------------------------------------

    fn index_for_mouse(&self, position: Point<Pixels>) -> usize {
        let Some(painted) = &self.last else {
            return 0;
        };
        let local = point(
            position.x - painted.bounds.left(),
            position.y - painted.bounds.top() + painted.scroll_y,
        );
        painted.layout.index_for_point(local, self.buffer.len())
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.request_virtual_keyboard();
        let index = self.index_for_mouse(event.position);
        self.preferred_x = None;
        match event.click_count {
            0 | 1 => {
                self.selecting = true;
                let next = self
                    .selection
                    .with_head(&self.buffer, index, event.modifiers.shift);
                self.set_selection(next, window, cx);
            }
            2 => {
                let r = selection::word_range_at(&self.buffer, index);
                self.set_selection(
                    Selection {
                        anchor: r.start,
                        head: r.end,
                    },
                    window,
                    cx,
                );
            }
            _ => {
                let r = selection::line_range_at(&self.buffer, index);
                self.set_selection(
                    Selection {
                        anchor: r.start,
                        head: r.end,
                    },
                    window,
                    cx,
                );
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting {
            let index = self.index_for_mouse(event.position);
            let next = self.selection.with_head(&self.buffer, index, true);
            self.set_selection(next, window, cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(painted) = &self.last else {
            return;
        };
        let dy = event.delta.pixel_delta(painted.layout.line_height).y;
        let max = painted.max_scroll();
        let next = (self.scroll_y - dy).max(Pixels::ZERO).min(max);
        if next != self.scroll_y {
            self.scroll_y = next;
            cx.notify();
        }
    }

    // --- UTF-16 offsets, which the platform input methods use ----------------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.buffer.text().chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.buffer.text().chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.offset_from_utf16(range.start);
        let end = self.offset_from_utf16(range.end).max(start);
        start..end
    }

    /// The range an input method edit applies to: the one it names, else the marked text,
    /// else the selection.
    fn edit_range(&self, given: Option<&Range<usize>>) -> Range<usize> {
        given
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.range())
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.buffer.text()[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection.range()),
            reversed: self.selection.reversed(),
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let r = self.range_from_utf16(&range_utf16);
        self.set_selection(
            Selection {
                anchor: r.start,
                head: r.end,
            },
            window,
            cx,
        );
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.buffer.text().encode_utf16().count())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.buffer.end_composition();
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composing = self.marked.is_some();
        let range = self.edit_range(range_utf16.as_ref());
        let result = if composing {
            self.buffer.replace_composing(range, new_text)
        } else {
            self.buffer.replace(range, new_text)
        };
        if composing {
            self.marked = None;
            self.buffer.end_composition();
        }
        match result {
            Ok(inserted) => {
                self.selection = Selection::caret(inserted.end);
                self.after_edit(window, cx);
            }
            Err(_) => window.play_system_bell(),
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.edit_range(range_utf16.as_ref());
        match self.buffer.replace_composing(range, new_text) {
            Ok(inserted) => {
                self.marked = (!inserted.is_empty()).then(|| inserted.clone());
                if self.marked.is_none() {
                    self.buffer.end_composition();
                }
                let start16 = self.offset_to_utf16(inserted.start);
                let inserted16 = self.buffer.text()[inserted.clone()].encode_utf16().count();
                let selected = new_selected_range_utf16
                    .map(|s| {
                        self.range_from_utf16(
                            &(start16 + s.start.min(inserted16)..start16 + s.end.min(inserted16)),
                        )
                    })
                    .unwrap_or(inserted.end..inserted.end);
                self.selection = Selection {
                    anchor: selected.start,
                    head: selected.end,
                };
                self.after_edit(window, cx);
            }
            Err(_) => window.play_system_bell(),
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let painted = self.last.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = painted.layout.position_for_index(range.start);
        let end = painted.layout.position_for_index(range.end);
        let width = if end.y == start.y {
            (end.x - start.x).max(px(1.))
        } else {
            px(1.)
        };
        Some(Bounds::new(
            point(
                element_bounds.left() + start.x,
                element_bounds.top() + start.y - painted.scroll_y,
            ),
            size(width, painted.layout.line_height),
        ))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        let Some(painted) = self.last.as_ref() else {
            return Vec::new();
        };
        let range = self.range_from_utf16(&range_utf16);
        painted
            .layout
            .rects_for_range(range)
            .into_iter()
            .map(|r| {
                Bounds::new(
                    point(
                        element_bounds.left() + r.origin.x,
                        element_bounds.top() + r.origin.y - painted.scroll_y,
                    ),
                    r.size,
                )
            })
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let painted = self.last.as_ref()?;
        painted.bounds.localize(&position)?;
        let index = self.index_for_mouse(position);
        Some(self.offset_to_utf16(index))
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll(cx.listener(Self::on_scroll))
            .w_full()
            .child(EditorElement::new(cx.entity()))
    }
}

// --- layout -----------------------------------------------------------------------------

/// The font and line height the text is shaped with. Taken when the element is laid out.
#[derive(Clone)]
pub struct Metrics {
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
}

/// One line of text (between newlines), shaped and wrapped.
struct Line {
    /// Byte offset of the line in the display text.
    start: usize,
    wrapped: WrappedLine,
    /// Top of the line, from the top of the text.
    y: Pixels,
    height: Pixels,
    /// The byte ranges of the visual rows, local to the line.
    rows: Vec<Range<usize>>,
}

/// The shaped text. Positions are from the top left of the text, before scrolling.
struct Layout {
    lines: Vec<Line>,
    line_height: Pixels,
    height: Pixels,
    /// The layout is of the placeholder, not of the text.
    placeholder: bool,
}

impl Layout {
    fn shape(
        text: &str,
        runs: &[TextRun],
        placeholder: bool,
        metrics: &Metrics,
        width: Option<Pixels>,
        window: &Window,
    ) -> Layout {
        let line_height = metrics.line_height;
        let wrap_width = width.map(|w| w.max(px(1.)));
        let shaped = window
            .text_system()
            .shape_text(
                SharedString::from(text.to_owned()),
                metrics.font_size,
                runs,
                wrap_width,
                None,
            )
            .unwrap_or_default();
        let mut lines = Vec::with_capacity(shaped.len());
        let mut start = 0;
        let mut y = Pixels::ZERO;
        for wrapped in shaped {
            let len = wrapped.len();
            let height = wrapped.size(line_height).height;
            let rows = rows_of(&wrapped);
            lines.push(Line {
                start,
                wrapped,
                y,
                height,
                rows,
            });
            start += len + 1;
            y += height;
        }
        let height = y.max(line_height);
        Layout {
            lines,
            line_height,
            height,
            placeholder,
        }
    }

    fn line_ix(&self, index: usize) -> usize {
        self.lines
            .partition_point(|l| l.start <= index)
            .saturating_sub(1)
    }

    fn position_for_index(&self, index: usize) -> Point<Pixels> {
        let Some(line) = self.lines.get(self.line_ix(index)) else {
            return point(Pixels::ZERO, Pixels::ZERO);
        };
        let local = (index - line.start.min(index)).min(line.wrapped.len());
        let p = line
            .wrapped
            .position_for_index(local, self.line_height)
            .unwrap_or_default();
        point(p.x, line.y + p.y)
    }

    /// The offset nearest a point. `len` is the length of the real text.
    fn index_for_point(&self, p: Point<Pixels>, len: usize) -> usize {
        if self.placeholder || self.lines.is_empty() {
            return 0;
        }
        let ix = self
            .lines
            .partition_point(|l| l.y + l.height <= p.y)
            .min(self.lines.len() - 1);
        let line = &self.lines[ix];
        let y = (p.y - line.y).max(Pixels::ZERO).min(line.height - px(1.));
        let local = line
            .wrapped
            .closest_index_for_position(point(p.x, y), self.line_height)
            .unwrap_or_else(|nearest| nearest);
        (line.start + local).min(len)
    }

    /// The offsets where the visual row holding `index` starts and ends.
    fn row_of(&self, index: usize) -> (usize, usize) {
        let Some(line) = self.lines.get(self.line_ix(index)) else {
            return (0, 0);
        };
        let local = index - line.start.min(index);
        let row = line
            .rows
            .iter()
            .find(|r| local <= r.end)
            .or(line.rows.last())
            .cloned()
            .unwrap_or(0..0);
        (line.start + row.start, line.start + row.end)
    }

    /// Rectangles that cover a range of text, one per visual row. A range that holds a
    /// line break gets a small stub at the end of the row, as in most editors.
    fn rects_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut rects = Vec::new();
        for (n, line) in self.lines.iter().enumerate() {
            let end = line.start + line.wrapped.len();
            let lo = range.start.max(line.start);
            let hi = range.end.min(end);
            let covers_break = range.end > end && range.start <= end && n + 1 < self.lines.len();
            if lo > hi && !covers_break {
                continue;
            }
            let layout = &line.wrapped.unwrapped_layout;
            for (r, row) in line.rows.iter().enumerate() {
                let a = (lo.max(line.start) - line.start).max(row.start);
                let b = (hi.saturating_sub(line.start)).min(row.end);
                let row_x = layout.x_for_index(row.start);
                let y = line.y + self.line_height * r;
                if a < b && lo < hi {
                    rects.push(Bounds::new(
                        point(layout.x_for_index(a) - row_x, y),
                        size(
                            layout.x_for_index(b) - layout.x_for_index(a),
                            self.line_height,
                        ),
                    ));
                }
                if covers_break && r + 1 == line.rows.len() {
                    let x = layout.x_for_index(row.end) - row_x;
                    rects.push(Bounds::new(point(x, y), size(px(4.), self.line_height)));
                }
            }
        }
        rects
    }
}

/// The byte ranges of the visual rows of a wrapped line.
fn rows_of(wrapped: &WrappedLine) -> Vec<Range<usize>> {
    let len = wrapped.len();
    let mut starts = vec![0];
    for boundary in wrapped.wrap_boundaries() {
        let index = wrapped
            .unwrapped_layout
            .runs
            .get(boundary.run_ix)
            .and_then(|run| run.glyphs.get(boundary.glyph_ix))
            .map(|glyph| glyph.index);
        if let Some(index) = index {
            starts.push(index.clamp(*starts.last().unwrap_or(&0), len));
        }
    }
    let mut rows = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(len);
        rows.push(*start..end);
    }
    rows
}

/// The runs for the text: pill text for tokens, an underline for composing text.
fn build_runs(
    len: usize,
    tokens: &[TokenSpan],
    marked: Option<&Range<usize>>,
    metrics: &Metrics,
    style: &EditorStyle,
) -> Vec<TextRun> {
    let mut cuts = vec![0, len];
    for t in tokens {
        cuts.push(t.range.start.min(len));
        cuts.push(t.range.end.min(len));
    }
    if let Some(m) = marked {
        cuts.push(m.start.min(len));
        cuts.push(m.end.min(len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .map(|w| {
            let (a, b) = (w[0], w[1]);
            let in_token = tokens
                .iter()
                .any(|t| t.range.start <= a && b <= t.range.end);
            let in_marked = marked.is_some_and(|m| m.start <= a && b <= m.end);
            TextRun {
                len: b - a,
                font: metrics.font.clone(),
                color: if in_token {
                    style.pill_text
                } else {
                    style.text
                },
                background_color: None,
                underline: in_marked.then(|| UnderlineStyle {
                    color: Some(style.text),
                    thickness: px(1.),
                    wavy: false,
                }),
                strikethrough: None,
            }
        })
        .collect()
}

/// What the editor remembers of its last paint, for the mouse and the input method.
struct Painted {
    layout: Layout,
    bounds: Bounds<Pixels>,
    scroll_y: Pixels,
}

impl Painted {
    fn max_scroll(&self) -> Pixels {
        (self.layout.height - self.bounds.size.height).max(Pixels::ZERO)
    }
}

/// Draws an [`Editor`].
pub struct EditorElement {
    editor: Entity<Editor>,
}

impl EditorElement {
    pub fn new(editor: Entity<Editor>) -> Self {
        Self { editor }
    }
}

pub struct PrepaintState {
    layout: Option<Layout>,
    metrics: Metrics,
    scroll_y: Pixels,
    pills: Vec<Bounds<Pixels>>,
    selection: Vec<Bounds<Pixels>>,
    caret: Option<Bounds<Pixels>>,
}

impl Editor {
    /// Shapes the text (or the placeholder when there is none) for the given width.
    fn shape(&self, metrics: &Metrics, width: Option<Pixels>, window: &Window) -> Layout {
        let text = self.buffer.text();
        if text.is_empty() {
            let runs = [TextRun {
                len: self.placeholder.len(),
                font: metrics.font.clone(),
                color: self.style.placeholder,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let runs = if self.placeholder.is_empty() {
                &[][..]
            } else {
                &runs[..]
            };
            return Layout::shape(&self.placeholder, runs, true, metrics, width, window);
        }
        let runs = build_runs(
            text.len(),
            self.buffer.tokens(),
            self.marked.as_ref(),
            metrics,
            &self.style,
        );
        Layout::shape(text, &runs, false, metrics, width, window)
    }

    fn max_height(&self, window: &Window) -> Pixels {
        window.viewport_size().height * self.style.max_height_fraction
    }
}

impl Element for EditorElement {
    type RequestLayoutState = Metrics;
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            text_style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let metrics = Metrics {
            font: text_style.font(),
            font_size,
            line_height,
        };

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let editor = self.editor.clone();
        let measure_metrics = metrics.clone();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                });
                let ed = editor.read(cx);
                let layout = ed.shape(&measure_metrics, width, window);
                let height = layout
                    .height
                    .min(ed.max_height(window))
                    .max(measure_metrics.line_height);
                size(width.unwrap_or(layout_width(&layout)), height)
            });
        (layout_id, metrics)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        metrics: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let ed = self.editor.read(cx);
        let layout = ed.shape(metrics, Some(bounds.size.width), window);
        let caret_index = ed.selection.head;
        let selection = ed.selection.range();
        let autoscroll = ed.autoscroll;
        let mut scroll_y = ed.scroll_y;

        // Keep the scroll in range, and the caret in view after an edit or a move.
        let view_height = bounds.size.height;
        let max_scroll = (layout.height - view_height).max(Pixels::ZERO);
        if autoscroll && !layout.placeholder {
            let caret = layout.position_for_index(caret_index);
            if caret.y < scroll_y {
                scroll_y = caret.y;
            } else if caret.y + layout.line_height > scroll_y + view_height {
                scroll_y = caret.y + layout.line_height - view_height;
            }
        }
        scroll_y = scroll_y.max(Pixels::ZERO).min(max_scroll);
        if autoscroll || scroll_y != ed.scroll_y {
            self.editor.update(cx, |ed, _| {
                ed.scroll_y = scroll_y;
                ed.autoscroll = false;
            });
        }

        let origin = point(bounds.left(), bounds.top() - scroll_y);
        let at = |r: Bounds<Pixels>| Bounds::new(origin + r.origin, r.size);
        let mut pills = Vec::new();
        let tokens = self.editor.read(cx).buffer.tokens().to_vec();
        for t in &tokens {
            for r in layout.rects_for_range(t.range.clone()) {
                pills.push(at(Bounds::new(
                    point(r.origin.x - px(2.), r.origin.y + px(1.)),
                    size(r.size.width + px(4.), r.size.height - px(2.)),
                )));
            }
        }
        let selection_rects = if layout.placeholder {
            Vec::new()
        } else {
            layout
                .rects_for_range(selection.clone())
                .into_iter()
                .map(at)
                .collect()
        };
        let caret = selection.is_empty().then(|| {
            let p = layout.position_for_index(caret_index);
            at(Bounds::new(p, size(px(1.5), layout.line_height)))
        });
        PrepaintState {
            layout: Some(layout),
            metrics: metrics.clone(),
            scroll_y,
            pills,
            selection: selection_rects,
            caret,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _metrics: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, style) = {
            let ed = self.editor.read(cx);
            (ed.focus_handle.clone(), ed.style.clone())
        };
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        let Some(layout) = prepaint.layout.take() else {
            return;
        };
        let line_height = prepaint.metrics.line_height;
        let origin = point(bounds.left(), bounds.top() - prepaint.scroll_y);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for pill in &prepaint.pills {
                let mut quad = fill(*pill, style.pill_background);
                quad.corner_radii = Corners::all(style.pill_radius);
                window.paint_quad(quad);
            }
            for rect in &prepaint.selection {
                window.paint_quad(fill(*rect, style.selection));
            }
            for line in &layout.lines {
                let _ = line.wrapped.paint(
                    point(origin.x, origin.y + line.y),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if let Some(caret) = prepaint.caret {
                if focus_handle.is_focused(window) {
                    window.paint_quad(fill(caret, style.caret));
                }
            }
        });
        let scroll_y = prepaint.scroll_y;
        self.editor.update(cx, |ed, _| {
            ed.last = Some(Painted {
                layout,
                bounds,
                scroll_y,
            });
        });
    }
}

fn layout_width(layout: &Layout) -> Pixels {
    layout
        .lines
        .iter()
        .map(|l| l.wrapped.width())
        .fold(Pixels::ZERO, |a, b| a.max(b))
}
