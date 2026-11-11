// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the gpui side: input method, UTF-16 offsets, layout and scrolling. They run in
//! gpui's test platform, so they need no screen.

use gpui::{div, prelude::*, px, Entity, EntityInputHandler, Focusable, TestAppContext};
use inlet_editor::{Editor, EditorEvent, Id, NoNames, Submit, Token, Trigger};

/// A window that holds an editor in a fixed width.
struct Host {
    editor: Entity<Editor>,
    width: f32,
    submits: usize,
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().w(px(self.width)).child(self.editor.clone())
    }
}

fn host(cx: &mut TestAppContext, width: f32) -> (Entity<Editor>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(|_, cx| {
        let editor = cx.new(|cx| Editor::new(5000, cx));
        cx.subscribe(&editor, |host: &mut Host, _, event: &EditorEvent, _| {
            if *event == EditorEvent::Submit {
                host.submits += 1;
            }
        })
        .detach();
        Host {
            editor,
            width,
            submits: 0,
        }
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

#[gpui::test]
fn a_composition_that_ends_too_long_is_taken_back(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(5, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.replace_text_in_range(None, "abc", window, cx);
            // The input method may show more than the limit while it composes.
            ed.replace_and_mark_text_in_range(None, "defgh", None, window, cx);
            assert_eq!(ed.buffer().text(), "abcdefgh");
            assert_eq!(ed.marked_range(), Some(3..8));
            // Undo is refused while the input method owns the text.
            assert!(!ed.undo_edit(window, cx));
            // The commit is checked against the limit, and refused as a whole.
            ed.replace_text_in_range(None, "defgh", window, cx);
            assert_eq!(ed.buffer().text(), "abc");
            assert_eq!(ed.marked_range(), None);
            assert_eq!(ed.selection().head, 3);
            // A short composition is committed as usual.
            ed.replace_and_mark_text_in_range(None, "d", None, window, cx);
            ed.replace_text_in_range(None, "de", window, cx);
            assert_eq!(ed.buffer().text(), "abcde");
            assert!(!ed.buffer().is_over_limit());
        })
        .unwrap();
}

fn submits(cx: &mut gpui::VisualTestContext) -> usize {
    cx.update(|window, cx| {
        let root = window.root::<Host>().flatten().unwrap();
        root.read(cx).submits
    })
}

#[gpui::test]
fn submit_ends_an_open_composition_before_it_sends(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 300.);
    editor.update_in(cx, |ed, window, cx| {
        window.focus(&ed.focus_handle(cx), cx);
        ed.replace_text_in_range(None, "ab", window, cx);
        ed.replace_and_mark_text_in_range(None, "\u{306b}", None, window, cx);
    });
    cx.dispatch_action(Submit);
    editor.update_in(cx, |ed, _, _| {
        assert_eq!(ed.marked_range(), None, "the composition was ended");
        assert_eq!(ed.buffer().text(), "ab\u{306b}");
    });
    assert_eq!(submits(cx), 0, "nothing was sent yet");
    cx.dispatch_action(Submit);
    assert_eq!(submits(cx), 1);
}

#[gpui::test]
fn copy_and_paste_keep_pills_inside_the_app(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 300.);
    editor.update_in(cx, |ed, window, cx| {
        window.focus(&ed.focus_handle(cx), cx);
        ed.set_wire("hi <@1> there", &NoNames, window, cx);
    });
    editor.update_in(cx, |ed, window, cx| {
        ed.set_selected_text_range(0..22, window, cx);
    });
    cx.dispatch_action(inlet_editor::actions::Copy);
    let item = cx.read_from_clipboard().expect("copied");
    assert_eq!(item.text().as_deref(), Some("hi @unknown-user there"));
    assert!(item.metadata().is_some(), "the pills are in the metadata");
    editor.update_in(cx, |ed, window, cx| {
        ed.clear(window, cx);
    });
    cx.dispatch_action(inlet_editor::actions::Paste);
    editor.update_in(cx, |ed, _, _| {
        assert_eq!(ed.to_wire(), "hi <@1> there");
        assert_eq!(ed.buffer().tokens().len(), 1);
    });
    // Another app puts plain text with the same words on the clipboard: no pill.
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
        "hi @unknown-user <@5> there".into(),
    ));
    editor.update_in(cx, |ed, window, cx| ed.clear(window, cx));
    cx.dispatch_action(inlet_editor::actions::Paste);
    editor.update_in(cx, |ed, _, _| {
        assert!(ed.buffer().tokens().is_empty());
        assert_eq!(ed.to_wire(), "hi @unknown-user \\<@5> there");
    });
}

