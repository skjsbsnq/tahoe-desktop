//! R05/R06: shared minimize/restore + closing lane ownership invariants.
//!
//! Layout-level tests cover floating / scrolling / tabbed model adapters without GPU.
//! GPU reverse morph / closing completion is covered in `tests::lifecycle_observe`.

use super::*;
use crate::layout::lifecycle_controller::MinimizeRestoreController;
use crate::layout::scrolling::LifecycleOverlayKind;

#[derive(Debug, Clone, Copy)]
enum SpaceKind {
    Scrolling,
    Floating,
    Tabbed,
}

fn layout_for(kind: SpaceKind) -> Layout<TestWindow> {
    let ops: Vec<Op> = match kind {
        SpaceKind::Scrolling => vec![
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
        ],
        SpaceKind::Floating => {
            let mut params = TestWindowParams::new(1);
            params.is_floating = true;
            vec![Op::AddOutput(1), Op::AddWindow { params }]
        }
        SpaceKind::Tabbed => vec![
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::FocusColumnLeft,
            Op::ConsumeWindowIntoColumn,
            Op::SetColumnDisplay(ColumnDisplay::Tabbed),
        ],
    };
    check_ops(ops)
}

/// Table: floating/scrolling/tabbed all route minimize/restore model through spaces that
/// hold one controller instance each — no dual animation containers.
#[test]
fn table_minimize_restore_model_across_spaces() {
    for kind in [SpaceKind::Scrolling, SpaceKind::Floating, SpaceKind::Tabbed] {
        let mut layout = layout_for(kind);
        assert!(
            layout.minimize_window(&1),
            "{kind:?}: minimize must change model"
        );
        assert!(
            window_ipc_state(&layout, 1).0,
            "{kind:?}: window must report minimized"
        );
        // Without GPU there is no Genie entry; controller must stay empty (no parallel fake state).
        let ws = layout.active_workspace().unwrap();
        match kind {
            SpaceKind::Floating => {
                for tile in ws.tiles() {
                    assert!(
                        !tile.is_suppressed_by_restore_lease(),
                        "{kind:?}: no restore lease without Genie overlay"
                    );
                }
            }
            SpaceKind::Scrolling | SpaceKind::Tabbed => {
                let obs = ws.scrolling().render_observation();
                assert!(
                    obs.lifecycle_overlays.iter().all(|o| {
                        o.kind != LifecycleOverlayKind::Minimize
                            && o.kind != LifecycleOverlayKind::Restore
                    }),
                    "{kind:?}: no minimize/restore overlay without renderer: {obs:?}"
                );
            }
        }

        assert!(
            layout.restore_window(&1),
            "{kind:?}: restore must change model"
        );
        assert!(
            !window_ipc_state(&layout, 1).0,
            "{kind:?}: window must report not minimized"
        );
    }
}

#[test]
fn remove_while_minimized_clears_controller_and_lease() {
    // Even without a GPU animation, set_minimized + remove must not leave lease state on a
    // re-added id path; controller clear on remove is the production cleanup path.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MinimizeWindow(1),
    ]);
    assert!(window_ipc_state(&layout, 1).0);
    Op::CloseWindow(1).apply(&mut layout);
    // Remaining window must not inherit a stray lease flag.
    let ws = layout.active_workspace().unwrap();
    for tile in ws.tiles() {
        assert!(
            !tile.is_suppressed_by_restore_lease(),
            "no tile may keep a restore lease after unrelated remove"
        );
    }
    // Controller empty: observation has no minimize/restore overlays.
    let obs = ws.scrolling().render_observation();
    assert!(obs.lifecycle_overlays.iter().all(|o| {
        o.kind != LifecycleOverlayKind::Minimize && o.kind != LifecycleOverlayKind::Restore
    }));
}

#[test]
fn empty_controller_are_animations_ongoing_false() {
    let c: MinimizeRestoreController<usize> = MinimizeRestoreController::new();
    assert!(!c.are_animations_ongoing());
    assert!(c.is_empty());
}

#[test]
fn floating_and_scrolling_each_own_separate_controller_instances() {
    // Structural: after adding one floating and one tiled window, both spaces report
    // independent empty controllers (no shared global container).
    let mut params = TestWindowParams::new(2);
    params.is_floating = true;
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow { params },
    ]);
    let ws = layout.active_workspace().unwrap();
    // Both spaces must answer are_animations_ongoing independently; neither crashes.
    let _ = ws.scrolling().are_animations_ongoing();
    let _ = ws.floating().are_animations_ongoing();
    assert!(ws
        .scrolling()
        .render_observation()
        .lifecycle_overlays
        .is_empty());
}

#[test]
fn minimize_then_restore_without_rect_does_not_suppress_live_tile() {
    // No-anchor restore uses alpha path, not Genie lease.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MinimizeWindow(1),
    ]);
    assert!(layout.restore_window(&1));
    let ws = layout.active_workspace().unwrap();
    for tile in ws.tiles() {
        if tile.window().id() == &1 {
            assert!(
                !tile.is_suppressed_by_restore_lease(),
                "no-anchor restore must not leave restore visibility lease"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// R06: closing animation lane
// ---------------------------------------------------------------------------

/// Close without unmap snapshot must not invent a closing overlay (adapters require snapshot).
#[test]
fn table_close_without_snapshot_leaves_closing_lane_empty() {
    for kind in [SpaceKind::Scrolling, SpaceKind::Floating, SpaceKind::Tabbed] {
        let mut layout = layout_for(kind);
        // Model-level close removes the window; without store_unmap_snapshot there is no
        // closing entry (start_closing only pushes after take_unmap_snapshot).
        Op::CloseWindow(1).apply(&mut layout);
        if let Some(ws) = layout.active_workspace() {
            // Remaining space (if any) must not report a Closing overlay observation.
            let obs = ws.scrolling().render_observation();
            assert!(
                obs.lifecycle_overlays
                    .iter()
                    .all(|o| o.kind != LifecycleOverlayKind::Closing),
                "{kind:?}: no closing overlay without snapshot: {obs:?}"
            );
            // Advance paths on both spaces remain safe (independent lane instances).
            let _ = ws.scrolling().are_animations_ongoing();
            let _ = ws.floating().are_animations_ongoing();
        }
    }
}

#[test]
fn floating_and_scrolling_each_own_separate_closing_lanes() {
    let mut params = TestWindowParams::new(2);
    params.is_floating = true;
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow { params },
    ]);
    let ws = layout.active_workspace().unwrap();
    // Structural: both spaces answer animation queries independently; no closing overlays.
    assert!(!ws.scrolling().are_animations_ongoing());
    assert!(!ws.floating().are_animations_ongoing());
    assert!(ws
        .scrolling()
        .render_observation()
        .lifecycle_overlays
        .iter()
        .all(|o| o.kind != LifecycleOverlayKind::Closing));
}

#[test]
fn close_while_opening_path_is_safe_without_snapshot() {
    // Opening unfinished + close: model remove must not panic; no fake closing entry.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::CompleteAnimations,
    ]);
    layout.start_open_animation_for_window(&1);
    Op::CloseWindow(1).apply(&mut layout);
    if let Some(ws) = layout.active_workspace() {
        let obs = ws.scrolling().render_observation();
        assert!(
            obs.lifecycle_overlays
                .iter()
                .all(|o| o.kind != LifecycleOverlayKind::Closing),
            "close without snapshot must not leave closing overlay: {obs:?}"
        );
    }
}
