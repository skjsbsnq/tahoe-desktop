//! Integration tests for Tahoe glass controller lifecycle (Task 05 / Task 19).
//!
//! These exercise the real protocol path: client Destroy must clear
//! committed regions on a still-alive wl_surface, and recreate must not
//! inherit prior glass state. If both Destroy and destroyed stayed empty,
//! these would fail; they share clear_surface_state_if_owner.
//!
//! Task 19 extends coverage to abnormal client disconnect and output
//! redraw/damage queueing when committed glass is cleared.

use smithay::desktop::layer_map_for_output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::with_states;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_surface_v1::TahoeGlassSurfaceV1;
use crate::protocols::tahoe_glass::{
    get_committed_regions, test_damage_old_region_count, test_fallback_redraw_all_count,
    test_last_damaged_old_rects, test_reset_redraw_counters, test_targeted_redraw_count,
};
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

fn set_and_commit_region(
    f: &mut Fixture,
    id: client::ClientId,
    client_surface: &WlSurface,
    glass: &TahoeGlassSurfaceV1,
    region_id: u32,
) {
    glass.set_region(
        region_id,
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
    f.client(id).layer(client_surface).commit();
    f.double_roundtrip(id);
}

/// Abnormal client disconnect: controller is destroyed with the client.
/// When the server wl_surface remains alive, committed glass must clear via
/// `destroyed()` without panicking. When the surface dies with the client,
/// no leftover client session may remain.
#[test]
fn abnormal_client_disconnect_clears_committed_glass() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);
    test_reset_redraw_counters();

    // Drop the client without protocol Destroy — smithay runs resource
    // destroyed handlers, which must clear owner glass like explicit Destroy.
    f.disconnect_client(id);

    assert!(
        f.state.clients.iter().all(|c| c.id != id),
        "disconnected client must be removed from the fixture"
    );

    if server_surface.alive() {
        assert_eq!(
            committed_count(&server_surface),
            0,
            "abnormal disconnect must clear committed glass while wl_surface lives"
        );
        // Clear of non-empty committed must have requested old-area damage.
        assert!(
            test_damage_old_region_count() >= 1,
            "disconnect clear must damage prior committed regions"
        );
    } else {
        // Surface died with the client; glass state is gone with the surface.
        // Still require that disconnect completed without leaving the client.
        assert!(
            f.state.clients.iter().all(|c| c.id != id),
            "client must be gone after disconnect"
        );
    }
}

/// Explicit controller Destroy with a mapped layer must use the targeted
/// redraw path (output_for_root hit), not queue_redraw_all.
#[test]
fn destroy_controller_queues_redraw_only_on_root_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    test_reset_redraw_counters();
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);

    assert_eq!(committed_count(&server_surface), 0);
    assert!(
        test_targeted_redraw_count() >= 1,
        "clearing committed glass must queue redraw via output_for_root"
    );
    assert_eq!(
        test_fallback_redraw_all_count(),
        0,
        "mapped root must not fall back to queue_redraw_all"
    );
    // set_and_commit_region uses rect (8,4)-(128,32).
    assert!(
        test_damage_old_region_count() >= 1,
        "clear must damage the old committed region area"
    );
    let rects = test_last_damaged_old_rects();
    assert!(
        rects.iter().any(|r| *r == (8, 4, 128, 32)),
        "old committed rect must be in the damage record: {rects:?}"
    );
}

/// When the root cannot be located to an output, the production redraw owner
/// falls back to queue_redraw_all.
#[test]
fn clear_with_unlocatable_root_queues_all_outputs() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    // Unmap the layer so output_for_root can no longer resolve the surface,
    // while the wl_surface (and glass controller) remain.
    {
        let layer = f.client(id).layer(&client_surface);
        layer.layer_surface.destroy();
        f.client(id).connection.flush().unwrap();
        f.roundtrip(id);
    }
    for _ in 0..4 {
        f.dispatch();
    }

    use crate::protocols::tahoe_glass::TahoeGlassHandler;
    // Precondition: after unmap, root must not resolve to an output.
    assert!(
        f.niri().output_for_root(&server_surface).is_none(),
        "precondition: root must be unlocatable after layer surface destroy"
    );

    // Drive the same State method the protocol Destroy path uses when clear
    // requests a redraw for a still-alive but unmapped surface.
    test_reset_redraw_counters();
    f.niri_state()
        .queue_redraw_for_tahoe_glass_surface(&server_surface);
    assert!(
        test_fallback_redraw_all_count() >= 1,
        "unlocatable root must fall back to queue_redraw_all"
    );
    assert_eq!(
        test_targeted_redraw_count(),
        0,
        "unlocatable root must not use the targeted path"
    );

    // Controller Destroy must still clear glass without panicking.
    test_reset_redraw_counters();
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    if server_surface.alive() {
        assert_eq!(committed_count(&server_surface), 0);
    }
}

/// Destroy then a second claim/destroy cycle is idempotent: glass stays empty
/// and later surface commits must not resurrect regions.
#[test]
fn destroy_is_idempotent_when_double_invoked() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);

    // Second clear path: recreate then immediately destroy without set_region.
    let glass2 = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);
    glass2.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);

    f.client(id).layer(&client_surface).commit();
    f.double_roundtrip(id);
    assert_eq!(
        committed_count(&server_surface),
        0,
        "idempotent clear must not resurrect glass on later surface commits"
    );
}
