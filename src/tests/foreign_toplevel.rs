use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{Point, Rectangle, Size};
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::tests::client::LayerConfigureProps;

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

#[test]
fn foreign_toplevel_set_rectangle_tracks_layer_surface_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    create_window(&mut f, id);

    let wl_output = f.client(id).output("headless-2");
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
    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);

    let output = f.niri_output(2);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    let rect = mapped.foreign_toplevel_rect().unwrap();
    assert_eq!(rect.output, output);
    assert_eq!(
        rect.rect,
        Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
    );
    let stored_rect = rect.rect;

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

    handle.set_rectangle(&layer_surface, 10, 20, 0, 40);
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(mapped.foreign_toplevel_rect().is_none());

    handle.set_rectangle(&layer_surface, 10, 20, 30, 40);
    f.double_roundtrip(id);
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.double_roundtrip(id);
    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(mapped.foreign_toplevel_rect().is_none());
}
