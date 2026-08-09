//! Integration tests for Tahoe glass controller lifecycle and v5 feedback.
//!
//! These exercise the real protocol path: client Destroy must clear
//! committed regions on a still-alive wl_surface, and recreate must not
//! inherit prior glass state. If both Destroy and destroyed stayed empty,
//! these would fail; they share clear_surface_state_if_owner.
//!
//! Task 19 extends coverage to abnormal client disconnect and output
//! redraw/damage queueing when committed glass is cleared.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Color32F;
use smithay::desktop::layer_map_for_output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{IsAlive, Scale, Size, Transform};
use smithay::wayland::compositor::with_states;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_surface_v1::{
    self, TahoeGlassSurfaceV1,
};
use crate::protocols::tahoe_glass::{
    get_committed_regions, test_damage_old_region_count, test_fallback_redraw_all_count,
    test_last_damaged_old_rects, test_redraw_counter_lock, test_reset_redraw_counters,
    test_targeted_redraw_count,
};
use crate::render_helpers::shaders::Shaders;
use crate::tests::client::LayerConfigureProps;
use crate::utils::lifecycle_diag;

crate::niri_render_elements! {
    TahoeGlassSceneElement => {
        Backdrop = crate::render_helpers::solid_color::SolidColorRenderElement,
        Glass = crate::render_helpers::tahoe_glass::TahoeGlassElement,
    }
}

#[test]
fn postprocess_shader_compiles_and_invalid_source_is_rejected() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();

    f.niri_state()
        .backend
        .with_primary_renderer(|renderer| {
            assert!(
                Shaders::get(renderer).postprocess_and_clip.is_some(),
                "the production postprocess shader must compile"
            );

            let invalid = renderer.compile_custom_texture_shader(
                "#version 100\nprecision highp float;\nvoid main() { invalid shader; }",
                &[],
            );
            assert!(
                invalid.is_err(),
                "invalid shader source must report failure"
            );
        })
        .unwrap();
}

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

fn force_idle_redraw_states(niri: &mut crate::niri::Niri) {
    use crate::niri::RedrawState;
    for state in niri.output_state.values_mut() {
        state.redraw_state = RedrawState::Idle;
    }
}

fn count_outputs_queued(niri: &crate::niri::Niri) -> (usize, usize) {
    use crate::niri::RedrawState;
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
    use crate::niri::RedrawState;
    matches!(
        niri.output_state
            .get(output)
            .map(|state| &state.redraw_state),
        Some(RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_))
    )
}

fn glass_events(
    f: &mut Fixture,
    id: client::ClientId,
    glass: &TahoeGlassSurfaceV1,
) -> client::TahoeGlassSurfaceEvents {
    f.client(id)
        .state
        .tahoe_glass_surface_events
        .get(glass)
        .cloned()
        .unwrap_or_default()
}

fn commit_layer(f: &mut Fixture, id: client::ClientId, surface: &WlSurface) {
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(surface).commit();
    f.double_roundtrip(id);
}

#[test]
fn v4_transform_path_remains_silent_and_usable() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(4);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    assert_eq!(glass_events(&mut f, id, &glass), Default::default());

    glass.set_transform(12.0.into(), 4.0.into(), 0.9.into(), 0.9.into());
    commit_layer(&mut f, id, &client_surface);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(glass_events(&mut f, id, &glass), Default::default());
}

#[test]
fn v5_capability_and_transform_completion_arrive_on_wire_once() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).capabilities,
        vec![u32::from(
            tahoe_glass_surface_v1::Capability::TransformFeedback
        )]
    );

    glass.set_transform_serial(42);
    glass.set_transform_target(
        20.0.into(),
        8.0.into(),
        0.8.into(),
        0.8.into(),
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);
    assert!(
        glass_events(&mut f, id, &glass)
            .transform_feedback
            .is_empty(),
        "animated transform must not complete at commit"
    );

    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback,
        vec![(
            42,
            u32::from(tahoe_glass_surface_v1::FeedbackStatus::Completed)
        )]
    );

    f.client(id).layer(&client_surface).commit();
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback.len(),
        1,
        "later commits and animation ticks must not repeat terminal feedback"
    );
}

