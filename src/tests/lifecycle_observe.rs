//! R00 headless fixture helpers: real Mapped serial path, multi-output geometry,
//! source layer lifecycle, and production render-observation of maximize + Genie.

use std::time::Duration;

use niri_config::animations::{Curve, EasingParams, Kind};
use niri_config::Config;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{Point, Rectangle, Size, Transform};
use smithay::wayland::shell::xdg::ToplevelSurface;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::{ClientId, LayerConfigureProps};
use super::*;
use crate::layout::scrolling::{
    LifecycleOverlayAction, LifecycleOverlayKind, MaximizeTransitionObservation,
    ScrollingRenderObservation,
};
use crate::lifecycle_command::{LifecycleAnchorInput, LifecycleCommand, LifecycleInvocationSource};
use crate::niri::Niri;
use crate::utils::lifecycle_diag;
use crate::utils::transaction::TransactionBlocker;
use crate::window::Mapped;

fn lifecycle_minimize(f: &mut Fixture, window: Window) -> bool {
    f.niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window,
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ))
        .changed()
}

fn lifecycle_restore(f: &mut Fixture, window: Window) -> bool {
    f.niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window,
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ))
        .changed()
}

fn set_time(niri: &mut Niri, time: Duration) {
    let now = niri.clock.now();
    niri.clock.set_unadjusted(now);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(Duration::ZERO);
    niri.clock.set_rate(1.0);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(time);
    let _ = niri.clock.now();
    niri.clock.set_rate(0.0);
}

fn create_window(f: &mut Fixture, id: ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// Layout window order is creation order for these simple fixtures.
fn mapped_at(niri: &Niri, idx: usize) -> &Mapped {
    niri.layout
        .windows()
        .nth(idx)
        .map(|(_, mapped)| mapped)
        .expect("mapped window at index")
}

fn observe_scrolling(niri: &Niri) -> ScrollingRenderObservation<Window> {
    niri.layout
        .active_workspace()
        .expect("active workspace")
        .scrolling()
        .render_observation()
}

#[test]
fn real_mapped_serial_distinguishes_configure_ack_and_commit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = create_window(&mut f, id, 200, 200);
    let _ = f.client(id).window(&surface).recent_configures();

    let window = mapped_at(f.niri(), 0).window.clone();

    // Request maximize through the real layout path (sends configure via Mapped).
    f.niri().layout.set_maximized(&window, true);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).configures_received.clone();
    assert!(
        !configures.is_empty(),
        "maximize must produce at least one configure"
    );
    let latest_client_serial = configures.last().unwrap().0;

    let (committed, pending, uncommitted) = mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(
        !committed,
        "before client commit, maximized is not committed"
    );
    assert!(pending, "maximize request is pending on Mapped");
    assert!(
        !uncommitted.is_empty(),
        "Mapped must queue uncommitted_maximized serials (production path, not layout mock)"
    );
    assert!(
        uncommitted.iter().any(|(_, maximized)| *maximized),
        "queue must record maximized=true for the pending configure: {uncommitted:?}"
    );
    let queued_before = uncommitted.clone();

    // Ack alone must not apply committed maximized (only commit does).
    f.client(id).window(&surface).ack_last();
    f.roundtrip(id);
    let (committed, pending, uncommitted_after_ack) =
        mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(!committed, "ack without commit must not apply maximized");
    assert!(pending);
    assert_eq!(
        uncommitted_after_ack, queued_before,
        "ack alone must not drain uncommitted_maximized"
    );

    // Commit after the previous ack (do not re-ack the same serial).
    let window_client = f.client(id).window(&surface);
    window_client.attach_new_buffer();
    window_client.set_size(1920, 1080);
    window_client.commit();
    f.roundtrip(id);

    let (committed, _pending, uncommitted_after_commit) =
        mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(
        committed,
        "commit of latest configure (client serial {latest_client_serial}) must apply maximized via Mapped::on_commit"
    );
    assert!(
        uncommitted_after_commit
            .iter()
            .all(|(serial, _)| !queued_before.iter().any(|(s, _)| s == serial)),
        "commit must consume the previously queued uncommitted_maximized serials: before={queued_before:?} after={uncommitted_after_commit:?}"
    );

    // Sanity: still a real Mapped (not layout TestWindow).
    let _: &ToplevelSurface = mapped_at(f.niri(), 0).toplevel();
}

