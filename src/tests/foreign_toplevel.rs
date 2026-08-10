use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{Point, Rectangle, Size};
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::tests::client::LayerConfigureProps;
use crate::window::mapped::{ForeignToplevelRectHint, ForeignToplevelRectUnresolvedReason};

fn create_window(f: &mut Fixture, id: client::ClientId) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn map_dock_on_output(f: &mut Fixture, id: client::ClientId, output_n: u8) -> WlSurface {
    let wl_output = f.client(id).output(&format!("headless-{output_n}"));
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
}

#[test]
fn foreign_toplevel_set_rectangle_tracks_layer_surface_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let window_surface = create_window(&mut f, id);
    let layer_surface = map_dock_on_output(&mut f, id, 2);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);

    let output = f.niri_output(2);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    let rect = mapped.foreign_toplevel_rect().unwrap();
    assert_eq!(rect.output, output);
    assert_eq!(
        rect.rect.as_rect(),
        Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
    );
    let stored_rect = rect.rect;
    let gen1 = rect.generation;

    handle.set_minimized();
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(mapped.is_minimized());
    assert_eq!(mapped.foreign_toplevel_rect().unwrap().rect, stored_rect);

    handle.unset_minimized();
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.is_minimized());
    assert_eq!(mapped.foreign_toplevel_rect().unwrap().rect, stored_rect);

    // Non-layer source: legal request replaces prior with Unresolved (not Cleared).
    handle.set_rectangle(&window_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    match mapped.foreign_toplevel_rect_hint() {
        ForeignToplevelRectHint::Unresolved(u) => {
            assert_eq!(
                u.reason,
                ForeignToplevelRectUnresolvedReason::SourceNotMapped
            );
            assert!(u.generation > gen1);
        }
        other => panic!("expected Unresolved, got {other:?}"),
    }
    assert!(mapped.foreign_toplevel_rect().is_none());

    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert_eq!(mapped.foreign_toplevel_rect().unwrap().rect, stored_rect);

    // 0×N is last request, not protocol delete (only 0×0 deletes).
    handle.set_rectangle(&layer_surface, 10, 20, 0, 40);
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    let zero_w = mapped
        .foreign_toplevel_rect()
        .expect("0×N stays Resolved last request");
    assert!(zero_w.rect.is_empty());
    assert_eq!(zero_w.rect.size().w, 0);
    assert_eq!(zero_w.rect.size().h, 40);

    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(
        mapped.foreign_toplevel_rect_hint().is_cleared(),
        "matching source destroy must clear the last-hint"
    );
}

#[test]
fn set_rectangle_0x0_clears_and_zero_area_does_not() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id);
    let layer_surface = map_dock_on_output(&mut f, id, 1);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 5, 6, 40, 40);
    f.double_roundtrip(id);
    assert!(f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect()
        .is_some());

    // N×0 also last request.
    handle.set_rectangle(&layer_surface, 5, 6, 40, 0);
    f.double_roundtrip(id);
    {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        let r = mapped.foreign_toplevel_rect().expect("N×0 is last request");
        assert!(r.rect.is_empty());
        assert_eq!(r.rect.size().w, 40);
        assert_eq!(r.rect.size().h, 0);
    }

    handle.set_rectangle(&layer_surface, 5, 6, 40, 40);
    f.double_roundtrip(id);

    // 0×0 deletes.
    handle.set_rectangle(&layer_surface, 5, 6, 0, 0);
    f.double_roundtrip(id);
    assert!(f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect_hint()
        .is_cleared());
}

#[test]
fn unmapped_source_replaces_prior_with_unresolved_not_prior_value() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id);
    let layer_surface = map_dock_on_output(&mut f, id, 1);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    let gen_resolved = f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect()
        .unwrap()
        .generation;

    // Unmap source: matching cleanup → Cleared.
    f.unmap_layer(id, &layer_surface);
    assert!(f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect_hint()
        .is_cleared());

    // Re-publish while still unmapped: Unresolved last-hint (not “keep cleared as None-only”).
    handle.set_rectangle(&layer_surface, 11, 22, 33, 44);
    f.double_roundtrip(id);
    {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        match mapped.foreign_toplevel_rect_hint() {
            ForeignToplevelRectHint::Unresolved(u) => {
                assert_eq!(
                    u.reason,
                    ForeignToplevelRectUnresolvedReason::SourceNotMapped
                );
                assert!(u.generation > gen_resolved);
                assert_eq!(
                    u.surface_local_rect.as_rect(),
                    Rectangle::new(Point::from((11, 22)), Size::from((33, 44)))
                );
            }
            other => panic!("expected Unresolved after unmapped publish, got {other:?}"),
        }
    }

    // Remap + republish → Resolved replaces Unresolved.
    f.remap_layer(
        id,
        &layer_surface,
        LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Bottom),
            size: Some((200, 80)),
            ..Default::default()
        },
        (200, 80),
    );
    handle.set_rectangle(&layer_surface, 11, 22, 33, 44);
    f.double_roundtrip(id);
    {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        let r = mapped
            .foreign_toplevel_rect()
            .expect("remapped source resolves");
        assert_eq!(r.rect.size(), Size::from((33, 44)));
    }
}

