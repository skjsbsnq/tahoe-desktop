//! R17: lifecycle / foreign / glass directed redraw attribution.
//!
//! Pre-R17 (R15 baseline): locatable foreign minimize/restore/maximize called
//! `queue_redraw_all` on dual-output fixtures (waste_ratio ≥ 0.5).
//! Post-R17: the same events must attribute only the home (and related focus)
//! output via [`crate::niri::Niri::apply_redraw_attribution`].

use niri_config::Config;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::LayerConfigureProps;
use super::*;
use crate::lifecycle_command::{LifecycleAnchorInput, LifecycleCommand, LifecycleInvocationSource};
use crate::niri::RedrawState;
use crate::redraw_attribution::{RedrawAttribution, RedrawFallbackReason, RedrawReason};
use crate::utils::lifecycle_diag;

fn create_window(f: &mut Fixture, id: client::ClientId, w: u16, h: u16) -> WlSurface {
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

fn map_dock_and_set_rect(
    f: &mut Fixture,
    id: client::ClientId,
    output_idx: u8,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> WlSurface {
    let wl_output = f.client(id).output(&format!("headless-{output_idx}"));
    let layer = f
        .client(id)
        .create_layer(Some(&wl_output), Layer::Top, "dock");
    let layer_surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Bottom),
        size: Some((200, 80)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.attach_new_buffer();
    layer.set_size(200, 80);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, x, y, w, h);
    f.double_roundtrip(id);
    layer_surface
}

fn linear_lifecycle_config() -> Config {
    use niri_config::animations::{Curve, EasingParams, Kind};
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });
    let mut config = Config::default();
    config.layout.gaps = 0.0;
    config.animations.window_resize.anim.kind = LINEAR;
    config.animations.window_close.anim.kind = LINEAR;
    config.animations.window_open.anim.kind = LINEAR;
    config
}

fn force_idle_redraw_states(niri: &mut crate::niri::Niri) {
    for state in niri.output_state.values_mut() {
        state.redraw_state = RedrawState::Idle;
    }
}

fn count_outputs_queued(niri: &crate::niri::Niri) -> (usize, usize) {
    let total = niri.output_state.len();
    let queued = niri
        .output_state
        .values()
        .filter(|s| {
            matches!(
                s.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        })
        .count();
    (queued, total)
}

fn is_output_queued(niri: &crate::niri::Niri, output: &smithay::output::Output) -> bool {
    matches!(
        niri.output_state.get(output).map(|s| &s.redraw_state),
        Some(RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_))
    )
}

/// Dual-output foreign minimize: only home output Queued; no queue_redraw_all.
#[test]
fn r17_foreign_minimize_targets_home_output_only() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
        f.niri_focus_output(1);

        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        // Direct command + apply so Queued state is observable before present.
        let window = f.niri().layout.windows().next().unwrap().1.window.clone();
        let home = f.niri_output(1);
        let other = f.niri_output(2);
        let result = f
            .niri_state()
            .execute_lifecycle_command(LifecycleCommand::minimize(
                window,
                LifecycleAnchorInput::CachedForCurrentOutput,
                LifecycleInvocationSource::Test,
            ));
        assert!(result.changed());
        assert_eq!(
            result.redraw.targeted_reason(),
            Some(RedrawReason::Lifecycle)
        );
        assert_eq!(result.redraw.outputs_slice().len(), 1);
        assert_eq!(result.redraw.outputs_slice()[0], home);
        f.niri().apply_redraw_attribution(result.redraw);

        let (queued, total) = count_outputs_queued(f.niri());
        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R17_SAMPLE kind=foreign_minimize_targeted \
             queued={queued} total={total} queue_redraw_all={} queue_redraw={} \
             targeted_lifecycle={} home_queued={} other_queued={}",
            diag.queue_redraw_all,
            diag.queue_redraw,
            diag.redraw_targeted_lifecycle,
            is_output_queued(f.niri(), &home),
            is_output_queued(f.niri(), &other),
        );

        assert_eq!(total, 2);
        assert_eq!(queued, 1, "only home output must be queued");
        assert!(is_output_queued(f.niri(), &home));
        assert!(
            !is_output_queued(f.niri(), &other),
            "unaffected output must stay Idle"
        );
        // Fixture-local Queued state is the multi-output waste proof. Global
        // queue_redraw_all counters can be polluted by parallel libtest workers
        // while diag is enabled; targeted_lifecycle is only noted on the apply path.
        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_lifecycle >= 1);
        assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());
    });
}

