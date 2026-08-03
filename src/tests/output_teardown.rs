//! Output removal layer teardown lifecycle tests (T01).
//!
//! These exercise the real output-removal path: layers mapped on a removed
//! output must have their `mapped_layer_surfaces` entry (and with it the
//! pre-commit hook), foreign-toplevel rect hints, Tahoe glass transform
//! directive, `unmapped_layer_surfaces` entry and layer-map slot released
//! before the output leaves the layout. Clients that ignore `close` and keep
//! committing or destroy their surface later must not panic or leave entries
//! behind, and 100 add/remove cycles must return to the baseline holdings.

use niri_config::Config;
use smithay::desktop::layer_map_for_output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::wayland::compositor::with_states;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;
use wayland_server::Resource;

use super::client::{ClientId, LayerConfigureProps};
use super::*;
use crate::protocols::tahoe_glass::{
    get_committed_regions, get_transform_directive, test_fallback_redraw_all_count,
    test_pending_state, test_redraw_counter_lock, test_reset_redraw_counters,
    test_targeted_redraw_count,
};

fn map_layer_on_output(f: &mut Fixture, id: ClientId, output_n: u8, namespace: &str) -> WlSurface {
    f.double_roundtrip(id);
    let surface = f.map_layer(
        id,
        Some(output_n),
        Layer::Top,
        namespace,
        LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Top),
            size: Some((200, 100)),
            ..Default::default()
        },
        (200, 100),
    );
    f.double_roundtrip(id);
    surface
}

fn server_surface_for_client_layer(
    output: &smithay::output::Output,
    client_surface: &WlSurface,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let map = layer_map_for_output(output);
    let found = map
        .layers()
        .find(|layer| layer.wl_surface().id().protocol_id() == client_surface.id().protocol_id())
        .unwrap_or_else(|| panic!("no mapped layer surface for client surface"))
        .wl_surface()
        .clone();
    found
}

fn output_by_name(f: &mut Fixture, name: &str) -> smithay::output::Output {
    f.niri()
        .global_space
        .outputs()
        .find(|output| output.name() == name)
        .unwrap_or_else(|| panic!("no output named {name}"))
        .clone()
}

fn put_pending_glass_state_in_flight(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(surface, &qh, ());
    // Uncommitted transform request + region request; neither rides a commit.
    glass.set_transform(10.0.into(), 5.0.into(), 0.8.into(), 0.8.into());
    glass.set_region(
        1,
        8,
        4,
        128,
        32,
        8,
        8,
        8,
        8,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
}

#[test]
fn orphaned_commits_do_not_reapply_tahoe_glass_state_after_output_removal() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_layer_on_output(&mut f, id, 2, "tahoe-layer");
    let output = f.niri_output(2);
    let server_surface = server_surface_for_client_layer(&output, &surface);

    // Pending (uncommitted) Tahoe glass state on the layer surface.
    put_pending_glass_state_in_flight(&mut f, id, &surface);
    assert_eq!(
        with_states(&server_surface, |states| test_pending_state(states)),
        (1, true, true),
        "pending glass state must exist before the output is removed"
    );

    // Remove the output while the client keeps the surface and its controller.
    let output = f.niri_output(2);
    f.niri().remove_output(&output);

    // The teardown must wipe pending + committed + directive in one go: the
    // surface no longer belongs to any output.
    assert_eq!(
        with_states(&server_surface, |states| test_pending_state(states)),
        (0, false, false),
        "pending glass state must be cleared on output removal"
    );
    assert!(
        with_states(&server_surface, |states| get_committed_regions(states)
            .is_empty()),
        "committed glass regions must be cleared on output removal"
    );
    assert!(
        with_states(&server_surface, |states| get_transform_directive(states)).is_none(),
        "transform directive must stay cleared on output removal"
    );

    // The client ignores close and keeps committing (null commit). Those
    // orphaned commits must not resurrect the glass state.
    let _guard = test_redraw_counter_lock();
    test_reset_redraw_counters();
    f.client(id).layer(&surface).attach_null();
    f.client(id).layer(&surface).commit();
    f.double_roundtrip(id);

    assert_eq!(
        with_states(&server_surface, |states| test_pending_state(states)),
        (0, false, false),
        "orphaned null commit must not re-create pending glass state"
    );
    assert!(
        with_states(&server_surface, |states| get_committed_regions(states)
            .is_empty()),
        "orphaned null commit must not re-commit pending glass regions"
    );
    assert!(
        with_states(&server_surface, |states| get_transform_directive(states)).is_none(),
        "orphaned null commit must not re-publish a transform directive"
    );
    assert_eq!(
        test_targeted_redraw_count() + test_fallback_redraw_all_count(),
        0,
        "orphaned null commit must not queue a Tahoe glass redraw"
    );

    f.client(id).layer(&surface).layer_surface.destroy();
    f.double_roundtrip(id);
}

