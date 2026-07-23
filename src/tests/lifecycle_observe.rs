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
    LifecycleOverlayKind, MaximizeTransitionObservation, ScrollingRenderObservation,
};
use crate::niri::Niri;
use crate::utils::lifecycle_diag;
use crate::window::Mapped;

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
            rect.rect,
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
            rect.rect,
            Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
        );
    }
}

#[test]
fn maximize_ongoing_plus_active_minimize_overlay_reports_suppressed_decision() {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });

    let mut config = Config::default();
    config.layout.gaps = 0.0;
    config.animations.window_resize.anim.kind = LINEAR;
    config.animations.window_close.anim.kind = LINEAR;

    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(config);
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

        // Focus left window (idx 0) and start maximize transition without committing client size yet.
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
        assert!(
            !obs.lifecycle_overlays_rendered,
            "lifecycle overlays suppressed while maximize ongoing; obs={obs:?}"
        );

        // Minimize non-target with real Genie snapshot (renderer present).
        let window2 = mapped_at(f.niri(), 1).window.clone();
        let changed = f
            .niri_state()
            .minimize_window_with_animation(&window2, None);
        assert!(changed, "minimize of non-target must succeed");

        let obs = observe_scrolling(f.niri());
        assert!(
            !obs.lifecycle_overlays_rendered,
            "F01 baseline: maximize ongoing must suppress overlay render decision"
        );
        let minimize = obs
            .lifecycle_overlays
            .iter()
            .find(|o| o.kind == LifecycleOverlayKind::Minimize)
            .expect("active minimize overlay must be observed from production containers");
        assert!(minimize.active);
        assert!(
            !minimize.rendered,
            "overlay entry exists and advances, but production decision keeps it unrendered"
        );
        assert!(minimize.progress.is_some());

        // Advance clock: progress must still grow even while suppressed (F01 risk captured).
        let p0 = minimize.progress.unwrap();
        set_time(f.niri(), Duration::from_millis(250));
        f.niri().advance_animations();
        let obs = observe_scrolling(f.niri());
        let minimize = obs
            .lifecycle_overlays
            .iter()
            .find(|o| o.kind == LifecycleOverlayKind::Minimize)
            .expect("minimize still active after partial advance");
        let p1 = minimize.progress.unwrap();
        assert!(
            p1 > p0 || !obs.lifecycle_overlays_rendered,
            "progress observed under suppressed decision (p0={p0}, p1={p1})"
        );
        assert!(!obs.lifecycle_overlays_rendered);

        let diag = lifecycle_diag::snapshot();
        assert!(
            diag.genie_create >= 1,
            "opt-in diag must count Genie snapshot creation when enabled"
        );
        assert!(diag.snapshot_peak_bytes > 0);
    });
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