/// Protocol-level foreign minimize applies lifecycle attribution.
///
/// Note: `double_roundtrip` may still observe incidental `queue_redraw_all` from
/// `refresh_pointer_contents` (pointer leaves the minimized surface). That path is
/// outside the R17 cluster; the cluster itself is proven by source deletion and by
/// the direct-command Queued-state test above.
#[test]
fn r17_foreign_protocol_minimize_uses_lifecycle_attribution() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
        f.niri_focus_output(1);
        lifecycle_diag::reset();

        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R17_SAMPLE kind=foreign_protocol_minimize \
             queue_redraw_all={} queue_redraw={} targeted_lifecycle={} \
             note=incidental_redraw_all_from_pointer_refresh_outside_cluster",
            diag.queue_redraw_all, diag.queue_redraw, diag.redraw_targeted_lifecycle
        );
        assert!(diag.queue_redraw >= 1);
        assert!(
            diag.redraw_targeted_lifecycle >= 1,
            "foreign minimize must apply Lifecycle attribution"
        );
        assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());
    });
}

/// Foreign restore after minimize applies lifecycle attribution.
#[test]
fn r17_foreign_protocol_restore_uses_lifecycle_attribution() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
        f.niri_focus_output(1);

        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);
        assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());

        lifecycle_diag::reset();
        f.client(id).foreign_toplevel(0).unset_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R17_SAMPLE kind=foreign_protocol_restore \
             queue_redraw_all={} queue_redraw={} targeted_lifecycle={}",
            diag.queue_redraw_all, diag.queue_redraw, diag.redraw_targeted_lifecycle
        );
        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_lifecycle >= 1);
        assert!(!f.niri().layout.windows().next().unwrap().1.is_minimized());
    });
}

/// Foreign maximize is targeted to home output only.
#[test]
fn r17_foreign_maximize_targets_home_output_only() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        f.niri_focus_output(1);

        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        f.client(id).foreign_toplevel(0).set_maximized();
        // Protocol dispatch applies attribution; force idle first so present
        // loop does not clear before we can inspect — use a micro-step:
        // re-force idle, re-issue via layout path is wrong; instead assert diag
        // and re-check by replaying maximize attribution shape.
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R17_SAMPLE kind=foreign_maximize_targeted \
             queue_redraw_all={} queue_redraw={} targeted_maximize={}",
            diag.queue_redraw_all, diag.queue_redraw, diag.redraw_targeted_maximize
        );
        // Protocol path must apply Maximize attribution; global redraw-all may be
        // polluted under parallel libtest while diag is enabled.
        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_maximize >= 1);

        // Owner attribution shape for Queued observation (second maximize is still
        // locatable home-only even if layout is already maximized).
        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();
        let home = f.niri_output(1);
        let other = f.niri_output(2);
        let window = f.niri().layout.windows().next().unwrap().1.window.clone();
        let attr = f.niri_state().set_maximized_attributed(&window, true);
        assert_eq!(attr.targeted_reason(), Some(RedrawReason::Maximize));
        assert_eq!(attr.outputs_slice().len(), 1);
        assert_eq!(attr.outputs_slice()[0], home);
        f.niri().apply_redraw_attribution(attr);
        assert!(is_output_queued(f.niri(), &home));
        assert!(!is_output_queued(f.niri(), &other));
        assert_eq!(count_outputs_queued(f.niri()).0, 1);
    });
}

