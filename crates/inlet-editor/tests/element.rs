// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the gpui side: input method, UTF-16 offsets, layout and scrolling. They run in
//! gpui's test platform, so they need no screen.

use gpui::{div, prelude::*, px, Entity, EntityInputHandler, TestAppContext};
use inlet_editor::{Editor, Id, NoNames, Token, Trigger};

/// A window that holds an editor in a fixed width.
struct Host {
    editor: Entity<Editor>,
    width: f32,
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().w(px(self.width)).child(self.editor.clone())
    }
}

fn host(cx: &mut TestAppContext, width: f32) -> (Entity<Editor>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(|_, cx| Host {
        editor: cx.new(|cx| Editor::new(5000, cx)),
        width,
    });
    let editor = host.read_with(cx, |host, _| host.editor.clone());
    (editor, cx)
}

#[gpui::test]
fn ime_composition_replaces_marked_range(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "ab", window, cx);
            assert_eq!(ed.buffer().text(), "ab");
            assert_eq!(ed.marked_range(), None);

            // The first preedit text is marked.
            ed.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            assert_eq!(ed.buffer().text(), "abn");
            assert_eq!(ed.marked_range(), Some(2..3));
            assert_eq!(ed.marked_text_range(window, cx), Some(2..3));

            // The next one replaces the marked text, and nothing else.
            ed.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
            assert_eq!(ed.buffer().text(), "abに");
            assert_eq!(ed.marked_range(), Some(2..5), "bytes");
            assert_eq!(ed.marked_text_range(window, cx), Some(2..3), "UTF-16");
            assert_eq!(ed.selection().head, 5);

            // The commit replaces the marked text with the final text and ends the mark.
            ed.replace_text_in_range(None, "日本", window, cx);
            assert_eq!(ed.buffer().text(), "ab日本");
            assert_eq!(ed.marked_range(), None);
            assert_eq!(ed.marked_text_range(window, cx), None);
            assert_eq!(ed.selection().head, 8);

            // The whole composition is one undo step.
            assert!(ed.undo_edit(window, cx));
            assert_eq!(ed.buffer().text(), "ab");
            assert!(ed.redo_edit(window, cx));
            assert_eq!(ed.buffer().text(), "ab日本");
        })
        .unwrap();
}

#[gpui::test]
fn ime_preedit_replaces_the_selection(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "abcd", window, cx);
            ed.set_selected_text_range(1..3, window, cx);
            ed.replace_and_mark_text_in_range(None, "x", None, window, cx);
            assert_eq!(ed.buffer().text(), "axd");
            assert_eq!(ed.marked_range(), Some(1..2));
            // Cancelling the composition (empty text) removes the marked text.
            ed.replace_and_mark_text_in_range(None, "", None, window, cx);
            assert_eq!(ed.buffer().text(), "ad");
            assert_eq!(ed.marked_range(), None);
        })
        .unwrap();
}

#[gpui::test]
fn ime_marked_selection_is_relative_to_the_inserted_text(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "A🙂Z", window, cx);
            // UTF-16: A=0..1, 🙂=1..3, Z=3..4. Replace the emoji.
            ed.replace_and_mark_text_in_range(Some(1..3), "é🙂", Some(1..3), window, cx);
            assert_eq!(ed.buffer().text(), "Aé🙂Z");
            assert_eq!(ed.marked_range(), Some(1..7));
            assert_eq!(ed.selection().range(), 3..7);
        })
        .unwrap();
}

#[gpui::test]
fn the_input_method_sees_utf16_offsets(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "e🙂漢", window, cx);
            assert_eq!(ed.text_length_utf16(window, cx), Some(4));
            ed.set_selected_text_range(1..3, window, cx);
            assert_eq!(ed.selection().range(), 1..5);
            assert_eq!(
                ed.selected_text_range(false, window, cx).unwrap().range,
                1..3
            );
            let mut actual = None;
            assert_eq!(
                ed.text_for_range(1..3, &mut actual, window, cx).as_deref(),
                Some("🙂")
            );
            assert_eq!(actual, Some(1..3));
            // An offset past the end stays at the end.
            ed.set_selected_text_range(3..99, window, cx);
            assert_eq!(ed.selection().range(), 5..8);
        })
        .unwrap();
}

#[gpui::test]
fn the_input_method_cannot_select_half_a_pill(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.set_wire("a <@1> b", &NoNames, window, cx);
            assert_eq!(ed.buffer().text(), "a @unknown-user b");
            // The pill is bytes 2..15 (13 characters). UTF-16 4..6 is inside it.
            ed.set_selected_text_range(4..6, window, cx);
            assert_eq!(ed.selection().range(), 2..15);
        })
        .unwrap();
}

