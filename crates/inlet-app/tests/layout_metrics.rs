// SPDX-License-Identifier: AGPL-3.0-or-later
//! The main layout's measures, from Fluxer's own CSS (see the design notes): guild
//! rail, channel list, headers, channel rows, the message grid, the composer and the
//! member list. The real view is drawn with the demo data on gpui's test platform and
//! measured through the elements' bounds; nothing is shown on screen.
//!
//! Run with `cargo test -p inlet --features demo --test layout_metrics`.

use gpui::{px, size, Bounds, Pixels, TestAppContext, VisualTestContext};
use inlet::layout_probe::{selector, Probe, CHANNEL_ROW_RADIUS};

/// The window of the visual checks' scenes.
const WINDOW: (f32, f32) = (1280., 860.);

fn open(cx: &mut TestAppContext) -> &mut VisualTestContext {
    let (_probe, cx) = cx.add_window_view(Probe::new);
    cx.simulate_resize(size(px(WINDOW.0), px(WINDOW.1)));
    cx.run_until_parked();
    // Drawn twice: some views size themselves from the previous frame's bounds.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx
}

fn bounds(cx: &mut VisualTestContext, name: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(name)
        .unwrap_or_else(|| panic!("nothing on screen is marked {name:?}"))
}

fn px_f(p: Pixels) -> f32 {
    p.as_f32()
}

#[track_caller]
fn assert_px(got: Pixels, want: f32, what: &str) {
    assert!(
        (px_f(got) - want).abs() < 0.01,
        "{what}: {} px, Fluxer has {want} px",
        px_f(got)
    );
}

#[gpui::test]
fn rail_is_72_wide_with_48_boxes(cx: &mut TestAppContext) {
    let cx = open(cx);
    let rail = bounds(cx, selector::RAIL);
    assert_px(rail.size.width, 72., "guild rail width");
    assert_px(rail.origin.x, 0., "guild rail left edge");
    for name in [selector::RAIL_HOME, selector::RAIL_GUILD] {
        let entry = bounds(cx, name);
        assert_px(entry.size.height, 48., name);
        assert_px(entry.size.width, 72., name);
        assert_px(entry.origin.x, 0., name);
    }
}

#[gpui::test]
fn sidebar_is_320_and_header_56(cx: &mut TestAppContext) {
    let cx = open(cx);
    let sidebar = bounds(cx, selector::SIDEBAR);
    assert_px(sidebar.size.width, 320., "channel list width");
    assert_px(
        sidebar.origin.x,
        72.,
        "channel list left edge (after the rail)",
    );
    let guild_header = bounds(cx, selector::GUILD_HEADER);
    assert_px(guild_header.size.height, 56., "guild header height");
    assert_px(guild_header.size.width, 320., "guild header width");
    let chat_header = bounds(cx, selector::CHAT_HEADER);
    assert_px(chat_header.size.height, 56., "channel header height");
    assert_px(chat_header.origin.y, 0., "channel header top");
    assert_px(chat_header.origin.x, 392., "channel header left edge");
}

#[gpui::test]
fn channel_row_is_32_with_radius_6(cx: &mut TestAppContext) {
    let cx = open(cx);
    let sidebar = bounds(cx, selector::SIDEBAR);
    let row = bounds(cx, selector::CHANNEL_ROW);
    assert_px(row.size.height, 32., "channel row height");
    // 8 px in from the list's left edge, as Fluxer's `margin-inline-start`.
    assert_px(row.origin.x - sidebar.origin.x, 8., "channel row inset");
    // gpui's bounds carry no corner radius, so this reads the value the row is drawn
    // with. The drawn corners are checked by the visual baselines: the selected row of
    // `rail-channels` and `chat-groups` differs from the sidebar by more than the
    // comparison's channel tolerance, so a different radius fails there.
    assert_eq!(CHANNEL_ROW_RADIUS, 6., "channel row corner radius");
}

#[gpui::test]
fn message_grid_is_16_40_12(cx: &mut TestAppContext) {
    let cx = open(cx);
    let row = bounds(cx, selector::MESSAGE_ROW);
    let avatar = bounds(cx, selector::MESSAGE_AVATAR_COLUMN);
    let content = bounds(cx, selector::MESSAGE_CONTENT);
    assert_px(
        avatar.origin.x - row.origin.x,
        16.,
        "space left of the avatar",
    );
    assert_px(avatar.size.width, 40., "avatar column");
    assert_px(
        content.origin.x - (avatar.origin.x + avatar.size.width),
        12.,
        "space between avatar and text",
    );
    assert_px(
        content.origin.x - row.origin.x,
        68.,
        "text start from the row's left edge",
    );
    // 16 px right padding, as `padding-inline-end`.
    assert_px(
        (row.origin.x + row.size.width) - (content.origin.x + content.size.width),
        16.,
        "space right of the text",
    );
}

#[gpui::test]
fn composer_min_height_is_58(cx: &mut TestAppContext) {
    let cx = open(cx);
    let composer = bounds(cx, selector::COMPOSER_BOX);
    assert_px(composer.size.height, 58., "empty composer height");
    // 6 px from the bottom of the window (`--footer-box-inset`).
    assert_px(
        px(WINDOW.1) - (composer.origin.y + composer.size.height),
        6.,
        "space under the composer",
    );
}

#[gpui::test]
fn member_list_is_264(cx: &mut TestAppContext) {
    let cx = open(cx);
    let list = bounds(cx, selector::MEMBER_LIST);
    assert_px(list.size.width, 264., "member list width");
    assert_px(
        list.origin.x + list.size.width,
        WINDOW.0,
        "member list right edge",
    );
}