#[gpui::test]
fn home_and_end_stay_on_the_row_at_a_soft_wrap(cx: &mut TestAppContext) {
    let (editor, cx) = host(cx, 120.);
    editor.update_in(cx, |ed, window, cx| {
        window.focus(&ed.focus_handle(cx), cx);
        ed.insert_text("aaaa bbbb cccc dddd eeee ffff gggg", window, cx)
            .unwrap();
    });
    cx.run_until_parked();
    // The caret is at the end of the text, on the last row. Home goes to the start of it.
    cx.dispatch_action(inlet_editor::actions::Home);
    let last_row = editor.update_in(cx, |ed, _, _| ed.selection().head);
    assert!(
        last_row > 0 && last_row < 34,
        "start of a later row: {last_row}"
    );
    // Up goes one row up. Home goes to the start of that row, and not further back.
    cx.dispatch_action(inlet_editor::actions::Up);
    cx.dispatch_action(inlet_editor::actions::Home);
    let row_above = editor.update_in(cx, |ed, _, _| ed.selection().head);
    assert!(row_above < last_row);
    // End goes to the end of that row. Home from there stays on the same row, even though
    // the end of a row is also the start of the next one.
    cx.dispatch_action(inlet_editor::actions::End);
    cx.dispatch_action(inlet_editor::actions::Home);
    assert_eq!(
        editor.update_in(cx, |ed, _, _| ed.selection().head),
        row_above
    );
    // Down onto the row below lands at its start: Home stays there.
    cx.dispatch_action(inlet_editor::actions::Down);
    cx.dispatch_action(inlet_editor::actions::Home);
    let below = editor.update_in(cx, |ed, _, _| ed.selection().head);
    assert!(below > row_above);
}

#[gpui::test]
fn unmark_without_a_composition_keeps_the_last_edit(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(5, cx));
    window
        .update(cx, |ed, window, cx| {
            // Text from the server that is over the limit.
            ed.set_wire("0123456789", &NoNames, window, cx);
            assert!(ed.buffer().is_over_limit());
            // The user deletes some (still over the limit), and the platform calls unmark.
            ed.replace_text_in_range(Some(0..2), "", window, cx);
            assert_eq!(ed.buffer().text(), "23456789");
            ed.unmark_text(window, cx);
            assert_eq!(ed.buffer().text(), "23456789", "the deletion is kept");
            assert!(ed.undo_edit(window, cx), "and it can be undone");
            assert_eq!(ed.buffer().text(), "0123456789");
        })
        .unwrap();
}

#[gpui::test]
fn a_composition_that_shrinks_over_limit_text_is_kept(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(5, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.set_wire("0123456789", &NoNames, window, cx);
            // Replace "89" with "x" through the input method: shorter, still over.
            ed.replace_and_mark_text_in_range(Some(8..10), "x", None, window, cx);
            ed.unmark_text(window, cx);
            assert_eq!(ed.buffer().text(), "01234567x");
            // A composition that makes it longer is taken back.
            ed.replace_and_mark_text_in_range(None, "yz", None, window, cx);
            assert_eq!(ed.buffer().text(), "01234567xyz");
            ed.unmark_text(window, cx);
            assert_eq!(ed.buffer().text(), "01234567x");
        })
        .unwrap();
}

#[gpui::test]
fn a_picked_custom_emoji_is_a_pill_and_never_a_mention(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| Editor::new(2000, cx));
    window
        .update(cx, |ed, window, cx| {
            ed.insert_text("hi ", window, cx).unwrap();
            assert!(ed.insert_custom_emoji(Id(41), "wave", false, window, cx));
            assert!(ed.insert_custom_emoji(Id(42), "party", true, window, cx));
            assert_eq!(ed.buffer().text(), "hi :wave::party:");
            assert_eq!(ed.to_wire(), "hi <:wave:41><a:party:42>");
            let allowed = ed.buffer().allowed_mentions();
            assert!(allowed.users.is_empty() && allowed.roles.is_empty() && !allowed.everyone);
            // An empty name is refused.
            assert!(!ed.insert_custom_emoji(Id(43), "", false, window, cx));
            // Nothing goes in while an input method is composing.
            ed.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            let before = ed.buffer().text().to_owned();
            assert!(!ed.insert_custom_emoji(Id(44), "x", false, window, cx));
            assert_eq!(ed.buffer().text(), before);
        })
        .unwrap();
}
