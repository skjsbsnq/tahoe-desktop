//! R00 observation helpers: read production maximize / lifecycle overlay state
//! without changing the render decision.

use super::*;
use crate::layout::scrolling::{
    LifecycleOverlayKind, MaximizeTransitionObservation, ScrollingRenderObservation,
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
        MaximizeTransitionObservation::Pending
    );
    assert_eq!(obs.maximize_target, Some(1));
    // R00 does not create fake overlay state machines; empty overlays still report the
    // production visibility decision (suppressed while maximize is ongoing).
    assert!(obs.lifecycle_overlays.is_empty());

    Op::AdvanceAnimations { msec_delta: 1001 }.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::TimedOut
    );
    assert_eq!(obs.maximize_target, None);

    Op::Communicate(1).apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::Committed
    );
    assert_eq!(obs.maximize_target, Some(1));

    Op::CompleteAnimations.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert_eq!(obs.maximize_target, None);
}

#[test]
fn maximize_ongoing_suppresses_lifecycle_overlay_render_decision() {
    // Construct maximize transition ongoing. Layout-level minimize without a renderer does
    // not populate snapshot containers (production needs GlesRenderer for Genie entries).
    // The overlay *decision* is still readable from production state and must report the
    // current F01 behavior: suppressed while maximize transition is ongoing.
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
    // Current production decision: maximize transition suppresses lifecycle overlays.
    assert!(!obs.lifecycle_overlays_rendered);

    // Non-target minimize still changes model state; snapshot lane stays empty without GPU.
    Op::MinimizeWindow(2).apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_target, Some(1));
    assert!(
        matches!(
            obs.maximize_transition,
            MaximizeTransitionObservation::Pending | MaximizeTransitionObservation::Committed
        ),
        "maximize transition still ongoing: {:?}",
        obs.maximize_transition
    );
    assert!(!obs.lifecycle_overlays_rendered);
    assert!(obs.lifecycle_overlays.is_empty());

    // After canceling maximize, decision must flip back to drawn even with empty containers.
    Op::MaximizeWindowToEdges { id: Some(1) }.apply(&mut layout);
    let obs = observe_active_scrolling(&layout);
    assert_eq!(obs.maximize_transition, MaximizeTransitionObservation::Idle);
    assert!(obs.lifecycle_overlays_rendered);
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
