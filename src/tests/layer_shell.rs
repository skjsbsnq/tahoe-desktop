use insta::assert_snapshot;
use niri_config::animations::{
    Animation, Curve, EasingParams, Kind, LayerAnimationEdge, LayerAnimationOrigin, LayerCloseAnim,
    LayerCloseAnimationStyle, LayerOpenAnim, LayerOpenAnimationStyle,
};
use niri_config::Config;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    Anchor, KeyboardInteractivity,
};
use smithay::utils::Point;
use std::time::Duration;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::tests::client::{LayerConfigureProps, LayerMargin};

#[test]
fn simple_top_anchor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 50");
}

#[test]
fn layer_rule_animations_resolve_by_namespace_and_merge() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    duration-ms 111
                    curve "ease-out-cubic"
                }
                layer-close {
                    duration-ms 77
                    curve "ease-out-quad"
                }
            }
        }

        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    duration-ms 222
                    curve "linear"
                }
            }
        }

        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-close {
                    duration-ms 33
                    curve "linear"
                }
            }
        }

        layer-rule {
            match namespace="^plain-layer$"
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_layer(&mut f, id, "animated-layer");
    let (layer_open, layer_close, animations_ongoing) =
        resolved_layer_animation_rules(&mut f, "animated-layer");
    assert_eq!(layer_open, Some(layer_open_anim(222, Curve::Linear)));
    assert_eq!(layer_close, Some(layer_close_anim(33, Curve::Linear)));
    assert!(animations_ongoing);

    advance_layer_animations(&mut f, Duration::from_millis(300));
    let (_, _, animations_ongoing) = resolved_layer_animation_rules(&mut f, "animated-layer");
    assert!(!animations_ongoing);

    map_layer(&mut f, id, "plain-layer");
    let (layer_open, layer_close, animations_ongoing) =
        resolved_layer_animation_rules(&mut f, "plain-layer");
    assert_eq!(layer_open, None);
    assert_eq!(layer_close, None);
    assert!(!animations_ongoing);
}