/// Old configure serial vs latest: commit of an earlier maximize serial must not clear a
/// newer unmaximize entry, and the latest commit decides the final committed maximized flag.
#[test]
fn real_mapped_serial_old_configure_vs_latest_commit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = create_window(&mut f, id, 200, 200);
    let _ = f.client(id).window(&surface).recent_configures();

    let window = mapped_at(f.niri(), 0).window.clone();

    // Configure epoch A: maximize.
    f.niri().layout.set_maximized(&window, true);
    f.double_roundtrip(id);
    let serial_a = f
        .client(id)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .0;
    let (committed, pending, uncommitted) = mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(!committed);
    assert!(pending);
    assert!(
        uncommitted.iter().any(|(_, m)| *m),
        "epoch A must queue maximized=true: {uncommitted:?}"
    );
    let uncommitted_after_a = uncommitted.clone();

    // Configure epoch B: unmaximize before the client commits A.
    f.niri().layout.set_maximized(&window, false);
    f.double_roundtrip(id);
    let serial_b = f
        .client(id)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .0;
    assert_ne!(
        serial_a, serial_b,
        "unmaximize must send a new configure serial"
    );

    let (committed, pending, uncommitted) = mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(!committed, "still uncommitted before any client commit");
    assert!(!pending, "pending follows the latest unmaximize request");
    assert!(
        uncommitted.len() >= 2,
        "both old maximize and latest unmaximize serials must remain queued: after_a={uncommitted_after_a:?} now={uncommitted:?}"
    );
    assert!(
        uncommitted.iter().any(|(_, m)| *m) && uncommitted.iter().any(|(_, m)| !*m),
        "queue must hold both maximized=true and maximized=false entries: {uncommitted:?}"
    );
    let has_newer_false = uncommitted.iter().any(|(serial, m)| {
        !*m && serial.is_no_older_than(&uncommitted.iter().find(|(_, m)| *m).unwrap().0)
    });
    assert!(
        has_newer_false || uncommitted.last().is_some_and(|(_, m)| !*m),
        "latest queued value should be unmaximize (false): {uncommitted:?}"
    );

    // Commit only the *old* maximize configure (ack A, not B).
    {
        let w = f.client(id).window(&surface);
        w.ack_serial(serial_a);
        w.attach_new_buffer();
        w.set_size(1920, 1080);
        w.commit();
    }
    f.roundtrip(id);

    let (committed, pending, uncommitted_after_old) =
        mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(
        committed,
        "commit of old maximize serial A must apply maximized=true via on_commit"
    );
    assert!(!pending);
    assert!(
        uncommitted_after_old.iter().any(|(_, m)| !*m),
        "newer unmaximize serial B must still be waiting after old commit: {uncommitted_after_old:?}"
    );
    let a_true_serials: Vec<_> = uncommitted_after_a
        .iter()
        .filter(|(_, m)| *m)
        .map(|(s, _)| *s)
        .collect();
    assert!(
        a_true_serials
            .iter()
            .all(|sa| !uncommitted_after_old.iter().any(|(s, _)| s == sa)),
        "epoch A maximize serial(s) must leave the queue after commit of A: a={a_true_serials:?} after={uncommitted_after_old:?}"
    );

    // Commit the *latest* unmaximize configure B.
    {
        let w = f.client(id).window(&surface);
        w.ack_serial(serial_b);
        w.attach_new_buffer();
        w.set_size(200, 200);
        w.commit();
    }
    f.roundtrip(id);

    let (committed, pending, uncommitted_final) =
        mapped_at(f.niri(), 0).test_maximize_commit_state();
    assert!(
        !committed,
        "commit of latest unmaximize serial B must leave committed maximized=false"
    );
    assert!(!pending);
    assert!(
        uncommitted_final.is_empty()
            || uncommitted_final
                .iter()
                .all(|(s, _)| !uncommitted_after_old.iter().any(|(so, _)| so == s)),
        "latest commit must consume the remaining unmaximize serial(s): {uncommitted_final:?}"
    );
}