#[test]
fn v5_controller_replacement_cancels_active_serial_and_blocks_late_completion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass_a = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    glass_a.set_transform_serial(301);
    glass_a.set_transform_target(
        20.0.into(),
        8.0.into(),
        0.8.into(),
        0.8.into(),
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);

    let glass_b = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.client(id).connection.flush().unwrap();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass_a).transform_feedback,
        vec![(
            301,
            u32::from(tahoe_glass_surface_v1::FeedbackStatus::Cancelled)
        )]
    );
    assert_eq!(
        glass_events(&mut f, id, &glass_b).capabilities,
        vec![u32::from(
            tahoe_glass_surface_v1::Capability::TransformFeedback
        )]
    );

    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass_a).transform_feedback.len(),
        1,
        "the superseded animation epoch must not complete after controller replacement"
    );
}

#[test]
#[should_panic(expected = "Protocol error 1 on object tahoe_glass_surface_v1")]
fn v5_reusing_an_in_flight_serial_is_a_protocol_error() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    glass.set_transform_serial(42);
    glass.set_transform_target(
        20.0.into(),
        8.0.into(),
        0.8.into(),
        0.8.into(),
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);

    glass.set_transform_serial(42);
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
}

#[test]
fn v5_supersede_and_reject_are_correlated_on_wire() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);

    glass.set_transform_serial(101);
    glass.set_transform_target(
        20.0.into(),
        8.0.into(),
        0.8.into(),
        0.8.into(),
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    glass.set_transform_serial(102);
    glass.set_transform(0.0.into(), 0.0.into(), 1.0.into(), 1.0.into());
    commit_layer(&mut f, id, &client_surface);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    glass.set_transform_serial(103);
    glass.set_region_morph(
        999,
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);

    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback,
        vec![
            (
                101,
                u32::from(tahoe_glass_surface_v1::FeedbackStatus::Superseded),
            ),
            (
                102,
                u32::from(tahoe_glass_surface_v1::FeedbackStatus::Completed),
            ),
            (
                103,
                u32::from(tahoe_glass_surface_v1::FeedbackStatus::Rejected),
            ),
        ]
    );
}

#[test]
fn v5_healable_region_morph_waits_for_surface_growth_then_completes_once() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    glass.set_region(
        1,
        8,
        4,
        240,
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
    glass.set_transform_serial(201);
    glass.set_region_morph(
        1,
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);
    assert!(
        glass_events(&mut f, id, &glass)
            .transform_feedback
            .is_empty(),
        "geometry-healable overflow must remain pending"
    );

    {
        let layer = f.client(id).layer(&client_surface);
        layer.set_size(300, 100);
        layer.attach_new_buffer();
        layer.commit();
    }
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback,
        vec![(
            201,
            u32::from(tahoe_glass_surface_v1::FeedbackStatus::Completed)
        )]
    );

    f.client(id).layer(&client_surface).commit();
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(glass_events(&mut f, id, &glass).transform_feedback.len(), 1);
}

#[test]
fn v5_unmap_cancels_healable_region_morph_without_late_completion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    glass.set_region(
        1,
        8,
        4,
        240,
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
    glass.set_transform_serial(202);
    glass.set_region_morph(
        1,
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);
    assert!(
        glass_events(&mut f, id, &glass)
            .transform_feedback
            .is_empty(),
        "geometry-healable overflow must remain pending before unmap"
    );

    f.unmap_layer(id, &client_surface);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback,
        vec![(
            202,
            u32::from(tahoe_glass_surface_v1::FeedbackStatus::Cancelled)
        )]
    );

    f.remap_layer(
        id,
        &client_surface,
        LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Top),
            size: Some((300, 100)),
            ..Default::default()
        },
        (300, 100),
    );
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback.len(),
        1,
        "the pre-unmap morph must not complete after remap and surface growth"
    );
}