#[test]
fn layer_rule_animations_select_style_by_namespace() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^fade-layer$"

            animations {
                layer-open {
                    style "fade"
                    opacity-from 0.2
                    duration-ms 120
                    curve "menu-decel"
                }
                layer-close {
                    style "fade"
                    opacity-to 0.3
                    duration-ms 80
                    curve "menu-accel"
                }
            }
        }

        layer-rule {
            match namespace="^slide-layer$"

            animations {
                layer-open {
                    style "slide"
                    edge "top"
                    distance 24
                    duration-ms 140
                    curve "emphasized-decel"
                }
                layer-close {
                    style "slide"
                    edge "bottom"
                    distance 18
                    duration-ms 90
                    curve "emphasized-accel"
                }
            }
        }

        layer-rule {
            match namespace="^pop-layer$"

            animations {
                layer-open {
                    style "popin"
                    scale-from 0.93
                    origin "anchor"
                    duration-ms 160
                    curve "stall"
                }
                layer-close {
                    style "popout"
                    scale-to 0.95
                    origin "anchor"
                    duration-ms 100
                    curve "linear"
                }
            }
        }

        layer-rule {
            match namespace="^edge-reveal-layer$"

            animations {
                layer-open {
                    style "edge-reveal"
                    edge "top"
                    distance 18
                    opacity-from 0.82
                    transform-duration-ms 180
                    transform-curve "emphasized-decel"
                    opacity-duration-ms 90
                    opacity-curve "standard-decel"
                }
                layer-close {
                    style "edge-reveal"
                    edge "bottom"
                    distance 14
                    opacity-to 0.55
                    transform-duration-ms 120
                    transform-curve "emphasized-accel"
                    opacity-duration-ms 80
                    opacity-curve "menu-accel"
                }
            }
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_layer(&mut f, id, "fade-layer");
    let (layer_open, layer_close, _) = resolved_layer_animation_rules(&mut f, "fade-layer");
    assert_eq!(
        layer_open,
        Some(LayerOpenAnim {
            anim: easing_anim(120, Curve::CubicBezier(0.1, 1., 0., 1.)),
            transform_anim: easing_anim(120, Curve::CubicBezier(0.1, 1., 0., 1.)),
            opacity_anim: easing_anim(120, Curve::CubicBezier(0.1, 1., 0., 1.)),
            style: LayerOpenAnimationStyle::Fade,
            opacity_from: 0.2,
            ..Default::default()
        })
    );
    assert_eq!(
        layer_close,
        Some(LayerCloseAnim {
            anim: easing_anim(80, Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
            transform_anim: easing_anim(80, Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
            opacity_anim: easing_anim(80, Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
            style: LayerCloseAnimationStyle::Fade,
            opacity_to: 0.3,
            ..Default::default()
        })
    );

    map_layer(&mut f, id, "slide-layer");
    let (layer_open, layer_close, _) = resolved_layer_animation_rules(&mut f, "slide-layer");
    assert_eq!(
        layer_open,
        Some(LayerOpenAnim {
            anim: easing_anim(140, Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
            transform_anim: easing_anim(140, Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
            opacity_anim: easing_anim(140, Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
            style: LayerOpenAnimationStyle::Slide,
            edge: LayerAnimationEdge::Top,
            distance: 24.,
            ..Default::default()
        })
    );
    assert_eq!(
        layer_close,
        Some(LayerCloseAnim {
            anim: easing_anim(90, Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
            transform_anim: easing_anim(90, Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
            opacity_anim: easing_anim(90, Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
            style: LayerCloseAnimationStyle::Slide,
            edge: LayerAnimationEdge::Bottom,
            distance: 18.,
            ..Default::default()
        })
    );

    map_layer(&mut f, id, "pop-layer");
    let (layer_open, layer_close, _) = resolved_layer_animation_rules(&mut f, "pop-layer");
    assert_eq!(
        layer_open,
        Some(LayerOpenAnim {
            anim: easing_anim(160, Curve::CubicBezier(1., -0.1, 0.7, 0.85)),
            transform_anim: easing_anim(160, Curve::CubicBezier(1., -0.1, 0.7, 0.85)),
            opacity_anim: easing_anim(160, Curve::CubicBezier(1., -0.1, 0.7, 0.85)),
            style: LayerOpenAnimationStyle::Popin,
            scale_from: 0.93,
            origin: LayerAnimationOrigin::Anchor,
            ..Default::default()
        })
    );
    assert_eq!(
        layer_close,
        Some(LayerCloseAnim {
            anim: easing_anim(100, Curve::Linear),
            transform_anim: easing_anim(100, Curve::Linear),
            opacity_anim: easing_anim(100, Curve::Linear),
            style: LayerCloseAnimationStyle::Popout,
            scale_to: 0.95,
            origin: LayerAnimationOrigin::Anchor,
            ..Default::default()
        })
    );

    map_layer(&mut f, id, "edge-reveal-layer");
    let (layer_open, layer_close, _) = resolved_layer_animation_rules(&mut f, "edge-reveal-layer");
    assert_eq!(
        layer_open,
        Some(LayerOpenAnim {
            anim: LayerOpenAnim::default().anim,
            transform_anim: easing_anim(180, Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
            opacity_anim: easing_anim(90, Curve::CubicBezier(0., 0., 0., 1.)),
            style: LayerOpenAnimationStyle::EdgeReveal,
            opacity_from: 0.82,
            edge: LayerAnimationEdge::Top,
            distance: 18.,
            ..Default::default()
        })
    );
    assert_eq!(
        layer_close,
        Some(LayerCloseAnim {
            anim: LayerCloseAnim::default().anim,
            transform_anim: easing_anim(120, Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
            opacity_anim: easing_anim(80, Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
            style: LayerCloseAnimationStyle::EdgeReveal,
            opacity_to: 0.55,
            edge: LayerAnimationEdge::Bottom,
            distance: 14.,
            ..Default::default()
        })
    );
}

#[test]
fn layer_rule_animations_resolve_split_channels() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^split-layer$"

            animations {
                layer-open {
                    duration-ms 180
                    curve "ease-out-cubic"
                    transform-duration-ms 240
                    transform-curve "emphasized-decel"
                    opacity-duration-ms 90
                    opacity-curve "linear"
                    opacity-delay-ms 25
                }
                layer-close {
                    duration-ms 150
                    curve "ease-out-quad"
                    transform-duration-ms 120
                    transform-curve "emphasized-accel"
                    opacity-duration-ms 80
                    opacity-curve "menu-accel"
                    opacity-delay-ms 10
                }
            }
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_layer(&mut f, id, "split-layer");
    let (layer_open, layer_close, _) = resolved_layer_animation_rules(&mut f, "split-layer");
    assert_eq!(
        layer_open,
        Some(LayerOpenAnim {
            anim: easing_anim(180, Curve::EaseOutCubic),
            transform_anim: easing_anim(240, Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
            opacity_anim: easing_anim(90, Curve::Linear),
            opacity_delay_ms: 25,
            ..Default::default()
        })
    );
    assert_eq!(
        layer_close,
        Some(LayerCloseAnim {
            anim: easing_anim(150, Curve::EaseOutQuad),
            transform_anim: easing_anim(120, Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
            opacity_anim: easing_anim(80, Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
            opacity_delay_ms: 10,
            ..Default::default()
        })
    );
}

#[test]
fn layer_close_animation_uses_snapshot_and_cleans_up() {
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
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    assert!(f.niri().closing_layers.is_empty());

    unmap_layer(&mut f, id, &surface);
    assert!(f
        .niri()
        .mapped_layer_surfaces
        .values()
        .all(|mapped| mapped.surface().namespace() != "animated-layer"));
    assert_eq!(f.niri().closing_layers.len(), 1);
    assert!(f.niri().closing_layers[0].live_close_effects.is_none());
    assert!(f.niri().closing_layers[0]
        .animation
        .are_animations_ongoing());

    advance_layer_animations(&mut f, Duration::from_millis(40));
    assert!(f.niri().closing_layers.is_empty());

    let plain_surface = map_layer(&mut f, id, "plain-layer");
    unmap_layer(&mut f, id, &plain_surface);
    assert!(f.niri().closing_layers.is_empty());
}

#[test]
fn tahoe_layer_close_keeps_fallback_blur_live() {
    let config = Config::parse_mem(
        r##"
        layer-rule {
            match namespace="^tahoe-control-center$"

            geometry-corner-radius 28

            background-effect {
                xray false
                blur true
                noise 0.006
                saturation 1.16
                tint-color "#ffffff"
                tint-amount 0.04
                edge-highlight 0.34
                refraction 0.04
            }

            animations {
                layer-close {
                    style "edge-reveal"
                    edge "top"
                    distance 24
                    opacity-to 1
                    transform-duration-ms 33
                    transform-curve "linear"
                    opacity-duration-ms 0
                }
            }
        }
        "##,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "tahoe-control-center");
    unmap_layer(&mut f, id, &surface);

    assert_eq!(f.niri().closing_layers.len(), 1);
    assert!(f.niri().closing_layers[0].live_close_effects.is_some());
}

#[test]
fn layer_close_animation_opacity_delay_extends_lifetime() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-close {
                    style "fade"
                    opacity-to 0
                    transform-duration-ms 0
                    transform-curve "linear"
                    opacity-duration-ms 100
                    opacity-curve "linear"
                    opacity-delay-ms 50
                }
            }
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    advance_layer_animations(&mut f, Duration::from_millis(120));
    assert_eq!(f.niri().closing_layers.len(), 1);

    advance_layer_animations(&mut f, Duration::from_millis(40));
    assert!(f.niri().closing_layers.is_empty());
}

#[test]
fn layer_close_animation_is_cancelled_on_reopen() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-close {
                    duration-ms 500
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
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    remap_layer(&mut f, id, &surface);
    assert!(f.niri().closing_layers.is_empty());
    assert!(f
        .niri()
        .mapped_layer_surfaces
        .values()
        .any(|mapped| mapped.surface().namespace() == "animated-layer"));
}

#[test]
fn layer_close_animation_interrupted_open_starts_from_current_visual_state() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    style "popin"
                    opacity-from 0.2
                    scale-from 0.8
                    duration-ms 1000
                    curve "linear"
                }
                layer-close {
                    style "popout"
                    opacity-to 0.1
                    scale-to 0.6
                    duration-ms 500
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
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    advance_layer_animations(&mut f, Duration::from_millis(250));

    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    let start = f.niri().closing_layers[0].animation.start_state_for_tests();
    assert!((start.start_alpha - 0.4).abs() < 0.001);
    assert!((start.start_scale - 0.85).abs() < 0.001);
    assert_eq!(start.start_offset, Point::from((0., 0.)));
}

#[test]
fn layer_close_animation_interrupted_open_uses_opacity_delay() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    style "popin"
                    opacity-from 0.2
                    scale-from 0.8
                    transform-duration-ms 1000
                    transform-curve "linear"
                    opacity-duration-ms 1000
                    opacity-curve "linear"
                    opacity-delay-ms 200
                }
                layer-close {
                    style "popout"
                    opacity-to 0.1
                    scale-to 0.6
                    duration-ms 500
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
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    advance_layer_animations(&mut f, Duration::from_millis(250));

    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    let start = f.niri().closing_layers[0].animation.start_state_for_tests();
    assert!((start.start_alpha - 0.24).abs() < 0.001);
    assert!((start.start_scale - 0.85).abs() < 0.001);
    assert_eq!(start.start_offset, Point::from((0., 0.)));
}

#[test]
fn layer_close_animation_interrupted_slide_open_starts_from_current_offset() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    style "slide"
                    edge "top"
                    distance 40
                    opacity-from 0.5
                    duration-ms 1000
                    curve "linear"
                }
                layer-close {
                    style "slide"
                    edge "bottom"
                    distance 20
                    opacity-to 0.25
                    duration-ms 500
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
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    advance_layer_animations(&mut f, Duration::from_millis(250));

    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    let start = f.niri().closing_layers[0].animation.start_state_for_tests();
    assert!((start.start_alpha - 0.625).abs() < 0.001);
    assert!((start.start_scale - 1.).abs() < 0.001);
    assert!((start.start_offset.x - 0.).abs() < 0.001);
    assert!((start.start_offset.y - -30.).abs() < 0.05);
}

#[test]
fn layer_close_animation_interrupted_edge_reveal_open_starts_from_current_offset() {
    let config = Config::parse_mem(
        r#"
        layer-rule {
            match namespace="^animated-layer$"

            animations {
                layer-open {
                    style "edge-reveal"
                    edge "top"
                    distance 20
                    opacity-from 0.8
                    transform-duration-ms 1000
                    transform-curve "linear"
                    opacity-duration-ms 500
                    opacity-curve "linear"
                }
                layer-close {
                    style "edge-reveal"
                    edge "bottom"
                    distance 12
                    opacity-to 0.55
                    transform-duration-ms 500
                    transform-curve "linear"
                    opacity-duration-ms 400
                    opacity-curve "linear"
                }
            }
        }
        "#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_layer(&mut f, id, "animated-layer");
    advance_layer_animations(&mut f, Duration::from_millis(250));

    unmap_layer(&mut f, id, &surface);
    assert_eq!(f.niri().closing_layers.len(), 1);

    let start = f.niri().closing_layers[0].animation.start_state_for_tests();
    assert!((start.start_alpha - 0.9).abs() < 0.001);
    assert!((start.start_scale - 1.).abs() < 0.001);
    assert!((start.start_offset.x - 0.).abs() < 0.001);
    assert!((start.start_offset.y - -15.).abs() < 0.05);
}

#[test]
fn margin_overflow() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top | Anchor::Bottom),
        margin: Some(LayerMargin {
            top: i32::MAX,
            right: i32::MAX,
            bottom: i32::MAX,
            left: i32::MAX,
        }),
        exclusive_zone: Some(i32::MAX),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 0 × 0");

    // Add a second one for good measure.
    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top | Anchor::Bottom),
        margin: Some(LayerMargin {
            top: i32::MAX,
            right: i32::MAX,
            bottom: i32::MAX,
            left: i32::MAX,
        }),
        exclusive_zone: Some(i32::MAX),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 0 × 0");
}

#[test]
fn unmap_through_null_buffer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 50");

    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // No new configure since nothing changed.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Unmap by attaching a null buffer. This moves the surface back to pre-initial-commit stage.
    layer.attach_null();
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // Configures must be empty because we haven't done an initial commit yet.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Do the initial commit again.
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 100)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // This is the new initial configure.
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 100");

    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"");
}

#[test]
fn multiple_commits_before_mapping() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 50");

    // Change something that won't cause a configure.
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        kb_interactivity: Some(KeyboardInteractivity::OnDemand),
        ..Default::default()
    });
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // No new configure since the size hasn't changed.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Change something that will cause a configure.
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 100)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // Configure with new size.
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 100");

    // Map.
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // No new configure since nothing changed.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Unmap by attaching a null buffer. This moves the surface back to pre-initial-commit stage.
    layer.attach_null();
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // Configures must be empty because we haven't done an initial commit yet.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Same configure props as before, but since we unmapped, we should get a new initial
    // configure (that will happen to match the previous configure we had got while mapped).
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 100)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 100");

    // Change something that won't cause a configure.
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 100)),
        kb_interactivity: Some(KeyboardInteractivity::OnDemand),
        ..Default::default()
    });
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // No new configure since the size hasn't changed.
    assert_snapshot!(layer.format_recent_configures(), @"");

    // Change something that will cause a configure.
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    // Configure with new size.
    assert_snapshot!(layer.format_recent_configures(), @"size: 1920 × 50");
}

