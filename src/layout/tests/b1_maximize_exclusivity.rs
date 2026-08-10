//! B1: regression tests for the three user-reported maximize-exclusivity bugs.
//!
//! These started as red baselines (B1) and turned green with the B2/B3 fixes:
//! - B2 (`minimize_maximized_window_reveals_column_tiles_at_animation_start`,
//!   `minimize_releases_only_target_window_visibility`): minimizing the maximize
//!   target ends the exclusivity immediately (per-window, BI-1/BI-2).
//! - B3 (`restore_minimized_window_keeps_other_windows_visible`): the restore must
//!   not keep `suppress_floating_live_tiles` active.
//! See `research-report.md` B-1/B-2/B-3 and `constraints.md` B-C2/BI-1/BI-2/BI-3:
//!
//! 1. Minimize of a maximized window: the other tiles of its column must rejoin the
//!    visible set at animation *start*, not only when the maximize transition settles.
//! 2. Exclusivity release is per-window: unrelated windows must not change visibility,
//!    and releasing the target must not flip the whole column at once.
//! 3. Restoring a minimized window must not leave the maximize exclusivity (and with
//!    it `suppress_floating_live_tiles`) active.
//!
//! Both render-path (`tiles_with_render_positions`) and hit-test-path (`window_under`)
//! visibility are asserted, per B-C2/BI-3: the two must never diverge.

use smithay::utils::Point;

use super::*;

/// Visible tile ids of the active workspace's render set, sorted (order is not the
/// concern of these tests — membership is).
fn sorted_visible_ids(layout: &Layout<TestWindow>) -> Vec<usize> {
    let mut ids: Vec<usize> = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .filter(|(_, _, visible)| *visible)
        .map(|(tile, _, _)| *tile.window().id())
        .collect();
    ids.sort_unstable();
    ids
}

/// 1. Minimize a maximized window whose column still has other live tiles → those tiles
///    must be visible in the render set immediately (before any animation completes).
///
/// Red today: `tiles_in_display_order` keeps `.take(1)` for the whole `CommittedSettling`
/// phase, which is only cleared by `finish_maximize_transition_if_settled()` — the
/// minimize's own tile animation delays settlement, so the back tiles are filtered out
/// until the maximize transition finishes (research-report B-1).
#[test]
fn minimize_maximized_window_reveals_column_tiles_at_animation_start() {
    let ops = [
        Op::AddOutput(1),
        // Build column [1,2,3] one window at a time: each new window lands in its own
        // single-tile column to the right, and consuming it left merges it into the
        // growing column (focus follows the consumed tile, so the next AddWindow
        // lands to its right again).
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None }, // [2] → [1,2]
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ConsumeOrExpelWindowLeft { id: None }, // [3] → [1,2,3]
        // Maximizing window 1 of a multi-tile column extracts it into its own
        // Maximized column (production `Column::set_maximized` behavior), leaving the
        // two back layers 2, 3 in the adjacent column — the user's reported setup.
        Op::MaximizeWindowToEdges { id: Some(1) },
        // Client commits maximized: PendingConfigure → CommittedSettling, resize
        // animation of the target column is still ongoing.
        Op::Communicate(1),
    ];

    let mut layout = check_ops(ops);

    // Precondition: the maximize transition is ongoing and exclusive.
    let obs = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .render_observation();
    assert!(
        obs.policy.maximize_exclusive,
        "maximize transition must still be exclusive before the minimize; obs={obs:?}"
    );
    assert_eq!(
        sorted_visible_ids(&layout),
        vec![1],
        "exclusivity must hide the back tiles while the maximize is settling"
    );

    // The user minimizes the maximized window. The back tiles must rejoin the visible
    // set at animation *start*, without waiting for the maximize transition to settle.
    // (Do NOT advance animations: this assertion is about the first frame of minimize.)
    layout.minimize_window(&1);
    layout.verify_invariants();

    let visible = sorted_visible_ids(&layout);
    assert!(
        visible.contains(&2) && visible.contains(&3),
        "back tiles must be visible at minimize animation start, got {visible:?}"
    );

    // Hit-test path must agree (B-C2): clicking the revealed area of a back tile must
    // hit that tile, not fall through to nothing. Probe the center of window 3's tile
    // (dynamic: the tile's render position/size, so the assertion stays valid across
    // geometry changes).
    let output = layout.outputs().next().unwrap().clone();
    let (tile_pos, tile_size) = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(tile, _, _)| *tile.window().id() == 3)
        .map(|(tile, pos, _)| (pos, tile.tile_size()))
        .expect("window 3 tile");
    let pos = Point::from((
        tile_pos.x + tile_size.w / 2.,
        tile_pos.y + tile_size.h / 2.,
    ));
    assert_eq!(
        layout
            .window_under(&output, pos)
            .map(|(win, _)| *win.id()),
        Some(3),
        "hit-test must reveal back tiles at minimize animation start"
    );
}