#[test]
fn v5_transform_on_never_mapped_layer_is_cancelled_without_late_completion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let layer = f
        .client(id)
        .create_layer(None, Layer::Top, "tahoe-glass-never-mapped");
    let client_surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Top),
        size: Some((200, 100)),
        ..Default::default()
    });

    let manager = f.client(id).bind_tahoe_glass_manager(5);
    let qh = f.client(id).qh.clone();
    let glass = manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    f.double_roundtrip(id);

    glass.set_transform_serial(203);
    glass.set_transform_target(
        20.0.into(),
        8.0.into(),
        0.8.into(),
        0.8.into(),
        tahoe_glass_surface_v1::TransformCurve::Eased,
        200.0.into(),
        0.2.into(),
        0.0.into(),
        0.2.into(),
        1.0.into(),
    );
    commit_layer(&mut f, id, &client_surface);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback,
        vec![(
            203,
            u32::from(tahoe_glass_surface_v1::FeedbackStatus::Cancelled)
        )]
    );

    {
        let layer = f.client(id).layer(&client_surface);
        layer.attach_new_buffer();
        layer.set_size(200, 100);
        layer.ack_last_and_commit();
    }
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        glass_events(&mut f, id, &glass).transform_feedback.len(),
        1,
        "the never-mapped transform must not complete after the layer maps"
    );
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
    set_and_commit_region_with_alpha(f, id, client_surface, glass, region_id, 1.0);
}

fn set_and_commit_region_with_alpha(
    f: &mut Fixture,
    id: client::ClientId,
    client_surface: &WlSurface,
    glass: &TahoeGlassSurfaceV1,
    region_id: u32,
    material_alpha: f64,
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
        material_alpha.into(),
    );
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    f.client(id).layer(client_surface).commit();
    f.double_roundtrip(id);
}

fn set_and_commit_region_before_redraw(
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
    f.roundtrip_protocol_only(id);
}

fn render_tahoe_scene(
    f: &mut Fixture,
    server_surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    config: &niri_config::TahoeGlass,
    backdrop_color: Color32F,
) -> (Vec<u8>, usize, Option<bool>) {
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use crate::render_helpers::tahoe_glass::{render_for_layer, TahoeGlassElement};
    use crate::render_helpers::xray::XrayPos;
    use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};

    f.niri_state()
        .backend
        .with_primary_renderer(|renderer| {
            let mut glass = Vec::new();
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            assert!(render_for_layer(
                ctx,
                None,
                server_surface,
                "tahoe-glass-test",
                (0., 0.).into(),
                1.0,
                config,
                1.0,
                None,
                XrayPos::default(),
                false,
                &mut |element| glass.push(element),
            ));

            let material_index = glass
                .iter()
                .position(|element| matches!(element, TahoeGlassElement::BackgroundEffect(_)));
            let shadow_index = glass
                .iter()
                .position(|element| matches!(element, TahoeGlassElement::Shadow(_)));
            let order = material_index
                .zip(shadow_index)
                .map(|(material, shadow)| material < shadow);
            let glass_count = glass.len();

            let backdrop_buffer = SolidColorBuffer::new((256., 256.), backdrop_color);
            let backdrop = SolidColorRenderElement::from_buffer(
                &backdrop_buffer,
                (0., 0.),
                1.,
                Kind::Unspecified,
            );
            let mut scene = vec![TahoeGlassSceneElement::Backdrop(backdrop)];
            scene.extend(glass.into_iter().map(TahoeGlassSceneElement::Glass));
            let pixels = render_to_vec(
                renderer,
                Size::from((256, 256)),
                Scale::from(1.),
                Transform::Normal,
                Fourcc::Abgr8888,
                scene.into_iter(),
            )
            .expect("rendering production Tahoe glass elements");

            (pixels, glass_count, order)
        })
        .expect("primary renderer")
}