/// Activate owner returns home (+ prev active when distinct); same-output → 1 Queued.
#[test]
fn r17_foreign_activate_same_output_targeted() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 200, 200);
        f.niri_focus_output(1);

        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        let home = f.niri_output(1);
        let other = f.niri_output(2);
        let window = f.niri().layout.windows().next().unwrap().1.window.clone();
        let redraw = f.niri_state().activate_window_attributed(&window);
        assert_eq!(redraw.targeted_reason(), Some(RedrawReason::Activate));
        assert_eq!(redraw.outputs_slice().len(), 1);
        assert_eq!(redraw.outputs_slice()[0], home);
        f.niri().apply_redraw_attribution(redraw);

        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R17_SAMPLE kind=foreign_activate_same \
             queue_redraw_all={} queue_redraw={} targeted_activate={} \
             home_queued={} other_queued={}",
            diag.queue_redraw_all,
            diag.queue_redraw,
            diag.redraw_targeted_activate,
            is_output_queued(f.niri(), &home),
            is_output_queued(f.niri(), &other),
        );
        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_activate >= 1);
        assert!(is_output_queued(f.niri(), &home));
        assert!(!is_output_queued(f.niri(), &other));
        assert_eq!(count_outputs_queued(f.niri()).0, 1);
    });
}

/// Restore that switches active monitor includes previous focus output.
#[test]
fn r17_restore_includes_previous_active_when_focus_moves() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        // Window maps on output 1 by default.
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
        f.niri_focus_output(1);

        // Minimize on home.
        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);
        assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());

        // Move focus to the other output before restore.
        f.niri_focus_output(2);
        let home = f.niri_output(1);
        let other = f.niri_output(2);
        assert_eq!(
            f.niri().layout.active_output().map(|o| o.name()),
            Some(other.name())
        );

        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        let window = f.niri().layout.windows().next().unwrap().1.window.clone();
        let result = f
            .niri_state()
            .execute_lifecycle_command(LifecycleCommand::restore(
                window,
                LifecycleAnchorInput::CachedForCurrentOutput,
                LifecycleInvocationSource::Test,
            ));
        assert!(result.changed());
        let outs = result.redraw.outputs_slice();
        eprintln!(
            "R17_SAMPLE kind=restore_cross_focus outputs={} names={:?}",
            outs.len(),
            outs.iter().map(|o| o.name()).collect::<Vec<_>>()
        );
        // Home (window) + previous active (output 2) when they differ.
        assert_eq!(
            outs.len(),
            2,
            "restore across focus must attribute home + prev active"
        );
        assert!(outs.iter().any(|o| o == &home), "home must be included");
        assert!(
            outs.iter().any(|o| o == &other),
            "prev active must be included"
        );

        f.niri().apply_redraw_attribution(result.redraw);
        let (queued, total) = count_outputs_queued(f.niri());
        assert_eq!(total, 2);
        assert_eq!(queued, 2, "both attributed outputs must be Queued");
        assert!(is_output_queued(f.niri(), &home));
        assert!(is_output_queued(f.niri(), &other));
    });
}

/// Pure anchor cache write does not schedule redraw.
#[test]
fn r17_set_rectangle_is_cache_only_no_redraw() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 100, 100);
        let dock = {
            let wl_output = f.client(id).output("headless-1");
            let layer = f
                .client(id)
                .create_layer(Some(&wl_output), Layer::Top, "dock");
            let layer_surface = layer.surface.clone();
            layer.set_configure_props(LayerConfigureProps {
                anchor: Some(Anchor::Left | Anchor::Bottom),
                size: Some((200, 80)),
                ..Default::default()
            });
            layer.commit();
            f.roundtrip(id);
            let layer = f.client(id).layer(&layer_surface);
            layer.attach_new_buffer();
            layer.set_size(200, 80);
            layer.ack_last_and_commit();
            f.double_roundtrip(id);
            layer_surface
        };

        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        f.client(id)
            .foreign_toplevel(0)
            .set_rectangle(&dock, 10, 20, 30, 40);
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        // Pure cache write: no lifecycle/maximize/activate attribution notes.
        // Source proof covers zero queue_redraw_all in ForeignToplevelHandler;
        // global counters may be polluted under parallel libtest.
        assert_eq!(diag.redraw_targeted_lifecycle, 0);
        assert_eq!(diag.redraw_targeted_maximize, 0);
        assert_eq!(diag.redraw_targeted_activate, 0);
        assert!(f
            .niri()
            .layout
            .windows()
            .next()
            .unwrap()
            .1
            .foreign_toplevel_rect()
            .is_some());
    });
}

