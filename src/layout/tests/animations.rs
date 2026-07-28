use std::fmt::Write as _;

use insta::assert_snapshot;
use niri_config::animations::{Curve, EasingParams, Kind};

use super::*;

fn format_tiles(layout: &Layout<TestWindow>) -> String {
    let mut buf = String::new();
    let ws = layout.active_workspace().unwrap();
    let mut tiles: Vec<_> = ws.tiles_with_render_positions().collect();

    // We sort by id since that gives us a consistent order (from first opened to last), but we
    // don't print the id since it's nondeterministic (the id is a global counter across all
    // running tests in the same binary).
    tiles.sort_by_key(|(tile, _, _)| tile.window().id());
    for (tile, pos, _visible) in tiles {
        let Size { w, h, .. } = tile.animated_tile_size();
        let Point { x, y, .. } = pos;
        writeln!(&mut buf, "{w:>3.0} × {h:>3.0} at x:{x:>3.0} y:{y:>3.0}").unwrap();
    }
    buf
}

fn make_options() -> Options {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });

    let mut options = Options {
        layout: niri_config::Layout {
            gaps: 0.0,
            ..Default::default()
        },
        ..Options::default()
    };
    options.animations.window_resize.anim.kind = LINEAR;
    options.animations.window_movement.0.kind = LINEAR;

    options
}

fn set_up_two_in_column() -> Layout<TestWindow> {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::ConsumeWindowIntoColumn,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];

    check_ops_with_options(make_options(), ops)
}

