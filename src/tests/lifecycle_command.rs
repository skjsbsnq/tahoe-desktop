//! R03 F04: single internal lifecycle command equivalence across entry adapters.
//!
//! Foreign-toplevel, IPC, and xdg-toplevel must only parse requests; cache selection,
//! snapshot/fallback, and model updates are owned by `State::execute_lifecycle_command`.

use niri_config::Action;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::LayerConfigureProps;
use super::*;
use crate::layout::scrolling::LifecycleOverlayKind;
use crate::layout::MinimizeRect;
use crate::lifecycle_command::{
    LifecycleAnchorInput, LifecycleCommand, LifecycleCommandOutcome, LifecycleInvocationSource,
};
use crate::niri::State;
use crate::window::Mapped;

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

fn mapped0(f: &mut Fixture) -> &Mapped {
    f.niri().layout.windows().next().unwrap().1
}

fn window_id(f: &mut Fixture) -> u64 {
    mapped0(f).id().get()
}

fn is_minimized(f: &mut Fixture) -> bool {
    mapped0(f).is_minimized()
}

fn observe_has_minimize_overlay(state: &State) -> bool {
    state
        .niri
        .layout
        .active_workspace()
        .map(|ws| {
            ws.scrolling()
                .render_observation()
                .lifecycle_overlays
                .iter()
                .any(|o| o.kind == LifecycleOverlayKind::Minimize && o.active)
        })
        .unwrap_or(false)
}

/// Same window + same cached dock rect: foreign, IPC, and xdg all reach one command owner
/// and leave the same minimized model state. Command is also exercised with Explicit and
/// CachedForCurrentOutput to prove adapters do not need separate strategy code.
#[test]
fn foreign_ipc_xdg_share_cached_anchor_and_final_state() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = create_window(&mut f, id, 200, 200);
    let _dock = map_dock_and_set_rect(&mut f, id, 1, 10, 20, 48, 48);

    let stored = mapped0(&mut f).foreign_toplevel_rect().unwrap().clone();
    let expected_rect = stored.rect.as_rect();

    // --- foreign path ---
    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);
    assert!(is_minimized(&mut f), "foreign minimize must apply");
    assert_eq!(
        mapped0(&mut f)
            .foreign_toplevel_rect()
            .unwrap()
            .rect
            .as_rect(),
        expected_rect,
        "minimize must not clear dock cache"
    );

    f.client(id).foreign_toplevel(0).unset_minimized();
    f.double_roundtrip(id);
    assert!(!is_minimized(&mut f), "foreign restore must apply");

    // --- IPC path (CachedForCurrentOutput, same as production Action handler) ---
    let mapped_id = window_id(&mut f);
    f.niri_state()
        .do_action(Action::MinimizeWindowById(mapped_id), true);
    assert!(
        is_minimized(&mut f),
        "IPC minimize must apply with same cache policy"
    );
    f.niri_state()
        .do_action(Action::RestoreWindowById(mapped_id), true);
    assert!(
        !is_minimized(&mut f),
        "IPC restore must apply with same cache policy"
    );

    // --- xdg path ---
    f.client(id).window(&surface).set_minimized();
    f.double_roundtrip(id);
    assert!(is_minimized(&mut f), "xdg minimize must apply");

    // Explicit restore via command (xdg has no unset_minimized wire) must clear model.
    let window = mapped0(&mut f).window.clone();
    let result = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window,
            LifecycleAnchorInput::CachedForCurrentOutput,
            LifecycleInvocationSource::Test,
        ));
    assert!(result.changed());
    assert!(!is_minimized(&mut f));

    // Direct command with Explicit rect equals Cached resolution for the same geometry.
    let window = mapped0(&mut f).window.clone();
    let output = f.niri_output(1);
    let explicit = MinimizeRect {
        output: output.clone(),
        rect: crate::layout::coords::OutputLocalRect::from_rect(expected_rect),
    };
    assert_eq!(explicit.rect.as_rect(), expected_rect);

    let r1 = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window.clone(),
            LifecycleAnchorInput::Explicit(explicit),
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(r1.outcome, LifecycleCommandOutcome::Applied);
    assert!(is_minimized(&mut f));

    let r2 = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window,
            LifecycleAnchorInput::CachedForCurrentOutput,
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(r2.outcome, LifecycleCommandOutcome::Applied);
    assert!(!is_minimized(&mut f));
}