#[gpui::test]
fn text_over_the_limit_is_refused_and_leaves_the_text(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(5, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "abc", window, cx);
            assert!(ed.insert_text("defg", window, cx).is_err());
            assert_eq!(ed.buffer().text(), "abc");
            assert!(ed.insert_text("de", window, cx).is_ok());
            assert_eq!(ed.buffer().text(), "abcde");
        })
        .unwrap();
}

#[gpui::test]
fn completing_a_trigger_inserts_a_pill(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.insert_text("hi @al", window, cx).unwrap();
            assert_eq!(
                ed.trigger().map(|t| t.1),
                Some(Trigger::Mention("al".into()))
            );
            assert!(ed.complete_trigger(Token::User(Id(1)), "@alice", window, cx));
            assert_eq!(ed.buffer().text(), "hi @alice");
            assert_eq!(ed.to_wire(), "hi <@1>");
            assert_eq!(ed.selection().head, 9);
            assert!(ed.trigger().is_none());
            assert!(!ed.complete_trigger(Token::Here, "@here", window, cx));

            ed.clear(window, cx);
            assert!(ed.buffer().is_empty());
            assert_eq!(ed.selection().head, 0);
        })
        .unwrap();
}

#[gpui::test]
fn text_wraps_and_positions_map_back_to_offsets(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 120.);
    editor.update_in(cx, |ed, window, cx| {
        ed.insert_text("aaaa bbbb cccc dddd eeee ffff gggg", window, cx)
            .unwrap();
    });
    cx.run_until_parked();
    editor.update_in(cx, |ed, window, cx| {
        let bounds = ed.painted_bounds().expect("the editor was painted");
        assert_eq!(bounds.size.width, px(120.));
        let first = ed.bounds_for_range(0..1, bounds, window, cx).unwrap();
        let last = ed.bounds_for_range(33..34, bounds, window, cx).unwrap();
        assert!(
            last.top() > first.top(),
            "the text wrapped onto a second row"
        );
        assert!(bounds.size.height > first.size.height);
        assert!(bounds.size.height >= last.bottom() - bounds.top());

        // The centre of a character maps back to its offset.
        for utf16 in [0, 5, 13, 20, 33] {
            let b = ed
                .bounds_for_range(utf16..utf16 + 1, bounds, window, cx)
                .unwrap();
            let found = ed
                .character_index_for_point(b.center(), window, cx)
                .unwrap();
            assert!(
                found == utf16 || found == utf16 + 1,
                "offset {utf16} mapped to {found}"
            );
        }
        // One rectangle per row for a selection over several rows.
        let rects = ed.selection_bounds_for_range(0..34, bounds, window, cx);
        assert!(rects.len() >= 2);
        assert!(rects.iter().all(|r| r.size.width > px(0.)));
    });
}

#[gpui::test]
fn a_pill_takes_one_step_of_the_caret_and_has_its_own_rectangle(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 400.);
    editor.update_in(cx, |ed, window, cx| {
        ed.set_wire("hi <@1> there", &NoNames, window, cx);
    });
    cx.run_until_parked();
    editor.update_in(cx, |ed, window, cx| {
        let bounds = ed.painted_bounds().unwrap();
        // "hi " is 3 characters; the pill label is "@unknown-user" (13).
        let rects = ed.selection_bounds_for_range(3..16, bounds, window, cx);
        assert_eq!(rects.len(), 1);
        let before = ed.bounds_for_range(0..3, bounds, window, cx).unwrap();
        assert!(rects[0].left() >= before.right() - px(0.5));
    });
}

#[gpui::test]
fn the_editor_stops_growing_at_half_the_window_and_scrolls(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 300.);
    let window_height = cx.update(|window, _| window.viewport_size().height);
    editor.update_in(cx, |ed, window, cx| {
        ed.insert_text(&"line\n".repeat(300), window, cx).unwrap();
    });
    cx.run_until_parked();
    editor.update_in(cx, |ed, _, _| {
        let bounds = ed.painted_bounds().unwrap();
        let limit = window_height * 0.5;
        assert!(
            (bounds.size.height - limit).abs() < px(1.),
            "height {:?} should be half of {:?}",
            bounds.size.height,
            window_height
        );
        // The caret is at the end, so the text is scrolled down to it.
        assert!(ed.scroll_offset() > px(0.));
    });
}

#[gpui::test]
fn a_short_text_does_not_scroll(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 300.);
    editor.update_in(cx, |ed, window, cx| {
        ed.insert_text("one\ntwo", window, cx).unwrap();
    });
    cx.run_until_parked();
    editor.update_in(cx, |ed, _, _| {
        assert_eq!(ed.scroll_offset(), px(0.));
        assert!(ed.painted_bounds().unwrap().size.height > px(0.));
    });
}