/// 2. Exclusivity must be released per-window: after the maximized window is
///    minimized, only the target's own visibility may change; unrelated windows
///    must keep the exact visibility they had before.
///
/// Red today (B-2): `.take(1)` is an all-or-nothing boolean — the moment the
/// transition stops filtering, the *whole* column flips back, so the two unrelated
/// windows change visibility simultaneously with the target.
#[test]
fn minimize_releases_only_target_window_visibility() {
    let ops = [
        Op::AddOutput(1),
        // Column A: [1, 3] (maximize target 1 + back layer 3), built one window at a
        // time so each consume merges a single-tile column. Column B: [2].
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ConsumeOrExpelWindowLeft { id: None }, // [3] → [1,3]
        Op::AddWindow {
            params: TestWindowParams::new(2),
        }, // [1,3], [2] — active = 2's column
        // Maximizing 1 of the multi-tile column A extracts it into its own Maximized
        // column; the back layer 3 stays in the adjacent column. The unrelated
        // window 2 stays in its own column.
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ];

    let mut layout = check_ops(ops);

    // Sanity: after settling, the maximize transition must be finished and all visible.
    assert_eq!(sorted_visible_ids(&layout), vec![1, 2, 3]);

    // Re-maximize 1 so the exclusivity transition is active again.
    check_ops_on_layout(
        &mut layout,
        [
            Op::MaximizeWindowToEdges { id: Some(1) }, // unmaximize first (settled)
            Op::Communicate(1),
            Op::CompleteAnimations,
            Op::MaximizeWindowToEdges { id: Some(1) }, // maximize again
            Op::Communicate(1),
        ],
    );
    // Exclusivity is active: only the maximize target is visible (all other columns
    // and column tiles are filtered).
    assert_eq!(sorted_visible_ids(&layout), vec![1]);

    // BI-2: releasing the target must not flip the whole column. Minimize only the
    // target; window 3 (not part of this lifecycle change) must keep its visibility.
    layout.minimize_window(&1);
    layout.verify_invariants();

    let visible = sorted_visible_ids(&layout);
    assert!(
        visible.contains(&3),
        "window 3 (not minimized, unrelated to the lifecycle change) must stay visible; \
         exclusivity release must be per-window, got {visible:?}"
    );
    // Window 2 (never involved) must remain visible throughout.
    assert!(
        visible.contains(&2),
        "window 2 (in its own column, never involved) must stay visible, got {visible:?}"
    );

    // Hit-test path must agree (B-C2): after the per-window release, clicking the
    // revealed area of a back tile must hit it (not the minimized target, and not
    // fall through). Probe the center of window 2's tile (dynamic geometry; window 2
    // sits in the viewport while window 3's column can be scrolled off-screen after
    // the re-maximize).
    let output = layout.outputs().next().unwrap().clone();
    let (tile_pos, tile_size) = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(tile, _, _)| *tile.window().id() == 2)
        .map(|(tile, pos, _)| (pos, tile.tile_size()))
        .expect("window 2 tile");
    let pos = Point::from((
        tile_pos.x + tile_size.w / 2.,
        tile_pos.y + tile_size.h / 2.,
    ));
    assert_eq!(
        layout
            .window_under(&output, pos)
            .map(|(win, _)| *win.id()),
        Some(2),
        "hit-test must reveal the back tile after per-window release"
    );
}