#[test]
fn stale_source_cleanup_does_not_clear_newer_binding() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let _ = create_window(&mut f, id);

    let dock_a = map_dock_on_output(&mut f, id, 1);
    let dock_b = map_dock_on_output(&mut f, id, 2);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&dock_a, 1, 2, 30, 40);
    f.double_roundtrip(id);
    let gen_a = f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect()
        .unwrap()
        .generation;

    // Newer binding on dock_b replaces dock_a.
    handle.set_rectangle(&dock_b, 5, 6, 48, 48);
    f.double_roundtrip(id);
    let output2 = f.niri_output(2);
    {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        let r = mapped.foreign_toplevel_rect().unwrap();
        assert!(r.generation > gen_a);
        assert_eq!(r.output, output2);
    }

    // Destroying the *old* source must not clear the new binding.
    f.client(id).layer(&dock_a).layer_surface.destroy();
    f.double_roundtrip(id);
    {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        let r = mapped
            .foreign_toplevel_rect()
            .expect("new binding must survive old source destroy");
        assert_eq!(r.output, output2);
        assert_eq!(r.rect.size(), Size::from((48, 48)));
    }

    // Destroying the *current* source clears.
    f.client(id).layer(&dock_b).layer_surface.destroy();
    f.double_roundtrip(id);
    assert!(f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect_hint()
        .is_cleared());
}

#[test]
fn zero_area_last_request_degrades_lifecycle_consume_without_clearing_slot() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id);
    let layer_surface = map_dock_on_output(&mut f, id, 1);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, 10, 20, 0, 40);
    f.double_roundtrip(id);

    let window = f.niri().layout.windows().next().unwrap().1.window.clone();
    let result = f.niri_state().execute_lifecycle_command(
        crate::lifecycle_command::LifecycleCommand::minimize(
            window,
            crate::lifecycle_command::LifecycleAnchorInput::CachedForCurrentOutput,
            crate::lifecycle_command::LifecycleInvocationSource::Test,
        ),
    );
    assert!(result.changed());
    assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());
    // Slot remains the zero-area last request; only animation degraded.
    let r = f
        .niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .foreign_toplevel_rect()
        .expect("zero-area last request must remain");
    assert!(r.rect.is_empty());
}

// ---------------------------------------------------------------------------
// R11 feasibility: coordinated ext-list ↔ wlr management pairing on niri.
// Identifier is MappedId decimal; creation order pairs streams without a new
// compositor id protocol. Fail closed when ready counts desync.
// ---------------------------------------------------------------------------

fn mapped_ids_in_layout_order(f: &mut Fixture) -> Vec<String> {
    f.niri()
        .layout
        .windows()
        .map(|(_, mapped)| mapped.id().to_protocol_identifier())
        .collect()
}

fn assert_pairs_match_mapped_ids(f: &mut Fixture, id: client::ClientId) {
    f.double_roundtrip(id);
    let pairs = f
        .client(id)
        .pair_ext_wlr_by_creation_order()
        .expect("ext↔wlr FIFO pairing must succeed");
    let mapped = mapped_ids_in_layout_order(f);
    assert_eq!(
        pairs.len(),
        mapped.len(),
        "pair count must match mapped windows"
    );
    let mut pair_ids: Vec<_> = pairs.iter().map(|(i, _, _)| i.clone()).collect();
    let mut mapped_ids = mapped;
    pair_ids.sort();
    mapped_ids.sort();
    assert_eq!(
        pair_ids, mapped_ids,
        "paired identifiers must equal MappedId set (order may follow HashMap)"
    );
}