#[test]
fn dual_output_fractional_scale_and_transform_fixture() {
    let mut f = Fixture::new();
    f.add_output_with_scale_transform(1, (1920, 1080), 1.25, Transform::Normal);
    f.add_output_with_scale_transform(2, (1280, 720), 2.0, Transform::_90);

    let o1 = f.niri_output(1);
    let o2 = f.niri_output(2);
    assert_eq!(o1.current_mode().unwrap().size, Size::from((1920, 1080)));
    assert_eq!(o2.current_mode().unwrap().size, Size::from((1280, 720)));
    assert!((o1.current_scale().fractional_scale() - 1.25).abs() < f64::EPSILON);
    assert!((o2.current_scale().fractional_scale() - 2.0).abs() < f64::EPSILON);
    assert_eq!(o1.current_transform(), Transform::Normal);
    assert_eq!(o2.current_transform(), Transform::_90);

    // Layout owns both monitors after real add_output path.
    assert_eq!(f.niri().layout.outputs().count(), 2);
}

#[test]
fn source_layer_map_unmap_remap_clears_and_restores_anchor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let _window_surface = create_window(&mut f, id, 100, 100);

    let props = LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Bottom),
        size: Some((200, 80)),
        ..Default::default()
    };
    let layer_surface = f.map_layer(id, Some(2), Layer::Top, "dock", props, (200, 80));

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);

    let output2 = f.niri_output(2);
    {
        let mapped = mapped_at(f.niri(), 0);
        let rect = mapped
            .foreign_toplevel_rect()
            .expect("anchor after set_rectangle");
        assert_eq!(rect.output, output2);
        assert_eq!(
            rect.rect.as_rect(),
            Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
        );
    }

    // Unmap source layer: production clears rectangle for that source.
    f.unmap_layer(id, &layer_surface);
    assert!(
        mapped_at(f.niri(), 0).foreign_toplevel_rect().is_none(),
        "unmap must clear foreign-toplevel rectangle for the source layer"
    );

    // Remap and re-publish rectangle.
    f.remap_layer(id, &layer_surface, props, (200, 80));
    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    let output2 = f.niri_output(2);
    {
        let mapped = mapped_at(f.niri(), 0);
        let rect = mapped
            .foreign_toplevel_rect()
            .expect("anchor restored after remap + set_rectangle");
        assert_eq!(rect.output, output2);
        assert_eq!(
            rect.rect.as_rect(),
            Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
        );
    }
}

fn linear_lifecycle_config() -> Config {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });
    let mut config = Config::default();
    config.layout.gaps = 0.0;
    config.animations.window_resize.anim.kind = LINEAR;
    // Minimize/restore default to close/open animation config when unset.
    config.animations.window_close.anim.kind = LINEAR;
    config.animations.window_open.anim.kind = LINEAR;
    config
}

fn assert_no_invisible_progress(obs: &ScrollingRenderObservation<Window>) {
    assert!(
        obs.lifecycle_overlays_rendered,
        "policy must Draw scrolling lifecycle overlays; obs={obs:?}"
    );
    assert_eq!(
        obs.policy.scrolling_lifecycle_overlays,
        LifecycleOverlayAction::Draw
    );
    for overlay in &obs.lifecycle_overlays {
        if overlay.active {
            assert!(
                overlay.rendered,
                "active overlay must not advance while unrendered: {overlay:?}"
            );
        }
    }
}

fn start_maximize_pending(f: &mut Fixture) -> Window {
    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();
    let window1 = mapped_at(f.niri(), 0).window.clone();
    f.niri().layout.activate_window(&window1);
    f.niri().layout.set_maximized(&window1, true);
    // Avoid dispatch/advance_animations: fixture roundtrips advance the real clock and can
    // settle or time out maximize transitions before we observe them.
    let obs = observe_scrolling(f.niri());
    assert_ne!(
        obs.maximize_transition,
        MaximizeTransitionObservation::Idle,
        "maximize transition must be ongoing before client commit; obs={obs:?}"
    );
    assert!(obs.policy.maximize_exclusive);
    assert_no_invisible_progress(&obs);
    window1
}