/// 3. Restoring a minimized window must not leave the maximize exclusivity active:
///    the restore path must not keep suppressing other windows' visibility.
///
/// Setup mirrors the user's report: a column `[1, 2]` where 2 is maximized and then
/// minimized; restoring 2 from the Dock must not keep the maximize exclusivity (and
/// with it `suppress_floating_live_tiles`) active against the unrelated floating
/// window 3.
///
/// Red before B2 (research-report B-3): minimizing the maximize target did not end the
/// maximize exclusivity, so after the restore `maximize_exclusive`/`suppress_floating_
/// live_tiles` were still true and the floating window stayed hidden. B2's
/// minimize-clears-transition fix resolves it; this test pins the exclusivity being
/// gone after the restore. (Whether the *maximized* column then covers the floating
/// layer is the separate, intentional `active_window_covers_floating` behavior — not
/// asserted here.)
#[test]
fn restore_minimized_window_keeps_other_windows_visible() {
    let ops = [
        Op::AddOutput(1),
        // Tiled column [1, 2]: 2 is the maximized-then-minimized window to restore.
        // Built one window at a time so each consume merges a single-tile column
        // (same reliable pattern as test 1).
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None }, // [2] → [1,2]
        // Maximize window 2 of the multi-tile column: it is extracted into its own
        // Maximized column (production behavior), leaving window 1 in the adjacent
        // column — the user's reported setup. The maximize transition is what B2's
        // fix must terminate on minimize.
        Op::FocusWindow(2),
        Op::MaximizeWindowToEdges { id: Some(2) },
        Op::Communicate(2), // column applied sizing_mode = Maximized
        // NOTE: no CompleteAnimations here — the maximize transition must still be
        // active (CommittedSettling) when the window is minimized, so the test pins
        // B2's fix (minimize ends the exclusivity) rather than a settled transition.
        Op::MinimizeWindow(2),
        // NOTE: also no CompleteAnimations after the minimize — advancing would let
        // `finish_maximize_transition_if_settled` settle (and clear) the transition,
        // masking the very exclusivity-residue this test pins.
        // Floating window 3 on top; adding it activates the floating layer.
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::MoveFloatingWindow {
            id: Some(3),
            x: PositionChange::SetFixed(0.),
            y: PositionChange::SetFixed(0.),
            animate: false,
        },
    ];

    let mut layout = check_ops(ops);

    // Precondition: window 2 is actually minimized (setup validity), and the maximize
    // transition may or may not still be active depending on the tree under test —
    // the restore-after assertions below are the real criterion.
    assert!(
        layout
            .active_workspace()
            .unwrap()
            .tiles()
            .any(|tile| *tile.window().id() == 2 && tile.window().is_minimized()),
        "window 2 must be minimized before the restore"
    );

    // Restore the minimized window. Nothing else may change visibility.
    assert!(layout.restore_window(&2));
    // NOTE: no animation advance here either — the assertions below must observe the
    // restore's immediate effect on the maximize exclusivity, before any settle could
    // clear the transition.
    layout.verify_invariants();

    // The restore must not re-activate the maximize exclusivity.
    let obs = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .render_observation();
    assert!(
        !obs.policy.maximize_exclusive && !obs.policy.suppress_floating_live_tiles,
        "restoring window 2 must not keep suppress_floating_live_tiles active; obs={obs:?}"
    );
    assert!(
        !layout.active_workspace().unwrap().scrolling().maximize_transition_is_ongoing(),
        "no maximize transition may remain after the restore"
    );

    // The tiled window 1 must stay visible.
    assert!(
        tile_visibility(&layout)
            .iter()
            .any(|(id, visible)| *id == 1 && *visible),
        "restoring window 2 must not hide the tiled window 1"
    );

    // Hit-test path must agree (B-C2): the restored window 2 (its Maximized column
    // occupies the viewport) must be reachable after the restore.
    let output = layout.outputs().next().unwrap().clone();
    let (tile_pos, tile_size) = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(tile, _, _)| *tile.window().id() == 2)
        .map(|(tile, pos, _)| (pos, tile.tile_size()))
        .expect("window 2 tile");
    let pos = Point::from((
        tile_pos.x + tile_size.w / 2.,
        tile_pos.y + tile_size.h / 2.,
    ));
    assert_eq!(
        layout
            .window_under(&output, pos)
            .map(|(win, _)| *win.id()),
        Some(2),
        "window 2 must be hit-testable after restoring window 2"
    );
}