#[test]
fn r11_pair_shell_first_then_create_windows() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    // Shell already bound both managers via registry (no windows yet).
    f.double_roundtrip(id);
    assert!(f
        .client(id)
        .pair_ext_wlr_by_creation_order()
        .unwrap()
        .is_empty());

    let _a = create_window(&mut f, id);
    let _b = create_window(&mut f, id);
    assert_pairs_match_mapped_ids(&mut f, id);

    // Same app_id/title must still pair by stream order, not fuzzy title.
    for surface in f
        .client(id)
        .state
        .windows
        .iter()
        .map(|w| w.surface.clone())
        .collect::<Vec<_>>()
    {
        let window = f.client(id).window(&surface);
        window.set_app_id("org.same.App");
        window.set_title("Identical");
        window.ack_last_and_commit();
    }
    f.double_roundtrip(id);
    let pairs = f
        .client(id)
        .pair_ext_wlr_by_creation_order()
        .expect("identical title still pairs");
    assert_eq!(pairs.len(), 2);
    assert!(pairs.iter().all(|(_, app, title)| {
        app.as_deref() == Some("org.same.App") && title.as_deref() == Some("Identical")
    }));
    let ids: Vec<_> = pairs.into_iter().map(|(i, _, _)| i).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "identifiers must remain distinct");
}

#[test]
fn r11_pair_shell_restart_with_existing_windows() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // App client creates windows first.
    let app = f.add_client();
    let _a = create_window(&mut f, app);
    let _b = create_window(&mut f, app);
    let _c = create_window(&mut f, app);
    f.double_roundtrip(app);
    let expected = mapped_ids_in_layout_order(&mut f);
    assert_eq!(expected.len(), 3);

    // Shell-like client binds after windows exist (registry bind order: ext then wlr).
    let shell = f.add_client();
    f.double_roundtrip(shell);
    let pairs = f
        .client(shell)
        .pair_ext_wlr_by_creation_order()
        .expect("restart pairing");
    let mut got: Vec<_> = pairs.into_iter().map(|(i, _, _)| i).collect();
    let mut exp = expected;
    got.sort();
    exp.sort();
    assert_eq!(got, exp);
}

#[test]
fn r11_pair_close_and_remap_updates_streams() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = create_window(&mut f, id);
    assert_pairs_match_mapped_ids(&mut f, id);
    let first_id = mapped_ids_in_layout_order(&mut f)[0].clone();

    // Close the only window.
    f.client(id).window(&surface).xdg_toplevel.destroy();
    f.client(id).window(&surface).xdg_surface.destroy();
    f.client(id).window(&surface).surface.destroy();
    f.double_roundtrip(id);
    // Allow compositor refresh to emit closed.
    f.double_roundtrip(id);
    let pairs = f
        .client(id)
        .pair_ext_wlr_by_creation_order()
        .expect("empty after close");
    assert!(pairs.is_empty(), "closed handles must not stay ready");

    let _again = create_window(&mut f, id);
    assert_pairs_match_mapped_ids(&mut f, id);
    let second = mapped_ids_in_layout_order(&mut f)[0].clone();
    assert_ne!(first_id, second, "MappedId must not reuse after unmap");
}

#[test]
fn r11_pair_identifier_equals_mapped_id_decimal() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id);
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1.id();
    let pairs = f.client(id).pair_ext_wlr_by_creation_order().unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, mapped.to_protocol_identifier());
    assert_eq!(pairs[0].0, mapped.get().to_string());
}

#[test]
fn r11_pair_map_between_manager_binds_fails_closed_on_count_desync() {
    // Prove fail-closed: if only one stream has a handle, pairing errors.
    // Simulated by filtering closed meta rather than partial bind (auto-bind
    // always pairs both); inject a synthetic ready-count mismatch via a closed
    // flag on one side after a real pair exists.
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = create_window(&mut f, id);
    f.double_roundtrip(id);
    assert!(f.client(id).pair_ext_wlr_by_creation_order().is_ok());

    // Mark one ext ready handle closed without closing wlr → desync.
    {
        let client = f.client(id);
        let meta = client
            .state
            .ext_foreign_toplevels
            .iter_mut()
            .find(|m| m.done && !m.closed)
            .expect("ext meta");
        meta.closed = true;
    }
    let err = f
        .client(id)
        .pair_ext_wlr_by_creation_order()
        .expect_err("must fail closed on count desync");
    assert!(
        err.contains("desync"),
        "error should mention desync, got {err}"
    );
}