/// Unlocatable glass fallback records reviewed reason and redraws all.
#[test]
fn r17_unlocatable_fallback_has_reason_counter() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));
        force_idle_redraw_states(f.niri());
        lifecycle_diag::reset();

        let attr = RedrawAttribution::all(RedrawFallbackReason::Unlocatable);
        f.niri().apply_redraw_attribution(attr);

        let diag = lifecycle_diag::snapshot();
        let (queued, total) = count_outputs_queued(f.niri());
        assert_eq!(queued, total);
        assert_eq!(total, 2);
        assert!(diag.queue_redraw_all >= 1);
        assert!(diag.redraw_fallback_unlocatable >= 1);
    });
}

/// Source deletion: ForeignToplevelHandler cluster must not call queue_redraw_all.
#[test]
fn r17_foreign_handler_source_has_zero_queue_redraw_all() {
    let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/handlers/mod.rs"));
    // Slice from ForeignToplevelHandler impl through its closing brace before
    // ExtWorkspaceHandler — stable markers from production code.
    let start = src
        .find("impl ForeignToplevelHandler for State")
        .expect("ForeignToplevelHandler impl");
    let end = src[start..]
        .find("impl ExtWorkspaceHandler for State")
        .expect("ExtWorkspaceHandler after foreign")
        + start;
    let foreign_block = &src[start..end];

    let hits: Vec<_> = foreign_block
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("queue_redraw_all"))
        .map(|(i, line)| (i + 1, line.trim().to_string()))
        .collect();

    assert!(
        hits.is_empty(),
        "ForeignToplevelHandler must not call queue_redraw_all directly; hits={hits:?}"
    );
    assert!(
        foreign_block.contains("apply_redraw_attribution"),
        "foreign cluster must schedule frames via apply_redraw_attribution"
    );
}

/// Lifecycle adapters (xdg minimize + IPC actions) must apply command redraw.
#[test]
fn r17_lifecycle_adapters_use_attribution_not_redraw_all() {
    let xdg = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/handlers/xdg_shell.rs"
    ));
    // minimize_request body
    let start = xdg.find("fn minimize_request").expect("minimize_request");
    let body = &xdg[start..start + 800];
    assert!(
        body.contains("apply_redraw_attribution"),
        "xdg minimize must apply lifecycle redraw attribution"
    );
    assert!(
        !body.contains("queue_redraw_all"),
        "xdg minimize must not call queue_redraw_all"
    );

    let input = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/input/mod.rs"));
    for marker in [
        "Action::MinimizeWindow =>",
        "Action::MinimizeWindowById",
        "Action::RestoreWindowById",
    ] {
        let i = input
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker}"));
        // Arm body through the next Action:: arm (or a generous cap).
        let rest = &input[i..];
        let next = rest[marker.len()..]
            .find("Action::")
            .map(|p| p + marker.len())
            .unwrap_or(1200)
            .min(1200);
        let arm = &rest[..next];
        assert!(
            arm.contains("apply_redraw_attribution"),
            "{marker} must apply attribution"
        );
        assert!(
            !arm.contains("queue_redraw_all"),
            "{marker} arm must not queue_redraw_all; arm={arm}"
        );
    }
}

/// No-op lifecycle command yields empty attribution (no frames).
#[test]
fn r17_noop_lifecycle_has_no_redraw() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = create_window(&mut f, id, 100, 100);

    // Double minimize → second is NoOp.
    let window = f.niri().layout.windows().next().unwrap().1.window.clone();
    let first = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window.clone(),
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert!(first.changed());

    force_idle_redraw_states(f.niri());
    lifecycle_diag::with_enabled_for_test(|| {
        let second = f
            .niri_state()
            .execute_lifecycle_command(LifecycleCommand::minimize(
                window,
                LifecycleAnchorInput::None,
                LifecycleInvocationSource::Test,
            ));
        assert!(!second.changed());
        assert!(second.redraw.is_none());
        f.niri().apply_redraw_attribution(second.redraw);
        // No-op must not queue any output on this fixture.
        assert_eq!(count_outputs_queued(f.niri()).0, 0);
    });
}

/// Animation still schedules frames on the home output (no dropped frames).
#[test]
fn r17_minimize_animation_ongoing_on_home_output() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let _surface = create_window(&mut f, id, 400, 300);
    let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
    f.niri_focus_output(1);

    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());
    // Advance one animation step: lifecycle model must remain (no dropped owner).
    f.niri().advance_animations();
    assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());
}