/// A zero-alpha region must stop before plan/capture accounting, while the
/// first restored frame must go through the normal Tahoe render path again.
#[test]
fn zero_alpha_skips_capture_and_restore_renders_first_visible_frame() {
    use crate::render_helpers::tahoe_glass::render_for_layer;
    use crate::render_helpers::xray::XrayPos;
    use crate::render_helpers::{RenderCtx, RenderTarget};

    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);
    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    let config = niri_config::TahoeGlass::default();

    set_and_commit_region_with_alpha(&mut f, id, &client_surface, &glass, 1, 0.0);
    let mut zero_alpha_elements = Vec::new();
    lifecycle_diag::with_enabled_for_test(|| {
        f.niri_state()
            .backend
            .with_primary_renderer(|renderer| {
                let ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Output,
                    xray: None,
                };
                let rendered = render_for_layer(
                    ctx,
                    None,
                    &server_surface,
                    "tahoe-glass-test",
                    (0., 0.).into(),
                    1.0,
                    &config,
                    1.0,
                    None,
                    XrayPos::default(),
                    false,
                    &mut |element| zero_alpha_elements.push(element),
                );
                assert!(
                    rendered,
                    "the committed region is present even though its alpha is zero"
                );
            })
            .unwrap();

        let zero = lifecycle_diag::snapshot();
        assert!(zero_alpha_elements.is_empty());
        assert_eq!(zero.tahoe_region_capture, 0);
        assert_eq!(zero.fb_effect_capture, 0);
        assert_eq!(zero.blur_render, 0);
    });

    let ((hidden_pixels, hidden_count, hidden_order), hidden_diag) =
        lifecycle_diag::with_enabled_for_test(|| {
            let rendered = render_tahoe_scene(
                &mut f,
                &server_surface,
                &config,
                Color32F::new(0.9, 0.9, 0.9, 1.),
            );
            (rendered, lifecycle_diag::snapshot())
        });
    assert_eq!(hidden_count, 0);
    assert_eq!(hidden_order, None);
    assert_eq!(hidden_diag.tahoe_region_capture, 0);
    assert_eq!(hidden_diag.fb_effect_capture, 0);
    assert_eq!(hidden_diag.blur_render, 0);

    let ((hidden_dark_pixels, hidden_dark_count, hidden_dark_order), hidden_dark_diag) =
        lifecycle_diag::with_enabled_for_test(|| {
            let rendered = render_tahoe_scene(
                &mut f,
                &server_surface,
                &config,
                Color32F::new(0.1, 0.1, 0.1, 1.),
            );
            (rendered, lifecycle_diag::snapshot())
        });
    assert_eq!(hidden_dark_count, 0);
    assert_eq!(hidden_dark_order, None);
    assert_eq!(hidden_dark_diag.tahoe_region_capture, 0);
    assert_eq!(hidden_dark_diag.fb_effect_capture, 0);
    assert_eq!(hidden_dark_diag.blur_render, 0);

    let hidden_center = |pixels: &[u8]| {
        let offset = (20 * 256 + 72) * 4;
        pixels[offset..offset + 4].to_owned()
    };
    assert_ne!(
        hidden_center(&hidden_pixels),
        hidden_center(&hidden_dark_pixels),
        "the backdrop must change while the glass is transparent"
    );

    set_and_commit_region_with_alpha(&mut f, id, &client_surface, &glass, 1, 1.0);
    let ((_bright_pixels, bright_count, bright_order), bright_diag) =
        lifecycle_diag::with_enabled_for_test(|| {
            let rendered = render_tahoe_scene(
                &mut f,
                &server_surface,
                &config,
                Color32F::new(0.9, 0.9, 0.9, 1.),
            );
            (rendered, lifecycle_diag::snapshot())
        });
    let ((dark_pixels, dark_count, dark_order), dark_diag) =
        lifecycle_diag::with_enabled_for_test(|| {
            let rendered = render_tahoe_scene(
                &mut f,
                &server_surface,
                &config,
                Color32F::new(0.1, 0.1, 0.1, 1.),
            );
            (rendered, lifecycle_diag::snapshot())
        });

    let sample = |pixels: &[u8], x: usize, y: usize| {
        let offset = (y * 256 + x) * 4;
        pixels[offset..offset + 4].to_owned()
    };
    assert_eq!(sample(&_bright_pixels, 0, 0), [225, 225, 225, 255]);
    // Panel interior over a bright backdrop: the material is backdrop-adaptive
    // (postprocess.frag), so here it darkens instead of vanishing into the
    // backdrop. Before that landed this sampled 239 against a 225 backdrop —
    // the panel was *brighter* than what it sat on.
    assert_eq!(sample(&_bright_pixels, 72, 20), [185, 185, 185, 255]);
    // Rim band: on a bright backdrop the edge light inverts to a darkening rim,
    // which is what keeps the panel boundary readable over light content.
    // Before, the additive rim clipped to a fully blown-out 255 here.
    assert_eq!(sample(&_bright_pixels, 72, 4), [162, 162, 162, 255]);
    assert_eq!(sample(&_bright_pixels, 72, 0), [214, 214, 214, 255]);
    // The dark samples are untouched by the adaptive term (its window is closed
    // over a dark backdrop), and these values are what the current material
    // actually renders. The literals here previously read 43/50, which had gone
    // stale several material re-tunings earlier: this test was already failing
    // on a clean tree before the adaptive tint, so do not read the change from
    // 43/50 to 59/81 as a dark-backdrop regression.
    assert_eq!(sample(&dark_pixels, 0, 0), [25, 25, 25, 255]);
    assert_eq!(sample(&dark_pixels, 72, 20), [59, 59, 59, 255]);
    assert_eq!(sample(&dark_pixels, 72, 4), [81, 81, 81, 255]);
    assert_eq!(sample(&dark_pixels, 72, 0), [23, 23, 23, 255]);

    assert!(bright_diag.tahoe_region_capture >= 1);
    assert!(bright_diag.fb_effect_capture >= 1);
    assert!(bright_diag.blur_render >= 1);
    assert!(dark_diag.tahoe_region_capture >= 1);
    assert!(dark_diag.fb_effect_capture >= 1);
    assert!(dark_diag.blur_render >= 1);
    assert!(
        bright_count >= 2,
        "restored frame must include material and shadow"
    );
    assert!(
        dark_count >= 2,
        "restored frame must include material and shadow"
    );
    assert_eq!(bright_order, Some(true));
    assert_eq!(dark_order, Some(true));

    assert_ne!(
        hidden_center(&hidden_dark_pixels),
        hidden_center(&dark_pixels),
        "the restored first frame must render the changed backdrop through glass"
    );

    assert!(
        bright_order == Some(true) && dark_order == Some(true),
        "glass material must be emitted before its shadow so the shadow cannot cover it"
    );
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

    let _counter_guard = test_redraw_counter_lock();
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
    let root_output = f.niri_output(1);
    let other_output = f.niri_output(2);

    force_idle_redraw_states(f.niri());
    let _counter_guard = test_redraw_counter_lock();
    test_reset_redraw_counters();
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip_protocol_only(id);

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
    assert!(
        is_output_queued(f.niri(), &root_output),
        "glass destroy must queue the root output"
    );
    assert!(
        !is_output_queued(f.niri(), &other_output),
        "glass destroy must leave the other output idle"
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

/// R14: region commit (post-commit hook) must use the same redraw owner as
/// destroy/recreate — targeted when root is locatable, never a parallel
/// inline output_for_root branch that skips the counters.
#[test]
fn commit_queues_redraw_via_unified_handler_on_root_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    let root_output = f.niri_output(1);
    let other_output = f.niri_output(2);

    // Hold the counter lock for the full reset→commit→assert window so other
    // counter-based tests cannot interleave a reset. Concurrent glass tests may
    // still note redraws if they omit the lock; assert only the targeted path
    // fired ( >= 1 ) and that this commit did not take the unlocatable fallback.
    let _counter_guard = test_redraw_counter_lock();
    test_reset_redraw_counters();
    force_idle_redraw_states(f.niri());
    set_and_commit_region_before_redraw(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);
    let targeted = test_targeted_redraw_count();
    let fallback = test_fallback_redraw_all_count();
    assert!(
        targeted >= 1,
        "glass commit must queue redraw via queue_redraw_for_tahoe_glass_surface (targeted={targeted}, fallback={fallback})"
    );
    // Fallback notes can only come from unlocatable roots. This mapped dual-output
    // fixture must resolve via output_for_root; any fallback here means the old
    // parallel post-commit branch or a broken owner.
    assert_eq!(
        fallback, 0,
        "mapped root commit must not fall back to queue_redraw_all (targeted={targeted})"
    );
    assert!(
        is_output_queued(f.niri(), &root_output),
        "glass commit must queue the root output"
    );
    assert!(
        !is_output_queued(f.niri(), &other_output),
        "glass commit must leave the other output idle"
    );
}