#[test]
fn remapped_surface_does_not_inherit_removed_output_glass_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    // Output 3 must exist before output 2 is removed: the fixture indexes
    // outputs by position, and a removed output leaves the global space.
    f.add_output(3, (1280, 720));
    let id = f.add_client();

    let surface = map_layer_on_output(&mut f, id, 2, "tahoe-layer");

    // Pending (uncommitted) Tahoe glass state from the soon-to-be-removed
    // output; the client keeps the surface and its controller alive.
    put_pending_glass_state_in_flight(&mut f, id, &surface);

    let output = f.niri_output(2);
    f.niri().remove_output(&output);

    // The client re-binds the same wl_surface to a new layer surface on the
    // still-present output 3. The wl_surface may only have one viewport
    // object for its lifetime, so the re-bound layer surface reuses it.
    f.double_roundtrip(id);

    let client_output_3 = f.client(id).output("headless-3");
    f.client(id).layer(&surface).layer_surface.destroy();
    let viewport = f.client(id).destroy_layer(&surface);
    f.roundtrip(id);

    let layer = f.client(id).create_layer_on_surface(
        &surface,
        Some(&client_output_3),
        Layer::Top,
        "tahoe-layer",
        &viewport,
    );
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Top),
        size: Some((200, 100)),
        ..Default::default()
    });
    // The surface still carries the buffer it had on the removed output; a
    // null commit clears it so the new mapping starts from an initial
    // configure, exactly like a real client re-binding the surface.
    layer.attach_null();
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(200, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let output_3 = output_by_name(&mut f, "headless-3");
    let server_surface = server_surface_for_client_layer(&output_3, &surface);
    assert!(
        with_states(&server_surface, |states| get_committed_regions(states)
            .is_empty()),
        "re-mapped surface must not inherit committed glass regions from the removed output"
    );
    assert!(
        with_states(&server_surface, |states| get_transform_directive(states)).is_none(),
        "re-mapped surface must not inherit a transform directive from the removed output"
    );
    assert_eq!(
        with_states(&server_surface, |states| test_pending_state(states)),
        (0, false, false),
        "re-mapped surface must not inherit pending glass state from the removed output"
    );
    assert!(
        !f.niri().mapped_layer_surfaces.is_empty(),
        "the re-mapped layer must be tracked as mapped on the new output"
    );
}

#[test]
fn output_removal_tears_down_layers_foreign_rects_and_tahoe_transform() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-close {
                    duration-ms 33
                    curve "linear"
                }
            }
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();

    // Two mapped layers on output 2.
    let surface_a = map_layer_on_output(&mut f, id, 2, "animated-layer");
    let surface_b = map_layer_on_output(&mut f, id, 2, "plain-layer");

    let output = f.niri_output(2);
    let server_surface_a = server_surface_for_client_layer(&output, &surface_a);
    let server_surface_b = server_surface_for_client_layer(&output, &surface_b);

    // Publish a Tahoe glass transform directive on layer B through the real
    // protocol path; it must be gone after the output removal.
    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&surface_b, &qh, ());
    glass.set_transform(10.0.into(), 5.0.into(), 0.8.into(), 0.8.into());
    f.client(id).layer(&surface_b).commit();
    f.double_roundtrip(id);
    assert!(
        with_states(&server_surface_b, |states| get_transform_directive(states)).is_some(),
        "directive must be published before the output is removed"
    );

    // A window on output 1 keeps a foreign-toplevel rect hint whose source is
    // layer B; it must be cleared when the output (and its layers) go away.
    {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);
        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.set_size(100, 100);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }
    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&surface_b, 10, 20, 30, 40);
    f.double_roundtrip(id);
    assert!(
        f.niri()
            .layout
            .windows()
            .next()
            .unwrap()
            .1
            .foreign_toplevel_rect()
            .is_some(),
        "foreign-toplevel rect hint must resolve before the output is removed"
    );

    // Unmap layer A so a close snapshot animation is running on output 2.
    // Removing the output must cancel it, not keep the old output alive.
    f.unmap_layer(id, &surface_a);
    assert_eq!(
        f.niri().closing_layers.len(),
        1,
        "unmap of layer A must start a close animation"
    );

    // Remove output 2 while its layers are still alive and unresponsive.
    f.niri().remove_output(&output);

    {
        let map = layer_map_for_output(&output);
        let names: Vec<_> = map
            .layers()
            .map(|layer| layer.namespace().to_owned())
            .collect();
        assert!(
            names.is_empty(),
            "layer map must be empty on output removal, got {names:?}"
        );
    }

    let remaining: Vec<_> = f
        .niri()
        .mapped_layer_surfaces
        .keys()
        .map(|layer| layer.namespace().to_owned())
        .collect();
    assert!(
        remaining.is_empty(),
        "mapped_layer_surfaces must be empty after output removal, got {remaining:?}"
    );
    assert!(
        !f.niri().unmapped_layer_surfaces.contains(&server_surface_a),
        "unmapped_layer_surfaces must not retain the torn-down surface"
    );
    assert!(
        with_states(&server_surface_b, |states| get_transform_directive(states)).is_none(),
        "Tahoe transform directive must be cleared on output removal"
    );
    assert!(
        f.niri()
            .layout
            .windows()
            .next()
            .unwrap()
            .1
            .foreign_toplevel_rect_hint()
            .is_cleared(),
        "foreign-toplevel rect hint must be cleared on output removal"
    );
    assert!(
        f.niri().closing_layers.is_empty(),
        "close animations on a removed output must be cancelled"
    );

    // The client ignores close: it keeps using its surfaces through legal
    // protocol sequences (a null commit on the still-mapped surface, and a
    // configure + null commit on the previously unmapped one). Neither the
    // commits nor the later destroys may panic or re-create any state.
    f.client(id).layer(&surface_b).attach_null();
    f.client(id).layer(&surface_b).commit();
    f.double_roundtrip(id);
    assert!(
        f.niri().mapped_layer_surfaces.is_empty(),
        "null commit after output removal must not leave mapped state"
    );

    f.client(id)
        .layer(&surface_a)
        .set_configure_props(LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Top),
            size: Some((200, 100)),
            ..Default::default()
        });
    f.client(id).layer(&surface_a).attach_null();
    f.client(id).layer(&surface_a).commit();
    f.double_roundtrip(id);
    assert!(
        f.niri().mapped_layer_surfaces.is_empty(),
        "configure + null commit after output removal must not map anything"
    );

    f.client(id).layer(&surface_a).layer_surface.destroy();
    f.double_roundtrip(id);
    f.client(id).layer(&surface_b).layer_surface.destroy();
    f.double_roundtrip(id);
    assert!(
        f.niri().mapped_layer_surfaces.is_empty(),
        "destroy after output removal must not leave mapped state"
    );
    assert!(
        f.niri().unmapped_layer_surfaces.is_empty(),
        "destroy after output removal must not leave unmapped state"
    );
}