fn map_layer(f: &mut Fixture, id: ClientId, namespace: &str) -> WlSurface {
    let surface = {
        let layer = f.client(id).create_layer(None, Layer::Top, namespace);
        let surface = layer.surface.clone();
        layer.set_configure_props(LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
            size: Some((0, 50)),
            ..Default::default()
        });
        layer.commit();
        surface
    };
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn unmap_layer(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let layer = f.client(id).layer(surface);
    layer.attach_null();
    layer.commit();
    f.double_roundtrip(id);
}

fn remap_layer(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let layer = f.client(id).layer(surface);
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
        size: Some((0, 50)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(surface);
    layer.attach_new_buffer();
    layer.set_size(100, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);
}

fn resolved_layer_animation_rules(
    f: &mut Fixture,
    namespace: &str,
) -> (Option<LayerOpenAnim>, Option<LayerCloseAnim>, bool) {
    let mapped = f
        .niri()
        .mapped_layer_surfaces
        .values()
        .find(|mapped| mapped.surface().namespace() == namespace)
        .unwrap();

    (
        mapped.rules().layer_open,
        mapped.rules().layer_close,
        mapped.are_animations_ongoing(),
    )
}

fn advance_layer_animations(f: &mut Fixture, elapsed: Duration) {
    let niri = f.niri();
    let now = niri.clock.now_unadjusted();
    niri.clock.set_unadjusted(now + elapsed);
    niri.advance_animations();
}

fn easing_anim(duration_ms: u32, curve: Curve) -> Animation {
    Animation {
        off: false,
        kind: Kind::Easing(EasingParams { duration_ms, curve }),
    }
}

fn layer_open_anim(duration_ms: u32, curve: Curve) -> LayerOpenAnim {
    let anim = easing_anim(duration_ms, curve);
    LayerOpenAnim {
        anim,
        transform_anim: anim,
        opacity_anim: anim,
        ..Default::default()
    }
}

fn layer_close_anim(duration_ms: u32, curve: Curve) -> LayerCloseAnim {
    let anim = easing_anim(duration_ms, curve);
    LayerCloseAnim {
        anim,
        transform_anim: anim,
        opacity_anim: anim,
        ..Default::default()
    }
}