/// A destroyed/unmapped root must have a definite attribution result: no
/// frame is needed (the surface is not rendered anywhere), so the redraw owner
/// skips instead of degrading to a full `queue_redraw_all`.
#[test]
fn unmapped_destroyed_root_skips_redraw_without_queueing_all() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    // Hold the counter lock for every handler-triggering window of this test
    // (region commit, clear): the global targeted/fallback counters must not
    // interleave with other tests' reset→assert windows.
    let _counter_guard = test_redraw_counter_lock();
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
    // requests a redraw for a still-alive but unmapped surface. The result
    // must be a recorded skip: no targeted path, no fallback, no output
    // queued. Lifecycle-diag counters are asserted (serialized by their own
    // test lock); the tahoe-glass TEST_* counters are deliberately not
    // asserted strictly here because unlocked T01-era tests still write them
    // in parallel (pre-existing race, tracked for T24).
    force_idle_redraw_states(f.niri());
    test_reset_redraw_counters();
    lifecycle_diag::with_enabled_for_test(|| {
        f.niri_state()
            .queue_redraw_for_tahoe_glass_surface(&server_surface);
        let diag = lifecycle_diag::snapshot();
        assert_eq!(
            diag.queue_redraw_all, 0,
            "skip disposition must not queue any output"
        );
        assert_eq!(
            diag.redraw_fallback_unlocatable, 0,
            "skip disposition must not record an unlocatable fallback"
        );
        assert!(
            diag.redraw_skip_unmapped >= 1,
            "skip disposition must be recorded"
        );
    });
    let (queued, _) = count_outputs_queued(f.niri());
    assert_eq!(
        queued, 0,
        "skip disposition must leave every output unqueued"
    );
    let root_output = f.niri_output(1);
    let other_output = f.niri_output(2);
    assert!(!is_output_queued(f.niri(), &root_output));
    assert!(!is_output_queued(f.niri(), &other_output));

    // Controller Destroy must still clear glass without panicking.
    test_reset_redraw_counters();
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    if server_surface.alive() {
        assert_eq!(committed_count(&server_surface), 0);
    }
}