#[test]
fn duplicate_minimize_restore_are_noop() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);

    let window = mapped0(&mut f).window.clone();
    let first = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window.clone(),
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(first.outcome, LifecycleCommandOutcome::Applied);

    let second = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window.clone(),
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(second.outcome, LifecycleCommandOutcome::NoOp);
    assert!(is_minimized(&mut f));

    let rest = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window.clone(),
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(rest.outcome, LifecycleCommandOutcome::Applied);
    assert!(!is_minimized(&mut f));

    let rest2 = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window,
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert_eq!(rest2.outcome, LifecycleCommandOutcome::NoOp);
}

#[test]
fn restore_without_anchor_still_updates_model_via_same_command() {
    let mut f = Fixture::new();
    // No headless renderer: proves renderer-unavailable is normal fallback, not another API.
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);

    let window = mapped0(&mut f).window.clone();
    let min = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window.clone(),
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert!(min.changed());
    assert!(is_minimized(&mut f));
    assert!(
        !observe_has_minimize_overlay(f.niri_state()),
        "without renderer there is no Genie overlay; model still minimized"
    );

    let rest = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::restore(
            window,
            LifecycleAnchorInput::None,
            LifecycleInvocationSource::Test,
        ));
    assert!(rest.changed());
    assert!(!is_minimized(&mut f));
}

#[test]
fn wrong_output_cached_anchor_degrades_to_no_anchor_but_still_minimizes() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);
    // Publish rect on output 2 while the window lives on output 1 (default).
    let _dock = map_dock_and_set_rect(&mut f, id, 2, 10, 20, 48, 48);

    let output2 = f.niri_output(2);
    let output1 = f.niri_output(1);
    let rect_output = mapped0(&mut f)
        .foreign_toplevel_rect()
        .unwrap()
        .output
        .clone();
    assert_eq!(rect_output, output2);
    // Window is on headless-1 by default (first output).
    let window_output = f
        .niri()
        .layout
        .windows()
        .next()
        .and_then(|(mon, _)| mon.map(|m| m.output().clone()))
        .expect("window has an output");
    assert_eq!(window_output, output1);

    let window = mapped0(&mut f).window.clone();
    let result = f
        .niri_state()
        .execute_lifecycle_command(LifecycleCommand::minimize(
            window,
            LifecycleAnchorInput::CachedForCurrentOutput,
            LifecycleInvocationSource::Test,
        ));
    assert!(result.changed());
    assert!(is_minimized(&mut f));
    // Cache remains the protocol last-request fact; only animation consumption degraded.
    assert_eq!(
        mapped0(&mut f).foreign_toplevel_rect().unwrap().output,
        output2
    );
}

#[test]
fn ipc_and_foreign_both_consume_same_cache_slot() {
    // F04 regression: IPC must not hardcode None and skip the dock cache.
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);
    let _dock = map_dock_and_set_rect(&mut f, id, 1, 10, 20, 48, 48);

    assert!(mapped0(&mut f).foreign_toplevel_rect().is_some());

    let mapped_id = window_id(&mut f);
    f.niri_state()
        .do_action(Action::MinimizeWindowById(mapped_id), true);
    assert!(is_minimized(&mut f));

    // With renderer + matching cache, Genie minimize overlay should be active
    // (same as foreign would produce).
    assert!(
        observe_has_minimize_overlay(f.niri_state()),
        "IPC CachedForCurrentOutput must create minimize overlay when dock rect matches"
    );
}