#[test]
fn maximize_ongoing_plus_active_minimize_overlay_is_drawn() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));

        let id = f.add_client();
        let surface1 = create_window(&mut f, id, 200, 200);
        let surface2 = create_window(&mut f, id, 200, 200);
        f.double_roundtrip(id);
        let _ = f.client(id).window(&surface1).recent_configures();
        let _ = f.client(id).window(&surface2).recent_configures();

        let _window1 = start_maximize_pending(&mut f);

        // Minimize non-target with real Genie snapshot (renderer present).
        let window2 = mapped_at(f.niri(), 1).window.clone();
        let changed = lifecycle_minimize(&mut f, window2);
        assert!(changed, "minimize of non-target must succeed");

        let obs = observe_scrolling(f.niri());
        assert_no_invisible_progress(&obs);
        assert!(obs.policy.maximize_exclusive);
        let minimize = obs
            .lifecycle_overlays
            .iter()
            .find(|o| o.kind == LifecycleOverlayKind::Minimize)
            .expect("active minimize overlay must be observed from production containers");
        assert!(minimize.active);
        assert!(minimize.rendered);
        assert!(minimize.progress.is_some());

        // Advance clock: progress grows while still rendered (F01 fixed).
        let p0 = minimize.progress.unwrap();
        set_time(f.niri(), Duration::from_millis(250));
        f.niri().advance_animations();
        let obs = observe_scrolling(f.niri());
        assert_no_invisible_progress(&obs);
        let minimize = obs
            .lifecycle_overlays
            .iter()
            .find(|o| o.kind == LifecycleOverlayKind::Minimize)
            .expect("minimize still active after partial advance");
        let p1 = minimize.progress.unwrap();
        assert!(p1 > p0, "progress must grow while drawn (p0={p0}, p1={p1})");
        assert!(minimize.rendered);

        let diag = lifecycle_diag::snapshot();
        assert!(
            diag.genie_create >= 1,
            "opt-in diag must count Genie snapshot creation when enabled"
        );
        assert!(diag.snapshot_peak_bytes > 0);
    });
}

#[test]
fn maximize_pending_draws_minimize_restore_reverse_and_close() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface1 = create_window(&mut f, id, 200, 200);
    let surface2 = create_window(&mut f, id, 200, 200);
    let surface3 = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface1).recent_configures();
    let _ = f.client(id).window(&surface2).recent_configures();
    let _ = f.client(id).window(&surface3).recent_configures();

    let window1 = start_maximize_pending(&mut f);

    // --- pending stage: minimize non-target ---
    let window2 = mapped_at(f.niri(), 1).window.clone();
    assert!(lifecycle_minimize(&mut f, window2.clone()));
    let obs = observe_scrolling(f.niri());
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::Pending
    );
    assert_no_invisible_progress(&obs);
    assert!(obs
        .lifecycle_overlays
        .iter()
        .any(|o| o.kind == LifecycleOverlayKind::Minimize && o.rendered));

    // Advance minimize partway, then reverse to restore while maximize still exclusive.
    set_time(f.niri(), Duration::from_millis(300));
    f.niri().advance_animations();
    let obs = observe_scrolling(f.niri());
    assert_no_invisible_progress(&obs);
    let minimize = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .expect("minimize mid-flight");
    assert!(minimize.progress.unwrap() > 0.0);

    assert!(lifecycle_restore(&mut f, window2));
    let obs = observe_scrolling(f.niri());
    assert_no_invisible_progress(&obs);
    let restore = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Restore)
        .expect("restore after reverse");
    assert!(restore.active && restore.rendered);
    // Restore morph is "how minimized": decreases as the window expands back.
    let p0 = restore.progress.unwrap();
    set_time(f.niri(), Duration::from_millis(550));
    f.niri().advance_animations();
    let obs = observe_scrolling(f.niri());
    assert!(obs.policy.maximize_exclusive);
    assert_no_invisible_progress(&obs);
    let restore = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Restore)
        .expect("restore still active");
    assert!(
        restore.progress.unwrap() < p0,
        "restore morph must decrease while drawn (p0={p0}, p1={})",
        restore.progress.unwrap()
    );

    // Close a different non-target with unmap snapshot while maximize exclusive.
    let window3 = mapped_at(f.niri(), 2).window.clone();
    assert_ne!(&window3, &window1);
    f.niri_state().store_unmap_snapshot(&window3, None);
    {
        let state = f.niri_state();
        state.backend.with_primary_renderer(|renderer| {
            state.niri.layout.start_close_animation_for_window(
                renderer,
                &window3,
                TransactionBlocker::completed(),
            );
        });
    }
    let obs = observe_scrolling(f.niri());
    assert!(obs.policy.maximize_exclusive);
    assert_no_invisible_progress(&obs);
    assert!(
        obs.lifecycle_overlays
            .iter()
            .any(|o| o.kind == LifecycleOverlayKind::Closing && o.rendered),
        "closing overlay must be drawn during maximize: {obs:?}"
    );

    // Live non-target tiles stay exclusive-filtered.
    let target = window1.clone();
    let visible: Vec<bool> = f
        .niri()
        .layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .filter(|(tile, _, _)| tile.window().window != target)
        .map(|(_, _, vis)| vis)
        .collect();
    assert!(
        visible.iter().all(|v| !*v),
        "maximize exclusivity must not re-expose non-target live tiles"
    );
}

