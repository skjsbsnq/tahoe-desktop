//! Integration tests for Tahoe glass controller lifecycle (Task 05).
//!
//! These exercise the real protocol path: client Destroy must clear
//! committed regions on a still-alive wl_surface, and recreate must not
//! inherit prior glass state. If both Destroy and destroyed stayed empty, these would fail; they share clear_surface_state_if_owner.

use smithay::desktop::layer_map_for_output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::wayland::compositor::with_states;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::protocols::tahoe_glass::get_committed_regions;
use crate::tests::client::LayerConfigureProps;

fn create_mapped_layer(f: &mut Fixture, id: client::ClientId) -> WlSurface {
    let layer = f
        .client(id)
        .create_layer(None, Layer::Top, "tahoe-glass-test");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Top),
        size: Some((200, 100)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(200, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn server_surface_for_client_layer(
    f: &mut Fixture,
    client_surface: &WlSurface,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let output = f.niri_output(1);
    let map = layer_map_for_output(&output);
    for layer in map.layers() {
        let server = layer.wl_surface().clone();
        // Match by checking client id string form is unreliable; there is only one layer.
        let _ = client_surface;
        return server;
    }
    panic!("no mapped layer surface on output");
}

fn committed_count(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> usize {
    with_states(surface, |states| get_committed_regions(states).len())
}

#[test]
fn destroy_controller_clears_committed_regions_while_surface_lives() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    // region id=1 fully inside 200x100 layer
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

    // Commit the surface so pending glass becomes committed.
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(
        committed_count(&server_surface),
        1,
        "set_region + surface commit must publish committed glass"
    );

    // Destroy controller while wl_surface stays alive.
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);

    assert_eq!(
        committed_count(&server_surface),
        0,
        "Destroy must clear committed glass immediately without waiting for another surface commit"
    );
    // Surface still usable: a subsequent commit must not revive old glass.
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);
}

#[test]
fn recreate_controller_does_not_inherit_previous_committed_regions() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();

    let glass_a = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    glass_a.set_region(
        1,
        0,
        0,
        64,
        32,
        0,
        0,
        0,
        0,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    // Destroy A, then create B without setting regions — B must start empty.
    glass_a.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);

    let glass_b = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    // No set_region on B; surface commit must not resurrect A's glass.
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(
        committed_count(&server_surface),
        0,
        "recreate must not inherit previous controller committed regions"
    );

    // B can set its own region successfully.
    glass_b.set_region(
        9,
        4,
        4,
        40,
        20,
        0,
        0,
        0,
        0,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(committed_count(&server_surface), 1);

    glass_b.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);
}

#[test]
fn recreate_while_old_controller_still_alive_takes_ownership() {
    // Protocol allows multiple controllers; implementation is last-claim-wins.
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();

    let glass_a = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    glass_a.set_region(
        1,
        0,
        0,
        50,
        50,
        0,
        0,
        0,
        0,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    // Create B without destroying A — claim clears A's committed glass.
    let glass_b = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(
        committed_count(&server_surface),
        0,
        "new controller claim must clear previous committed glass"
    );

    // Stale writes from A must not reappear.
    glass_a.set_region(
        2,
        0,
        0,
        40,
        40,
        0,
        0,
        0,
        0,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(
        committed_count(&server_surface),
        0,
        "stale controller set_region must not publish glass"
    );

    // Late destroy of A must not clear B after B sets glass.
    glass_b.set_region(
        3,
        1,
        1,
        30,
        30,
        0,
        0,
        0,
        0,
        String::from("panel"),
        7,
        0.0.into(),
        1.0.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(committed_count(&server_surface), 1);

    glass_a.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(
        committed_count(&server_surface),
        1,
        "stale controller destroy must not clear new owner"
    );

    glass_b.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);
}