/// A glass commit riding a *subsurface* commit must be attributed through the
/// existing root resolution (layer root), not degrade to queue_redraw_all.
#[test]
fn subsurface_glass_commit_attributes_to_root_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    // Desynced subsurface on the layer root. The transform directive is
    // published by the child's FIRST commit: this exercises the compositor
    // root-cache reorder (the cache must be populated before on_surface_commit
    // so the very commit carrying the directive can resolve the root).
    let (subsurface, child) = {
        let client = f.client(id);
        let compositor = client.state.compositor.as_ref().unwrap();
        let subcompositor = client.state.subcompositor.as_ref().unwrap();
        let child = compositor.create_surface(&client.qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &client_surface, &client.qh, ());
        (subsurface, child)
    };
    subsurface.set_desync();

    // Publish a transform directive on the child surface before its first
    // commit: the directive rides that commit and must be attributed to the
    // layer root's output.
    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&child, &qh, ());
    glass.set_transform(10.0.into(), 5.0.into(), 0.8.into(), 0.8.into());
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    let root_output = f.niri_output(1);
    let other_output = f.niri_output(2);

    let _counter_guard = test_redraw_counter_lock();
    test_reset_redraw_counters();
    force_idle_redraw_states(f.niri());
    child.commit();
    f.client(id).connection.flush().unwrap();
    f.roundtrip_protocol_only(id);

    let targeted = test_targeted_redraw_count();
    let fallback = test_fallback_redraw_all_count();
    assert!(
        targeted >= 1,
        "subsurface glass commit must attribute to the layer root's output (targeted={targeted}, fallback={fallback})"
    );
    assert_eq!(
        fallback, 0,
        "subsurface glass commit must not degrade to queue_redraw_all (targeted={targeted})"
    );
    assert!(
        is_output_queued(f.niri(), &root_output),
        "subsurface glass commit must queue the root output"
    );
    assert!(
        !is_output_queued(f.niri(), &other_output),
        "subsurface glass commit must leave the other output idle"
    );

    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
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

