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
use crate::protocols::tahoe_glass::get_transform_directive;

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
    f: &mut Fixture,
    output_n: u8,
    client_surface: &WlSurface,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let output = f.niri_output(output_n);
    let map = layer_map_for_output(&output);
    let found = map
        .layers()
        .find(|layer| layer.wl_surface().id().protocol_id() == client_surface.id().protocol_id())
        .unwrap_or_else(|| panic!("no mapped layer surface for client surface"))
        .wl_surface()
        .clone();
    found
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
    let server_surface_a = server_surface_for_client_layer(&mut f, 2, &surface_a);
    let server_surface_b = server_surface_for_client_layer(&mut f, 2, &surface_b);

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
    let server_surface = server_surface_for_client_layer(&mut f, 2, &surface);

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