#[test]
fn maximize_committed_still_draws_lifecycle_overlays() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface1 = create_window(&mut f, id, 200, 200);
    let surface2 = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface1).recent_configures();
    let _ = f.client(id).window(&surface2).recent_configures();

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    let window1 = mapped_at(f.niri(), 0).window.clone();
    f.niri().layout.activate_window(&window1);
    f.niri().layout.set_maximized(&window1, true);
    f.double_roundtrip(id);

    // Client commits maximized size from the latest configure.
    {
        let win = f.client(id).window(&surface1);
        let cfg = win
            .recent_configures()
            .last()
            .cloned()
            .expect("maximize configure");
        win.set_size(cfg.size.0.max(1) as u16, cfg.size.1.max(1) as u16);
        win.ack_last_and_commit();
    }
    f.roundtrip(id);

    // Freeze clock so transition does not settle during observation.
    set_time(f.niri(), Duration::from_millis(50));
    f.niri().advance_animations();

    let obs = observe_scrolling(f.niri());
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::Committed,
        "after client maximize commit: {obs:?}"
    );
    assert!(obs.policy.maximize_exclusive);
    assert_no_invisible_progress(&obs);

    let window2 = mapped_at(f.niri(), 1).window.clone();
    assert!(lifecycle_minimize(&mut f, window2));
    let obs = observe_scrolling(f.niri());
    assert_eq!(
        obs.maximize_transition,
        MaximizeTransitionObservation::Committed
    );
    assert_no_invisible_progress(&obs);
    let minimize = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .expect("minimize during committed maximize");
    assert!(minimize.rendered && minimize.active);
    let p0 = minimize.progress.unwrap();
    set_time(f.niri(), Duration::from_millis(300));
    f.niri().advance_animations();
    let obs = observe_scrolling(f.niri());
    assert_no_invisible_progress(&obs);
    let minimize = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .expect("minimize still active");
    assert!(minimize.progress.unwrap() > p0);
}

#[test]
fn maximize_hides_floating_live_but_draws_floating_lifecycle_policy() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface1 = create_window(&mut f, id, 200, 200);
    let surface_float = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface1).recent_configures();
    let _ = f.client(id).window(&surface_float).recent_configures();

    let window_float = mapped_at(f.niri(), 1).window.clone();
    f.niri().layout.toggle_window_floating(Some(&window_float));
    f.double_roundtrip(id);

    let _window1 = start_maximize_pending(&mut f);
    {
        let ws = f.niri().layout.active_workspace().unwrap();
        assert!(
            !ws.is_floating_visible(),
            "floating live tiles stay hidden during maximize exclusivity"
        );
        let policy = ws.scrolling().render_policy();
        assert!(policy.suppress_floating_live_tiles);
        assert_eq!(
            policy.floating_lifecycle_overlays,
            LifecycleOverlayAction::Draw
        );
    }

    // Floating minimize while maximize exclusive: policy still Draw for floating overlays.
    assert!(lifecycle_minimize(&mut f, window_float));
    {
        let ws = f.niri().layout.active_workspace().unwrap();
        let policy = ws.scrolling().render_policy();
        assert!(policy.maximize_exclusive);
        assert!(policy.suppress_floating_live_tiles);
        assert!(policy.floating_lifecycle_overlays_are_rendered());
        assert!(!ws.is_floating_visible());
    }
}