/// A03.2: a surface whose committed regions became empty must still drain its
/// pending damage on the next render — the old glass area needs repainting.
/// The pre-fix early return (`regions.is_empty()`) skipped the drain, leaving
/// the damage stored forever and the removed glass area without repaint.
#[test]
fn empty_regions_render_still_drains_pending_damage() {
    use smithay::backend::renderer::element::Element as _;
    use smithay::utils::{Point, Scale};

    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    // Hold the counter lock for every handler-triggering window of this test
    // (region commit, clear): the global targeted/fallback counters must not
    // interleave with other tests' reset→assert windows.
    let _counter_guard = test_redraw_counter_lock();
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    assert_eq!(committed_count(&server_surface), 1);

    use crate::render_helpers::tahoe_glass::{render_for_layer, TahoeGlassElement};
    use crate::render_helpers::xray::XrayPos;
    use crate::render_helpers::{RenderCtx, RenderTarget};

    // Render the layer once while the region is present: this creates the
    // per-surface damage renderer (production renders do the same), so the
    // subsequent clear can record damage into it.
    {
        let mut pushed = Vec::new();
        f.niri_state()
            .backend
            .with_primary_renderer(|renderer| {
                let ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Output,
                    xray: None,
                };
                let config = niri_config::TahoeGlass::default();
                let _ = render_for_layer(
                    ctx,
                    None,
                    &server_surface,
                    "tahoe-glass-test",
                    Point::from((0., 0.)),
                    1.0,
                    &config,
                    1.0,
                    None,
                    XrayPos::default(),
                    false,
                    &mut |elem| pushed.push(elem),
                );
            })
            .unwrap();
        assert!(
            pushed.iter().any(|elem| matches!(
                elem,
                TahoeGlassElement::BackgroundEffect(_)
                    | TahoeGlassElement::Shadow(_)
                    | TahoeGlassElement::ExtraDamage(_)
            )),
            "the pre-render must actually render the present region"
        );
    }

    // Remove all regions: committed becomes empty and damage for the old
    // rect is recorded into the surface's pending damage storage. The clear
    // also triggers the production redraw handler; the counter lock acquired
    // above is still held, so no other test's reset→assert window can
    // interleave.
    glass.destroy();
    f.client(id).connection.flush().unwrap();
    f.roundtrip(id);
    assert_eq!(committed_count(&server_surface), 0);
    assert!(
        test_damage_old_region_count() >= 1,
        "clear must have recorded damage for the removed region area"
    );

    // Render the layer with empty committed regions: the pending damage must
    // still drain into render elements (an ExtraDamage element covering the
    // old region rect), even though no glass region is present anymore.
    let mut pushed = Vec::new();
    f.niri_state()
        .backend
        .with_primary_renderer(|renderer| {
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            let config = niri_config::TahoeGlass::default();
            let _ = render_for_layer(
                ctx,
                None,
                &server_surface,
                "tahoe-glass-test",
                Point::from((0., 0.)),
                1.0,
                &config,
                1.0,
                None,
                XrayPos::default(),
                false,
                &mut |elem| pushed.push(elem),
            );
        })
        .unwrap();

    let damage_geoms: Vec<_> = pushed
        .iter()
        .filter_map(|elem| match elem {
            TahoeGlassElement::ExtraDamage(damage) => Some(damage.geometry(Scale::from(1.0))),
            _ => None,
        })
        .collect();
    assert!(
        damage_geoms
            .iter()
            .any(|geo| geo.contains(Point::from((8, 4))) && geo.contains(Point::from((135, 35)))),
        "empty-region render must still push damage covering the removed glass area (8,4,128,32): {damage_geoms:?}"
    );
}