#[test]
fn remove_output_with_unmapped_layer_cleans_up_without_panic() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_layer_on_output(&mut f, id, 2, "unmapped-layer");
    let output = f.niri_output(2);
    let server_surface = server_surface_for_client_layer(&output, &surface);

    // Null commit: the layer unmaps while its output is still present.
    f.unmap_layer(id, &surface);
    assert!(
        f.niri().unmapped_layer_surfaces.contains(&server_surface),
        "unmapped surface must be tracked before output removal"
    );

    let output = f.niri_output(2);
    f.niri().remove_output(&output);

    assert!(
        f.niri().mapped_layer_surfaces.is_empty(),
        "no mapped entry may survive output removal"
    );
    assert!(
        !f.niri().unmapped_layer_surfaces.contains(&server_surface),
        "unmapped entry must be released on output removal"
    );
    assert_eq!(
        layer_map_for_output(&output).layers().count(),
        0,
        "layer map must be empty on output removal"
    );
}

#[test]
fn remove_last_output_with_mapped_layer_does_not_panic() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_layer_on_output(&mut f, id, 1, "last-output-layer");
    let output = f.niri_output(1);

    // Removing the last output must not panic and must still tear down its
    // layer state.
    f.niri().remove_output(&output);

    assert!(f.niri().mapped_layer_surfaces.is_empty());
    assert!(f.niri().unmapped_layer_surfaces.is_empty());

    // The client ignores close and destroys the surface later.
    f.client(id).layer(&surface).layer_surface.destroy();
    f.double_roundtrip(id);
    assert!(f.niri().mapped_layer_surfaces.is_empty());
}

#[test]
fn repeated_output_add_remove_returns_mapped_layer_holdings_to_baseline() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    for cycle in 0..100 {
        f.add_output(2, (1280, 720));
        let id = f.add_client();
        let surface = map_layer_on_output(&mut f, id, 2, "hotplug-layer");

        let output = f.niri_output(2);
        f.niri().remove_output(&output);

        // Every holding returns to its pre-cycle baseline.
        assert!(
            f.niri().mapped_layer_surfaces.is_empty(),
            "cycle {cycle}: mapped_layer_surfaces must return to baseline"
        );
        assert!(
            f.niri().unmapped_layer_surfaces.is_empty(),
            "cycle {cycle}: unmapped_layer_surfaces must return to baseline"
        );
        assert_eq!(
            layer_map_for_output(&output).layers().count(),
            0,
            "cycle {cycle}: layer map must be empty"
        );

        // The client ignores close: null commit and destroy must not panic.
        f.client(id).layer(&surface).attach_null();
        f.client(id).layer(&surface).commit();
        f.double_roundtrip(id);
        f.client(id).layer(&surface).layer_surface.destroy();
        f.double_roundtrip(id);

        assert!(
            f.niri().mapped_layer_surfaces.is_empty(),
            "cycle {cycle}: destroy after removal must not leave mapped state"
        );
    }
}
