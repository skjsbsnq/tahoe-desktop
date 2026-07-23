//! R00/R01 observation helpers: read production maximize / lifecycle overlay policy.

use super::*;
use crate::layout::scrolling::{
    LifecycleOverlayAction, LifecycleOverlayKind, MaximizeTransitionObservation,
    ScrollingRenderObservation, ScrollingRenderPolicy,
};

fn observe_active_scrolling(layout: &Layout<TestWindow>) -> ScrollingRenderObservation<usize> {
    layout
        .active_workspace()
        .expect("active workspace")
        .scrolling()
        .render_observation()
}

#[test]
fn maximize_transition_observation_tracks_pending_timeout_and_idle() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::MaximizeWindowToEdges { id: Some(1) },
    ];

    let mut layout = check_ops_with_options(linear_resize_options(), ops);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::PendingConfigure
    );
    assert_eq!(obs.maximize_target, Some(1));
    // Empty overlay containers still report the production Draw policy.
    assert!(obs.lifecycle_overlays.is_empty());
    assert!(obs.lifecycle_overlays_rendered);
    assert!(obs.policy.maximize_exclusive);

    Op::AdvanceAnimations { msec_delta: 1001 }.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::TimedOutVisibleFallback
    );
    assert_eq!(obs.maximize_target, None);

    Op::Communicate(1).apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::CommittedSettling
    );
    assert_eq!(obs.maximize_target, Some(1));

    Op::CompleteAnimations.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert_eq!(obs.maximize_target, None);
}

#[test]
fn maximize_ongoing_still_draws_lifecycle_overlay_policy() {
    // Construct maximize transition ongoing. Layout-level minimize without a renderer does
    // not populate snapshot containers (production needs GlesRenderer for Genie entries).
    // R01: the overlay *decision* must remain Draw while maximize exclusivity only filters live
    // tiles / floating live tiles.
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::MaximizeWindowToEdges { id: Some(1) },
    ];

    let mut layout = check_ops_with_options(linear_resize_options(), ops);
    let obs = observe_active_scrolling(&layout);
    assert_ne!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert!(obs.policy.maximize_exclusive);
    assert_eq!(
        obs.policy.scrolling_lifecycle_overlays,
        LifecycleOverlayAction::Draw
    );
    assert!(obs.policy.suppress_floating_live_tiles);
    assert_eq!(
        obs.policy.floating_lifecycle_overlays,
        LifecycleOverlayAction::Draw
    );
    assert!(obs.lifecycle_overlays_rendered);

    // Non-target minimize still changes model state; snapshot lane stays empty without GPU.
    Op::MinimizeWindow(2).apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_target, Some(1));
    assert!(
        matches!(
            obs.maximize_transition,
            MaximizeTransitionObservation::PendingConfigure
                | MaximizeTransitionObservation::CommittedSettling
        ),
        "maximize transition still ongoing: {:?}",
        obs.maximize_transition
    );
    assert!(obs.lifecycle_overlays_rendered);
    assert!(obs.lifecycle_overlays.is_empty());
    // Live tiles of non-target remain hidden under exclusivity.
    assert_eq!(tile_visibility(&layout), [(1, true), (2, false)]);
    assert!(!layout.active_workspace().unwrap().is_floating_visible());

    // After canceling maximize, exclusivity ends; Draw remains.
    Op::MaximizeWindowToEdges { id: Some(1) }.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert!(!obs.policy.maximize_exclusive);
    assert!(obs.lifecycle_overlays_rendered);
    assert_eq!(obs.policy, ScrollingRenderPolicy::for_maximize_state(false));
}

#[test]
fn observation_does_not_invent_overlay_kinds() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert!(obs.lifecycle_overlays.is_empty());
    // Enum exists for production classification; empty means no second test state machine.
    let _ = LifecycleOverlayKind::Minimize;
}

#[test]
fn render_policy_never_pairs_invisible_with_draw_false_during_maximize() {
    // Invariant: when maximize is exclusive, overlays are still Draw (not a silent drop).
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::Communicate(1),
        Op::AdvanceAnimations { msec_delta: 100 },
    ];
    let layout = check_ops_with_options(linear_resize_options(), ops);
    let obs = observe_active_scrolling(&layout);
    assert!(obs.policy.maximize_exclusive);
    assert!(
        obs.lifecycle_overlays_rendered,
        "active overlays must not be unrendered while progress clocks run"
    );
    assert_ne!(
        obs.policy.scrolling_lifecycle_overlays,
        LifecycleOverlayAction::Pause
    );
    assert_ne!(
        obs.policy.scrolling_lifecycle_overlays,
        LifecycleOverlayAction::Cancel
    );
}