#[test]
fn lifecycle_diag_default_off_does_not_count() {
    lifecycle_diag::with_test_lock(|| {
        lifecycle_diag::disable_and_reset();
        assert!(!lifecycle_diag::is_enabled());

        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        // queue_redraw_all happens on various paths; with diag off, snapshot stays zero.
        f.niri().queue_redraw_all();
        assert_eq!(lifecycle_diag::snapshot().queue_redraw_all, 0);
    });
}

/// R05: reverse at 0% / mid / near-complete reuses one overlay (no dual morph jump) and
/// completing restore releases the visibility lease so the live tile can show again.
#[test]
fn reverse_at_progress_points_continuous_and_lease_released() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    // --- reverse near 0% ---
    let window = mapped_at(f.niri(), 0).window.clone();
    assert!(lifecycle_minimize(&mut f, window.clone()));
    set_time(f.niri(), Duration::from_millis(10));
    f.niri().advance_animations();
    let p_min = observe_scrolling(f.niri())
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .and_then(|o| o.progress)
        .expect("minimize near start");
    assert!(lifecycle_restore(&mut f, window.clone()));
    let obs = observe_scrolling(f.niri());
    let restore = obs
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Restore)
        .expect("restore after reverse near 0%");
    // One entry: minimize gone, restore present (same controller entry, reversed).
    assert_eq!(
        obs.lifecycle_overlays
            .iter()
            .filter(|o| {
                o.kind == LifecycleOverlayKind::Minimize || o.kind == LifecycleOverlayKind::Restore
            })
            .count(),
        1
    );
    // Morph is "how minimized" in both directions; reverse reuses the same value (no jump).
    let p_rest = restore.progress.unwrap();
    assert!(
        (p_rest - p_min).abs() < 0.15,
        "near-0 reverse morph jump: min={p_min} rest={p_rest}"
    );

    // Finish restore so next cycle starts clean.
    set_time(f.niri(), Duration::from_millis(2000));
    f.niri().advance_animations();
    assert!(
        observe_scrolling(f.niri())
            .lifecycle_overlays
            .iter()
            .all(|o| {
                o.kind != LifecycleOverlayKind::Minimize && o.kind != LifecycleOverlayKind::Restore
            }),
        "restore must complete and clear controller"
    );
    // Live tile must not remain lease-suppressed after restore completion.
    let suppressed = f
        .niri()
        .layout
        .active_workspace()
        .unwrap()
        .tiles()
        .any(|t| t.is_suppressed_by_restore_lease());
    assert!(
        !suppressed,
        "visibility lease must release when restore finishes"
    );

    // Reset animation clock baseline before the next minimize cycle.
    set_time(f.niri(), Duration::ZERO);

    // --- reverse mid ---
    assert!(lifecycle_minimize(&mut f, window.clone()));
    set_time(f.niri(), Duration::from_millis(450));
    f.niri().advance_animations();
    let p_min = observe_scrolling(f.niri())
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .and_then(|o| o.progress)
        .expect("minimize mid");
    assert!(
        p_min > 0.2 && p_min < 0.9,
        "mid morph expected, got {p_min}"
    );
    assert!(lifecycle_restore(&mut f, window.clone()));
    let p_rest = observe_scrolling(f.niri())
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Restore)
        .and_then(|o| o.progress)
        .expect("restore mid");
    assert!(
        (p_rest - p_min).abs() < 0.15,
        "mid reverse morph jump: min={p_min} rest={p_rest}"
    );

    set_time(f.niri(), Duration::from_millis(2500));
    f.niri().advance_animations();
    set_time(f.niri(), Duration::ZERO);

    // --- reverse near complete ---
    assert!(lifecycle_minimize(&mut f, window.clone()));
    set_time(f.niri(), Duration::from_millis(900));
    f.niri().advance_animations();
    let p_min = observe_scrolling(f.niri())
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Minimize)
        .and_then(|o| o.progress)
        .expect("minimize near end");
    assert!(p_min > 0.7, "near-complete morph expected, got {p_min}");
    assert!(lifecycle_restore(&mut f, window));
    let p_rest = observe_scrolling(f.niri())
        .lifecycle_overlays
        .iter()
        .find(|o| o.kind == LifecycleOverlayKind::Restore)
        .and_then(|o| o.progress)
        .expect("restore near end");
    assert!(
        (p_rest - p_min).abs() < 0.15,
        "near-complete reverse morph jump: min={p_min} rest={p_rest}"
    );

    // Reverse restore → minimize must also keep a single overlay and release lease.
    let window_again = mapped_at(f.niri(), 0).window.clone();
    assert!(lifecycle_minimize(&mut f, window_again));
    let obs = observe_scrolling(f.niri());
    assert_eq!(
        obs.lifecycle_overlays
            .iter()
            .filter(|o| {
                o.kind == LifecycleOverlayKind::Minimize || o.kind == LifecycleOverlayKind::Restore
            })
            .count(),
        1
    );
    assert!(
        obs.lifecycle_overlays
            .iter()
            .any(|o| o.kind == LifecycleOverlayKind::Minimize),
        "reverse back to minimize"
    );
    let suppressed = f
        .niri()
        .layout
        .active_workspace()
        .unwrap()
        .tiles()
        .any(|t| t.is_suppressed_by_restore_lease());
    assert!(
        !suppressed,
        "reverse-to-minimize must release restore lease"
    );
}