#[test]
fn height_resize_animates_next_y() {
    let mut layout = set_up_two_in_column();

    let ops = [
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::AdjustFixed(-50),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 50)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed yet, so we're at the initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    // Advance the time halfway.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Top window is half-resized at 75 px tall, bottom window is at y=75 matching it.
    assert_snapshot!(format_tiles(&layout), @r"
    100 ×  75 at x:  0 y:  0
    200 × 200 at x:  0 y: 75
    ");

    // Advance the time to completion.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Final state at 50 px.
    assert_snapshot!(format_tiles(&layout), @r"
    100 ×  50 at x:  0 y:  0
    200 × 200 at x:  0 y: 50
    ");
}

#[test]
fn clientside_height_change_doesnt_animate() {
    let mut layout = set_up_two_in_column();

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    let ops = [
        // The top window shrinks by itself, without a niri-issued resize.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 50)),
        },
        // This does not start any animations.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed yet, but we are at the final state right away.
    assert_snapshot!(format_tiles(&layout), @r"
    100 ×  50 at x:  0 y:  0
    200 × 200 at x:  0 y: 50
    ");
}

#[test]
fn height_resize_and_back() {
    let mut layout = set_up_two_in_column();

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time halfway.
        Op::AdvanceAnimations { msec_delta: 500 },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Top window is half-resized at 150 px tall, bottom window is at y=150 matching it.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 150 at x:  0 y:  0
    200 × 200 at x:  0 y:150
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This starts a new resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed yet, and we expect no animation jumps, so this state matches the last.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 150 at x:  0 y:  0
    200 × 200 at x:  0 y:150
    ");

    // Advance the time halfway.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Halfway through at 125px.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 125 at x:  0 y:  0
    200 × 200 at x:  0 y:125
    ");

    // Advance the time to completion.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Final state back at 100px.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn height_resize_and_cancel() {
    let mut layout = set_up_two_in_column();

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time slightly.
        Op::AdvanceAnimations { msec_delta: 50 },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Top window is half-resized at 105 px tall, bottom window is at y=105 matching it.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 105 at x:  0 y:  0
    200 × 200 at x:  0 y:105
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 5 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Since the resize animation is cancelled, the height goes to the new value immediately. The Y
    // position doesn't jump, instead the animation is offset to preserve the current position.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:105
    ");

    // Advance to the end of the move animation.
    Op::AdvanceAnimations { msec_delta: 950 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn height_resize_and_back_during_another_y_anim() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Consume second window into column, starting the X/Y move anim down.
    Op::ConsumeWindowIntoColumn.apply(&mut layout);

    // No time had passed, so no change in coordinates yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance the time halfway.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Second window halfway to the bottom.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 50 y: 50
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed, so no change in state yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 50 y: 50
    ");

    // Advance the time a bit.
    Op::AdvanceAnimations { msec_delta: 200 }.apply(&mut layout);

    // X changed by 20, but y changed by 30 since the Y movement from the resize compounds with the
    // Y movement from consume-into-column.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 120 at x:  0 y:  0
    200 × 200 at x: 30 y: 80
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed, so no change in state yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 120 at x:  0 y:  0
    200 × 200 at x: 30 y: 80
    ");

    // Advance the time a bit. Both resize and consume movement are still ongoing.
    // T-15/A-3: the resize-back here lands at progress 0.2 (under half), so the
    // in-flight resize is target-tracked (start_time preserved) rather than
    // restarted; the height reads 115 here and 113 one advance later instead of
    // the pre-T-15 restart values 116 / 114. The bottom window's Y also shifts
    // (84→85, 86→88) because its move anim is now phase-locked to the preserved
    // resize timeline instead of being restarted at the commit instant.
    Op::AdvanceAnimations { msec_delta: 200 }.apply(&mut layout);

    assert_snapshot!(format_tiles(&layout), @r"
    100 × 115 at x:  0 y:  0
    200 × 200 at x: 10 y: 85
    ");

    // Advance the time to complete the consume movement.
    Op::AdvanceAnimations { msec_delta: 100 }.apply(&mut layout);

    // The Y position is still lower than the height since the window started the resize-induced Y
    // movement high up.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 113 at x:  0 y:  0
    200 × 200 at x:  0 y: 88
    ");

    // Advance the time to complete the resize.
    Op::AdvanceAnimations { msec_delta: 700 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn height_resize_and_cancel_during_another_y_anim() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Consume second window into column, starting the X/Y move anim down.
    Op::ConsumeWindowIntoColumn.apply(&mut layout);

    // No time had passed, so no change in coordinates yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance the time halfway.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);

    // Second window halfway to the bottom.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 50 y: 50
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time slightly.
        Op::AdvanceAnimations { msec_delta: 50 },
    ];
    check_ops_on_layout(&mut layout, ops);

    // X changed by 5, but y changed by 8 since the Y movement from the resize compounds with the Y
    // movement from consume-into-column.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 105 at x:  0 y:  0
    200 × 200 at x: 45 y: 58
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 5 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Since the resize anim was cancelled, second window's Y anim is adjusted to preserve the
    // current position while targeting the new final position.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 45 y: 58
    ");

    // Advance the time to complete the consume movement.
    Op::AdvanceAnimations { msec_delta: 450 }.apply(&mut layout);

    // Since we don't cancel the resize-induced part of the anim (in fact the move Y anim isn't
    // split into parts, so there's no way to tell), it keeps going still.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y: 78
    ");

    // Advance the time to complete the resize-induced anim.
    Op::AdvanceAnimations { msec_delta: 550 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn height_resize_before_another_y_anim_then_back() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time a bit.
        Op::AdvanceAnimations { msec_delta: 200 },
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The resize is in progress.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 120 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Consume second window into column, starting the X/Y move anim down.
    Op::ConsumeWindowIntoColumn.apply(&mut layout);

    // No time had passed, so no change in coordinates yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 120 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance the time halfway.
    Op::AdvanceAnimations { msec_delta: 600 }.apply(&mut layout);

    // Second window halfway to the bottom. Since consume happened after the start of the first
    // window's resize, the second window's Y is unaffected by it and is animating towards the
    // final position right away.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 180 at x:  0 y:  0
    200 × 200 at x: 40 y:120
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // No time had passed, so no change in state yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 180 at x:  0 y:  0
    200 × 200 at x: 40 y:120
    ");

    // Advance the time a bit. Both resize and consume movement are still ongoing.
    Op::AdvanceAnimations { msec_delta: 200 }.apply(&mut layout);

    assert_snapshot!(format_tiles(&layout), @r"
    100 × 164 at x:  0 y:  0
    200 × 200 at x: 20 y:116
    ");

    // Advance the time to complete the consume movement.
    Op::AdvanceAnimations { msec_delta: 200 }.apply(&mut layout);

    assert_snapshot!(format_tiles(&layout), @r"
    100 × 148 at x:  0 y:  0
    200 × 200 at x:  0 y:112
    ");

    // Advance the time to complete the resize.
    Op::AdvanceAnimations { msec_delta: 600 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn height_resize_before_another_y_anim_then_cancel() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time a bit.
        Op::AdvanceAnimations { msec_delta: 20 },
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The resize is in progress.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 102 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Consume second window into column, starting the X/Y move anim down.
    Op::ConsumeWindowIntoColumn.apply(&mut layout);

    // No time had passed, so no change in coordinates yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 102 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance the time a little.
    Op::AdvanceAnimations { msec_delta: 20 }.apply(&mut layout);

    // Second window on its way to the bottom.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 104 at x:  0 y:  0
    200 × 200 at x: 98 y:  4
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 4 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // The second window's trajectory readjusts to the new final position at 100 px, without jumps.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 98 y:  4
    ");

    // Advance the time to complete the consume movement.
    Op::AdvanceAnimations { msec_delta: 980 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn clientside_height_change_during_another_y_anim() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
        Op::ConsumeWindowIntoColumn,
        // Clear the animate next configure flag.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time a bit.
        Op::AdvanceAnimations { msec_delta: 200 },
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // Second window on its way to the bottom.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x: 80 y: 20
    ");

    let ops = [
        // The top window suddenly grows.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // The second window's trajectory readjusts to the new final position at 200 px, without jumps.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 200 at x:  0 y:  0
    200 × 200 at x: 80 y: 20
    ");

    // Advance the time to complete the consume movement.
    Op::AdvanceAnimations { msec_delta: 800 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 200 at x:  0 y:  0
    200 × 200 at x:  0 y:200
    ");
}

#[test]
fn height_resize_cancel_with_stationary_second_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
        // Issue a resize.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The top window grows in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 200)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time a bit.
        Op::AdvanceAnimations { msec_delta: 20 },
    ];
    let mut options = make_options();
    // Window movement will happen instantly.
    options.animations.window_movement.0.off = true;
    let mut layout = check_ops_with_options(options, ops);

    // The resize is in progress.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 102 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Consume second window into column, starting the X/Y move anim down.
    Op::ConsumeWindowIntoColumn.apply(&mut layout);

    // No time had passed, so no change in coordinates yet.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 102 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance the time a little.
    Op::AdvanceAnimations { msec_delta: 20 }.apply(&mut layout);

    // The window movement anim is off, so the second window is already at the bottom. Since
    // consume started after the resize, the second window is unaffected by the resize-induced Y
    // movement, and sits at the final position at 200 px.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 104 at x:  0 y:  0
    200 × 200 at x:  0 y:200
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response, the bottom remains as is.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 4 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // This causes the second window to jump down, which is correct because it hadn't been in an
    // animation, and as far as it's concerned, this is the same case as a window just deciding to
    // do a clientside resize on its own, which is not animated.
    //
    // Since the resize is also cancelled, this is the final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");
}

#[test]
fn width_resize_and_cancel() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        // The left window grows in response.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(200, 100)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time slightly.
        Op::AdvanceAnimations { msec_delta: 50 },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Left window is half-resized at 105 px wide, right window is at x=105 matching it.
    assert_snapshot!(format_tiles(&layout), @r"
    105 × 100 at x:  0 y:  0
    200 × 200 at x:105 y:  0
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 5 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Since the resize animation is cancelled, the width goes to the new value immediately. The X
    // position doesn't jump, instead the animation is restarted to preserve the current position.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:105 y:  0
    ");

    // Advance to the end of the move animation.
    Op::AdvanceAnimations { msec_delta: 1000 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");
}

#[test]
fn width_resize_and_cancel_of_column_to_the_left() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        Op::SetForcedSize {
            id: 2,
            size: Some(Size::new(200, 200)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    let mut layout = check_ops_with_options(make_options(), ops);

    // The initial state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");

    let ops = [
        // Issue a resize.
        Op::SetWindowWidth {
            id: Some(1),
            change: SizeChange::SetFixed(200),
        },
        // The left window grows in response.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(200, 100)),
        },
        // This starts the resize animation.
        Op::Communicate(1),
        Op::Communicate(2),
        // Advance the time slightly.
        Op::AdvanceAnimations { msec_delta: 50 },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Left window is half-resized at 105 px wide, it's at x=-5 matching the right edge position.
    assert_snapshot!(format_tiles(&layout), @r"
    105 × 100 at x: -5 y:  0
    200 × 200 at x:100 y:  0
    ");

    let ops = [
        // Issue a resize back.
        Op::SetWindowWidth {
            id: Some(1),
            change: SizeChange::SetFixed(100),
        },
        // The top window shrinks in response.
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 100)),
        },
        // This cancels the resize animation since the change of 5 px is less than the resize
        // animation threshold.
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Since the resize animation is cancelled, the width goes to the new value immediately. The X
    // position doesn't jump, instead the animation is restarted to preserve the current position.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x: -5 y:  0
    200 × 200 at x:100 y:  0
    ");

    // Advance to the end of the move animation.
    Op::AdvanceAnimations { msec_delta: 1000 }.apply(&mut layout);

    // Final state.
    assert_snapshot!(format_tiles(&layout), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:100 y:  0
    ");
}

/// T-11: fast horizontal swipe release must settle with a single velocity-
/// carrying animation (no zero-velocity secondary snap).
#[test]
fn view_offset_gesture_end_preserves_swipe_velocity() {
    use std::time::Duration;

    // Spring settle so velocity() is meaningful (defaults are already spring).
    let mut options = Options::default();
    options.layout.gaps = 0.0;
    options.animations.horizontal_view_movement.0.kind =
        niri_config::animations::Kind::Spring(niri_config::animations::SpringParams {
            damping_ratio: 1.0,
            stiffness: 800,
            epsilon: 0.0001,
        });

    // Output is 1280×720; a 2000px-wide window is larger-than-view so the
    // former HACK secondary snap path used to fire on edge settle.
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::SetForcedSize {
                id: 1,
                size: Some(Size::new(2000, 400)),
            },
            Op::Communicate(1),
            Op::CompleteAnimations,
        ],
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::ViewOffsetGestureBegin {
                output_idx: 1,
                workspace_idx: None,
                is_touchpad: false,
            },
            // Fast rightward fling over ~30ms.
            Op::ViewOffsetGestureUpdate {
                delta: 40.,
                timestamp: Duration::from_millis(0),
                is_touchpad: false,
            },
            Op::ViewOffsetGestureUpdate {
                delta: 50.,
                timestamp: Duration::from_millis(10),
                is_touchpad: false,
            },
            Op::ViewOffsetGestureUpdate {
                delta: 60.,
                timestamp: Duration::from_millis(20),
                is_touchpad: false,
            },
        ],
    );

    // Align controlled clock with last event so idle push is accepted as a
    // zero-extra-time sample (no decay), not silently rejected.
    layout.clock.set_unadjusted(Duration::from_millis(20));
    Op::ViewOffsetGestureEnd {
        is_touchpad: Some(false),
    }
    .apply(&mut layout);

    let ws = layout.active_workspace().unwrap();
    let scrolling = ws.scrolling();
    assert!(
        scrolling.test_view_offset_animating(),
        "gesture end should produce a settle animation"
    );

    let v0 = scrolling.test_view_offset_velocity();
    // T-14 LS on (40,90,150) @ (0,10,20)ms is well above 1000 px/s; the
    // pre-T-11 double-anim path zeroed velocity on secondary snap retarget.
    assert!(
        v0.abs() > 100.,
        "settle animation must carry swipe velocity, got {v0}"
    );

    // Frame-by-frame: 1ms sample of view_pos must not reverse abruptly
    // relative to the handed-off velocity (no 急停 kink).
    let p0 = scrolling.view_pos();
    Op::AdvanceAnimations { msec_delta: 1 }.apply(&mut layout);
    let ws = layout.active_workspace().unwrap();
    let scrolling = ws.scrolling();
    let p1 = scrolling.view_pos();
    let fd = (p1 - p0) / 0.001;
    // Finite-difference over the first ms must stay within 15% of v0 (spring
    // acceleration changes velocity, but a zero-vel restart would be ~0).
    let rel = (fd - v0).abs() / v0.abs();
    assert!(
        rel < 0.15,
        "view_pos velocity kink after settle start: v0={v0} fd={fd} rel={rel}"
    );
}

/// T-13: interactive-move release must hand pointer velocity into the settle
/// `animate_move_from` so the first frame of `render_offset` keeps flinging.
#[test]
fn interactive_move_end_preserves_pointer_velocity() {
    use std::time::Duration;

    let mut options = Options::default();
    options.layout.gaps = 0.0;
    options.animations.window_movement.0.kind =
        niri_config::animations::Kind::Spring(niri_config::animations::SpringParams {
            damping_ratio: 1.0,
            stiffness: 800,
            epsilon: 0.0001,
        });

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::CompleteAnimations,
        ],
    );

    let output = layout.outputs().next().unwrap().clone();

    // Detach past the rubberband threshold (256²), then fling rightward.
    assert!(layout.interactive_move_begin(1, &output, Point::from((50., 50.))));

    // Large step to enter Moving (non-floating threshold is 256²).
    assert!(layout.interactive_move_update(
        &1,
        Point::from((300., 0.)),
        output.clone(),
        Point::from((350., 50.)),
        Duration::from_millis(0),
    ));

    // Fast fling after detach. T-14 SwipeTracker uses least-squares slope of
    // cumulative position, so the large detach seed no longer inflates velocity.
    assert!(layout.interactive_move_update(
        &1,
        Point::from((40., 0.)),
        output.clone(),
        Point::from((390., 50.)),
        Duration::from_millis(10),
    ));
    assert!(layout.interactive_move_update(
        &1,
        Point::from((50., 0.)),
        output.clone(),
        Point::from((440., 50.)),
        Duration::from_millis(20),
    ));
    assert!(layout.interactive_move_update(
        &1,
        Point::from((60., 0.)),
        output.clone(),
        Point::from((500., 50.)),
        Duration::from_millis(30),
    ));

    // Align controlled clock with last event so idle compensation is a no-op
    // (zero extra time) rather than decaying the fling.
    layout.clock.set_unadjusted(Duration::from_millis(30));
    layout.interactive_move_end(&1);
    layout.verify_invariants();

    let ws = layout.active_workspace().unwrap();
    let tile = ws
        .tiles()
        .find(|t| *t.window().id() == 1)
        .expect("tile reinserted after move end");

    let vel = tile.move_animation_velocity();
    // Acceptance: first-frame render_offset velocity ≈ pointer/tracker velocity.
    // Pre-T-13 always started settle at 0 (静止再缓动).
    // T-14 LS on cumulative pos (300,340,390,450) @ (0,10,20,30)ms = 5000 px/s.
    // Detach also starts a rubberband move-anim whose residual absolute velocity
    // is folded into settle (combined_move_velocity), so exact equality to pure
    // pointer LS is not expected — but we must be clearly in the de-biased
    // regime, not the old Σdelta/(t_last−t_first)=15000 overestimate, and not 0.
    // Pure pointer LS is 5000; detach rubberband move-anim residual is folded
    // in via combined_move_velocity (measured ~2k extra on this fixture).
    let ls_pointer_vx = 5000.0_f64;
    let old_biased_vx = (300. + 40. + 50. + 60.) / 0.030;
    assert!(
        vel.x > 1000.,
        "settle must carry fling velocity (pre-T-13 was 0), got {}",
        vel.x
    );
    assert!(
        (vel.x - ls_pointer_vx).abs() < (vel.x - old_biased_vx).abs(),
        "T-14 de-bias: settle vel {} should be nearer LS {} than old biased {}",
        vel.x,
        ls_pointer_vx,
        old_biased_vx
    );
    // Residual-inclusive band: not a free 1k–10k window.
    let rel_to_ls = (vel.x - ls_pointer_vx).abs() / ls_pointer_vx;
    assert!(
        rel_to_ls < 0.50,
        "settle vel {} must stay within 50% of pointer LS {} (rel={rel_to_ls})",
        vel.x,
        ls_pointer_vx
    );

    // First-millisecond FD of render_offset.x ≈ handed-off velocity (no kink).
    let o0 = tile.render_offset();
    Op::AdvanceAnimations { msec_delta: 1 }.apply(&mut layout);
    let ws = layout.active_workspace().unwrap();
    let tile = ws.tiles().find(|t| *t.window().id() == 1).unwrap();
    let o1 = tile.render_offset();
    let fd = (o1.x - o0.x) / 0.001;
    let rel = (fd - vel.x).abs() / vel.x.abs().max(1.);
    assert!(
        rel < 0.15,
        "render_offset velocity kink after move end: v0={} fd={fd} rel={rel}",
        vel.x
    );
}

/// T-15/A-3: three sequential client commits during an in-flight resize must
/// keep the same animation `start_time` (target tracking), not restart phase.
#[test]
fn resize_midflight_commits_preserve_start_time() {
    let mut options = make_options();
    // Linear 1000ms so progress is a pure clock fraction.
    options.animations.window_resize.anim.kind =
        niri_config::animations::Kind::Easing(niri_config::animations::EasingParams {
            duration_ms: 1000,
            curve: niri_config::animations::Curve::Linear,
        });

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::SetForcedSize {
                id: 1,
                size: Some(Size::new(400, 400)),
            },
            Op::Communicate(1),
            Op::CompleteAnimations,
        ],
    );

    // Issue an animated width resize; the (slow) client answers in steps.
    Op::SetWindowWidth {
        id: None,
        change: niri_ipc::SizeChange::SetFixed(280),
    }
    .apply(&mut layout);

    // Helper: re-arm the per-configure animate bit so subsequent commits of a
    // multi-step client response still produce an animation snapshot (mirrors
    // Mapped storing a snapshot on each animated serial commit).
    let rearm_animate = |layout: &Layout<TestWindow>| {
        for (_mon, win) in layout.windows() {
            if win.0.id == 1 {
                win.0.animate_next_configure.set(true);
            }
        }
    };

    let commit_size = |layout: &mut Layout<TestWindow>, w: i32| {
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(w, 400)),
        }
        .apply(layout);
        Op::Communicate(1).apply(layout);
    };

    let anim_start = |layout: &Layout<TestWindow>| {
        let scrolling = layout.active_workspace().unwrap().scrolling();
        let tile = scrolling.tiles().next().unwrap();
        tile.resize_animation()
            .expect("resize anim must be active")
            .start_time()
    };

    // Commit 1: first step (Δ=20 from 400→380) starts the resize anim.
    commit_size(&mut layout, 380);
    let t0 = anim_start(&layout);

    // Advance past a few frames but stay well under half (track band).
    Op::AdvanceAnimations { msec_delta: 100 }.apply(&mut layout);

    // Commit 2: another small step — must target-track, not restart.
    rearm_animate(&layout);
    commit_size(&mut layout, 360);
    let t1 = anim_start(&layout);
    assert_eq!(
        t0, t1,
        "T-15: second mid-flight commit must preserve start_time (was {t0:?}, now {t1:?})"
    );

    Op::AdvanceAnimations { msec_delta: 100 }.apply(&mut layout);

    // Commit 3: third step, still under half and under restart threshold.
    rearm_animate(&layout);
    commit_size(&mut layout, 340);
    let t2 = anim_start(&layout);
    assert_eq!(
        t0, t2,
        "T-15: triple-commit start_time must stay {t0:?}, got {t2:?}"
    );

    // Progress should still reflect wall time from original start (~200ms of
    // 1000ms linear), not a fresh 0 from a restart.
    let progress = {
        let scrolling = layout.active_workspace().unwrap().scrolling();
        let tile = scrolling.tiles().next().unwrap();
        tile.resize_animation().unwrap().value()
    };
    assert!(
        progress > 0.15 && progress < 0.35,
        "tracked progress should stay near 0.2 after 200ms linear, got {progress}"
    );
}

/// T-15/A-3 glue: while the top tile's resize is target-tracked across a
/// mid-flight commit, the tile below must stay *glued* to the top tile's bottom
/// edge at every frame — i.e. the resize anim and the tiles-below move anim
/// share the same `start_time` so `val_companion + val_resize == 1`. Pre-T-15
/// the companion restarted at the commit instant (fresh `start_time`), opening
/// a transient gap to a lagging neighbour once the tracked resize settled.
#[test]
fn resize_tracking_keeps_tiles_below_glued() {
    let mut options = make_options();
    options.animations.window_resize.anim.kind =
        niri_config::animations::Kind::Easing(niri_config::animations::EasingParams {
            duration_ms: 1000,
            curve: niri_config::animations::Curve::Linear,
        });

    // Two tiles stacked in one column, gaps 0. Top id=1 (100×200), bottom id=2 (200×200).
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::FocusColumnLeft,
            Op::ConsumeWindowIntoColumn,
            Op::SetForcedSize {
                id: 1,
                size: Some(Size::new(100, 200)),
            },
            Op::SetForcedSize {
                id: 2,
                size: Some(Size::new(200, 200)),
            },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::CompleteAnimations,
        ],
    );

    // Snapshot of (top animated height, bottom render y) at the current instant.
    let glue = |layout: &Layout<TestWindow>| -> (f64, f64) {
        let ws = layout.active_workspace().unwrap();
        let mut tiles: Vec<_> = ws.tiles_with_render_positions().collect();
        tiles.sort_by_key(|(t, _, _)| t.window().id());
        let (top, top_pos, _) = &tiles[0];
        let (_bot, bot_pos, _) = &tiles[1];
        (top.animated_tile_size().h + top_pos.y, bot_pos.y)
    };

    // Start an animated height shrink on the top tile: 200 → 150.
    let ops = [
        Op::SetWindowHeight {
            id: None,
            change: niri_ipc::SizeChange::SetFixed(150),
        },
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 150)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Advance to progress 0.2 (200ms of 1000ms) — under half, inside the track band.
    Op::AdvanceAnimations { msec_delta: 200 }.apply(&mut layout);

    // Second small commit (150 → 130): Δ visual = 60 ≤ 80, progress 0.2 < 0.5 → TRACK.
    for (_mon, win) in layout.windows() {
        if win.0.id == 1 {
            win.0.animate_next_configure.set(true);
        }
    }
    let ops = [
        Op::SetWindowHeight {
            id: None,
            change: niri_ipc::SizeChange::SetFixed(130),
        },
        Op::SetForcedSize {
            id: 1,
            size: Some(Size::new(100, 130)),
        },
        Op::Communicate(1),
        Op::Communicate(2),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Continuity at the track instant: the top height must not jump.
    let (top_h, bot_y) = glue(&layout);
    assert!(
        (top_h - 190.).abs() < 0.5,
        "track instant must preserve visual (top height ≈ 190), got {top_h}"
    );
    assert!(
        (bot_y - top_h).abs() < 1.5,
        "bottom must stay glued at the track instant (bot_y {bot_y} vs top bottom {top_h})"
    );

    // Advance across the resize's original end (t=1000). Pre-fix the tracked
    // resize settled at t=1000 while the restarted companion lagged to t=1200,
    // opening a ~gap; with the phase-locked companion they must stay glued.
    for dt in [300, 300, 200, 200, 200] {
        Op::AdvanceAnimations { msec_delta: dt }.apply(&mut layout);
        let (top_h, bot_y) = glue(&layout);
        assert!(
            (bot_y - top_h).abs() < 1.5,
            "glue broken at t+{dt}ms: bottom y {bot_y} != top bottom {top_h}"
        );
    }
}

/// T-16/A-7: consecutive swaps must stay continuous.
///
/// `swap_window_in_direction` used to `stop_move_animations()` on the tile arriving from the
/// target column, as a workaround for the push `add_tile_to_column` applied when inserting above
/// the tile it was about to remove. That workaround also killed the tile's *legitimate* in-flight
/// animation, so a second swap issued mid-flight teleported the tile to the previous swap's
/// endpoint before starting the new one. With the insertion push suppressed for that one tile, no
/// cancellation is needed and the second swap composes with the first.
#[test]
fn consecutive_swaps_stay_continuous() {
    // Two columns of two tiles each, so swap takes the tile-swap path (not move_column_to).
    let mut layout = check_ops_with_options(
        make_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::FocusColumnLeft,
            Op::ConsumeWindowIntoColumn,
            Op::AddWindow {
                params: TestWindowParams::new(3),
            },
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
            Op::FocusColumnLeft,
            Op::ConsumeWindowIntoColumn,
            Op::CompleteAnimations,
        ],
    );

    let render_x = |layout: &Layout<TestWindow>, id: usize| -> f64 {
        let ws = layout.active_workspace().unwrap();
        ws.tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().0.id == id)
            .map(|(_, pos, _)| pos.x)
            .unwrap_or_else(|| panic!("window {id} not on the active workspace"))
    };

    // First swap: window 1 (active, in the left column) trades places with window 3 and starts
    // animating rightwards across the column boundary. Via check_ops_on_layout so the layout
    // invariants run right after the swap — the changed code is inside swap.
    check_ops_on_layout(
        &mut layout,
        [Op::SwapWindowInDirection(ScrollDirection::Left)],
    );
    assert_eq!(
        render_x(&layout, 1),
        0.,
        "the swap animates from the old position, so frame zero is still the old x"
    );

    // Let it run part of the way, then read where window 1 visually is.
    Op::AdvanceAnimations { msec_delta: 300 }.apply(&mut layout);
    let x_before = render_x(&layout, 1);
    assert!(
        x_before > 0. && x_before < 100.,
        "window 1 must be mid-flight between the columns, got x = {x_before}"
    );

    // Second swap while the first is still in flight. Window 1 came *from* the target column of
    // this swap, so it lands in the slot the old workaround cancelled animations on.
    check_ops_on_layout(
        &mut layout,
        [Op::SwapWindowInDirection(ScrollDirection::Right)],
    );
    let x_after = render_x(&layout, 1);

    // The tile must not jump at the instant of the second swap. Pre-fix, the
    // stop_move_animations() dropped the in-flight offset and the tile snapped to the first
    // swap's endpoint.
    assert!(
        (x_after - x_before).abs() < 1.,
        "A-7: second swap must not teleport the tile: render x jumped {x_before} -> {x_after}"
    );
}