/// R06: closing lane starts with completed blocker, draws, then drops entry (texture release).
#[test]
fn closing_overlay_completes_and_releases_on_advance() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    let window = mapped_at(f.niri(), 0).window.clone();
    f.niri_state().store_unmap_snapshot(&window, None);
    {
        let state = f.niri_state();
        state.backend.with_primary_renderer(|renderer| {
            state.niri.layout.start_close_animation_for_window(
                renderer,
                &window,
                TransactionBlocker::completed(),
            );
        });
    }

    let obs = observe_scrolling(f.niri());
    assert_eq!(
        obs.lifecycle_overlays
            .iter()
            .filter(|o| o.kind == LifecycleOverlayKind::Closing)
            .count(),
        1,
        "closing lane must hold one entry after start: {obs:?}"
    );
    assert!(
        obs.lifecycle_overlays
            .iter()
            .any(|o| o.kind == LifecycleOverlayKind::Closing && o.active && o.rendered),
        "closing must be active and drawn under Draw policy: {obs:?}"
    );

    // Mid-animation: still present.
    set_time(f.niri(), Duration::from_millis(400));
    f.niri().advance_animations();
    let obs = observe_scrolling(f.niri());
    assert!(
        obs.lifecycle_overlays
            .iter()
            .any(|o| o.kind == LifecycleOverlayKind::Closing && o.active),
        "closing still active mid-flight: {obs:?}"
    );

    // Past duration: advance drops finished entry → texture released with drop.
    set_time(f.niri(), Duration::from_millis(1100));
    f.niri().advance_animations();
    let obs = observe_scrolling(f.niri());
    assert!(
        obs.lifecycle_overlays
            .iter()
            .all(|o| o.kind != LifecycleOverlayKind::Closing),
        "closing entry must be gone after completion: {obs:?}"
    );
}

/// R06: floating close uses the floating space lane; scrolling observation stays free of it.
#[test]
fn floating_closing_uses_floating_lane_not_scrolling() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface_t = create_window(&mut f, id, 200, 200);
    let surface_f = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface_t).recent_configures();
    let _ = f.client(id).window(&surface_f).recent_configures();

    let window_f = mapped_at(f.niri(), 1).window.clone();
    f.niri().layout.toggle_window_floating(Some(&window_f));
    f.double_roundtrip(id);

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    f.niri_state().store_unmap_snapshot(&window_f, None);
    {
        let state = f.niri_state();
        state.backend.with_primary_renderer(|renderer| {
            state.niri.layout.start_close_animation_for_window(
                renderer,
                &window_f,
                TransactionBlocker::completed(),
            );
        });
    }

    // Scrolling lane must not own the floating close overlay.
    let obs = observe_scrolling(f.niri());
    assert!(
        obs.lifecycle_overlays
            .iter()
            .all(|o| o.kind != LifecycleOverlayKind::Closing),
        "floating close must not appear on scrolling observation: {obs:?}"
    );
    // Floating space reports ongoing animation while the lane holds the entry.
    assert!(
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .floating()
            .are_animations_ongoing(),
        "floating closing lane must report ongoing animation"
    );

    set_time(f.niri(), Duration::from_millis(1100));
    f.niri().advance_animations();
    assert!(
        !f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .floating()
            .are_animations_ongoing(),
        "floating closing lane must clear after completion"
    );
}
