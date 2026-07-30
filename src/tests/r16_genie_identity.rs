//! R16: Genie stable render-element identity and zero per-frame construction sites.
//!
//! Pre-R16 `render_genie` allocated each frame: `Rc::new`, `HashMap::from`, `String::from`,
//! and `ShaderRenderElement::new` → `Id::new`. This harness asserts the production path keeps
//! a stable `Id`, advances the commit counter for dynamic uniforms, and that the source of
//! `render_genie` no longer contains those per-frame constructors.

use std::path::PathBuf;
use std::time::Duration;

use niri_config::Config;
use smithay::backend::renderer::element::Element;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{Point, Rectangle, Size};
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::LayerConfigureProps;
use super::*;
use crate::layout::minimize_window_animation::MinimizeWindowAnimationRenderElement;
use crate::render_helpers::{RenderCtx, RenderTarget};
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

fn set_time(niri: &mut crate::niri::Niri, time: Duration) {
    let now = niri.clock.now();
    niri.clock.set_unadjusted(now);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(Duration::ZERO);
    niri.clock.set_rate(1.0);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(time);
    let _ = niri.clock.now();
    niri.clock.set_rate(0.0);
}

/// Source-level deletion proof for the four forbidden per-frame constructors in `render_genie`.
#[test]
fn r16_render_genie_source_has_zero_per_frame_constructors() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/layout/minimize_window_animation.rs");
    let src = std::fs::read_to_string(&path).expect("read minimize_window_animation.rs");
    let start = src
        .find("fn render_genie(")
        .expect("render_genie must exist");
    // Function body ends at the next top-level `fn` / impl item at column 0 after indent of 4 spaces
    // for methods — take until the matching closing of this method by scanning to the next
    // `    fn ` or `    pub` at the same indent after the body starts, or the end of impl.
    let after = &src[start..];
    let body_start = after.find('{').expect("render_genie body");
    let body = &after[body_start..];
    // Stop at the next method-level item (`    fn ` or `    pub` after a closing `}`).
    // Heuristic: first occurrence of "\n    fn " or "\n    pub" or "\n}" that closes the impl
    // after a reasonable minimum; we use the known following method name if present.
    let end_markers = [
        "\n    /// Stable Genie",
        "\n    #[cfg(test)]",
        "\n}\n\nconst GENIE",
    ];
    let mut end = body.len();
    for marker in end_markers {
        if let Some(i) = body.find(marker) {
            end = end.min(i);
        }
    }
    let render_genie = &body[..end];
    // Strip line comments so documentation of the deleted sites does not false-positive.
    let code_only: String = render_genie
        .lines()
        .map(|line| {
            if let Some((code, _)) = line.split_once("//") {
                code
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in [
        "Rc::new",
        "HashMap::from",
        "String::from",
        "ShaderRenderElement::new",
    ] {
        assert!(
            !code_only.contains(forbidden),
            "render_genie must not contain {forbidden}; body excerpt:\n{render_genie}"
        );
    }
    // Must still use the stable element mutation path.
    assert!(
        render_genie.contains("genie_shader")
            && render_genie.contains("with_uniforms_mut")
            && render_genie.contains("set_texture")
            && render_genie.contains("damage_all"),
        "render_genie must update the stable element in place"
    );
}

#[test]
fn r16_genie_element_id_stable_across_frames_and_commit_advances() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);

        set_time(f.niri(), Duration::ZERO);
        f.niri_complete_animations();

        lifecycle_diag::reset();
        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        assert!(
            diag.genie_create >= 1,
            "minimize with dock target must create Genie snapshot"
        );

        // Capture stable id from the animation object before any render.
        let id_before = {
            let mut found = None;
            f.niri()
                .layout
                .active_workspace()
                .unwrap()
                .scrolling()
                .test_for_each_minimize_restore(|_, _, anim| {
                    found = Some(anim.test_genie_element_id());
                });
            found.expect("active minimize Genie animation")
        };
        let commit_before = {
            let mut found = None;
            f.niri()
                .layout
                .active_workspace()
                .unwrap()
                .scrolling()
                .test_for_each_minimize_restore(|_, _, anim| {
                    found = Some(anim.test_genie_commit());
                });
            found.unwrap()
        };

        let view_rect = Rectangle::from_size(Size::from((1920.0, 1080.0)));
        let scale = smithay::utils::Scale::from(1.0);

        // Frame 1: render overlay list via production path.
        let (id_f1, commit_f1) = {
            let state = f.niri_state();
            state
                .backend
                .with_primary_renderer(|renderer| {
                    let mut ctx = RenderCtx {
                        renderer,
                        target: RenderTarget::Output,
                        xray: None,
                    };
                    let mut elems: Vec<MinimizeWindowAnimationRenderElement> = Vec::new();
                    state
                        .niri
                        .layout
                        .active_workspace()
                        .unwrap()
                        .scrolling()
                        .test_render_minimize_restore_overlays(ctx.r(), view_rect, scale, |elem| {
                            elems.push(elem)
                        });
                    assert_eq!(elems.len(), 1, "one minimize overlay");
                    let e = &elems[0];
                    match e {
                        MinimizeWindowAnimationRenderElement::Shader(s) => {
                            (s.id().clone(), s.current_commit())
                        }
                        other => panic!("expected Genie Shader element, got {other:?}"),
                    }
                })
                .expect("renderer")
        };

        assert_eq!(
            id_f1, id_before,
            "frame-1 rendered element Id must equal the animation's stable Id"
        );
        assert_ne!(
            commit_f1, commit_before,
            "first render_genie must advance commit for dynamic uniforms"
        );

        // Advance animation clock so uniforms change again.
        set_time(f.niri(), Duration::from_millis(250));
        f.niri().advance_animations();

        let (id_f2, commit_f2) = {
            let state = f.niri_state();
            state
                .backend
                .with_primary_renderer(|renderer| {
                    let mut ctx = RenderCtx {
                        renderer,
                        target: RenderTarget::Output,
                        xray: None,
                    };
                    let mut elems: Vec<MinimizeWindowAnimationRenderElement> = Vec::new();
                    state
                        .niri
                        .layout
                        .active_workspace()
                        .unwrap()
                        .scrolling()
                        .test_render_minimize_restore_overlays(ctx.r(), view_rect, scale, |elem| {
                            elems.push(elem)
                        });
                    assert_eq!(elems.len(), 1);
                    let e = &elems[0];
                    match e {
                        MinimizeWindowAnimationRenderElement::Shader(s) => {
                            (s.id().clone(), s.current_commit())
                        }
                        other => panic!("expected Genie Shader element, got {other:?}"),
                    }
                })
                .expect("renderer")
        };

        assert_eq!(
            id_f2, id_f1,
            "element Id must remain stable across animation frames"
        );
        assert_ne!(
            commit_f2, commit_f1,
            "commit counter must advance when progress/uniforms change"
        );

        // Reverse to restore: same animation entry / same element Id (R05 reverse reuses texture).
        f.client(id).foreign_toplevel(0).unset_minimized();
        f.double_roundtrip(id);

        let id_restore = {
            let mut found = None;
            f.niri()
                .layout
                .active_workspace()
                .unwrap()
                .scrolling()
                .test_for_each_minimize_restore(|_, dir, anim| {
                    assert_eq!(
                        dir,
                        crate::layout::lifecycle_controller::LifecycleAnimDirection::Restore
                    );
                    found = Some(anim.test_genie_element_id());
                });
            found.expect("restore reverse reuses the same Genie animation entry")
        };
        assert_eq!(
            id_restore, id_f1,
            "reverse_to_restore must keep the same stable Genie element Id"
        );

        eprintln!(
            "R16_SAMPLE kind=genie_identity_stable \
             id_frames_equal=1 commit_advanced_f1=1 commit_advanced_f2=1 reverse_same_id=1"
        );
    });
}

#[test]
fn r16_genie_texture_fallback_when_target_rect_invalid() {
    // Without a usable dock target, Genie returns None and production uses texture fallback.
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = create_window(&mut f, id, 200, 200);
    // No dock rectangle → target is None → texture fallback path.
    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    let view_rect = Rectangle::from_size(Size::from((1920.0, 1080.0)));
    let scale = smithay::utils::Scale::from(1.0);
    let state = f.niri_state();
    let kinds = state
        .backend
        .with_primary_renderer(|renderer| {
            let mut ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            let mut kinds = Vec::new();
            state
                .niri
                .layout
                .active_workspace()
                .unwrap()
                .scrolling()
                .test_render_minimize_restore_overlays(ctx.r(), view_rect, scale, |elem| {
                    kinds.push(match &elem {
                        MinimizeWindowAnimationRenderElement::Shader(_) => "shader",
                        MinimizeWindowAnimationRenderElement::Texture(_) => "texture",
                    });
                });
            kinds
        })
        .expect("renderer");

    assert_eq!(
        kinds.len(),
        1,
        "minimize without dock must still produce one overlay"
    );
    assert_eq!(
        kinds[0], "texture",
        "without target rect, render_genie returns None and texture fallback is used"
    );
}

/// Render-target variant binding: Output path uses the primary buffer texture binding on the
/// stable element; a second render with the same element Id must still succeed after clock advance.
#[test]
fn r16_output_target_reuses_stable_binding() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = create_window(&mut f, id, 320, 240);
    let _dock = map_dock_and_set_rect(&mut f, id, 1, 40, 60, 48, 48);

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();
    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    let view_rect = Rectangle::new(Point::from((0.0, 0.0)), Size::from((1920.0, 1080.0)));
    let scale = smithay::utils::Scale::from(1.0);

    let mut ids = Vec::new();
    for ms in [0u64, 100, 200] {
        set_time(f.niri(), Duration::from_millis(ms));
        f.niri().advance_animations();
        let state = f.niri_state();
        let id = state
            .backend
            .with_primary_renderer(|renderer| {
                let mut ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Output,
                    xray: None,
                };
                let mut last = None;
                state
                    .niri
                    .layout
                    .active_workspace()
                    .unwrap()
                    .scrolling()
                    .test_render_minimize_restore_overlays(ctx.r(), view_rect, scale, |elem| {
                        if let MinimizeWindowAnimationRenderElement::Shader(s) = &elem {
                            last = Some(s.id().clone());
                        }
                    });
                last
            })
            .flatten();
        if let Some(id) = id {
            ids.push(id);
        }
    }

    assert!(
        ids.len() >= 2,
        "need at least two Genie frames; got {}",
        ids.len()
    );
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "all Output frames must share one stable element Id: {ids:?}"
    );
}

/// T-20: a `set_rectangle` mid-flight does **not** retarget an active minimize
/// Genie — the minimize target locks to the dock endpoint sampled at
/// `start_minimize` time for the animation's native duration. The shelf
/// thumbnail (the `DockMinimizedWindow` delegate created when the window
/// enters the shelf) shares the toplevel handle with the dock icon and is a
/// later "last writer wins" publisher; if the minimize Genie followed mid-air
/// rects it would be yanked to the shelf thumbnail (left) and flicker for the
/// whole animation. The stable element Id is still unchanged (no recreate).
/// Restore direction still retargets (see `r16_genie_retarget_updates_active_restore_target`).
/// Output 1 (1920×1080) with a Left|Bottom 200×80 dock yields output-local (x, 1000 + y).
#[test]
fn r16_genie_retarget_minimize_locks_takeoff_target() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = create_window(&mut f, id, 400, 300);
    let dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48); // A = (20, 1040)

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    // Minimize → Genie targets the dock rect A.
    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    let (id_before, target_a) = {
        let mut found_id = None;
        let mut found_target = None;
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .scrolling()
            .test_for_each_minimize_restore(|_, _, anim| {
                found_id = Some(anim.test_genie_element_id());
                found_target = anim.test_output_local_target();
            });
        (
            found_id.expect("active minimize Genie"),
            found_target.expect("minimize Genie has a dock target"),
        )
    };
    assert_eq!(
        target_a,
        Rectangle::new(Point::from((20., 1040.)), Size::from((48., 48.))),
        "Genie targets the initial dock rect A"
    );

    // Mid-animation the dock reflows and reports a new rect B.
    set_time(f.niri(), Duration::from_millis(250));
    f.niri().advance_animations();
    f.client(id)
        .foreign_toplevel(0)
        .set_rectangle(&dock, 200, 40, 48, 48); // B = (200, 1040)
    f.double_roundtrip(id);

    let (id_after, target_b) = {
        let mut found_id = None;
        let mut found_target = None;
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .scrolling()
            .test_for_each_minimize_restore(|_, _, anim| {
                found_id = Some(anim.test_genie_element_id());
                found_target = anim.test_output_local_target();
            });
        (
            found_id.expect("active minimize Genie still running"),
            found_target.expect("minimize Genie keeps its dock target"),
        )
    };
    assert_eq!(
        target_b,
        target_a,
        "minimize Genie locks its takeoff dock rect A; mid-flight set_rectangle does not retarget it"
    );
    assert_eq!(id_after, id_before, "entry is unchanged (no recreate)");
}

/// T-20: a `set_rectangle` with no active animation retargets nothing — no
/// controller entry is created, and the (already-dropped) Genie is untouched.
#[test]
fn r16_genie_retarget_noop_without_active_animation() {
    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = create_window(&mut f, id, 400, 300);
    let dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    // Let the minimize animation finish and its entry drop.
    set_time(f.niri(), Duration::from_millis(2000));
    f.niri_complete_animations();

    let mut count = 0;
    f.niri()
        .layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .test_for_each_minimize_restore(|_, _, _| count += 1);
    assert_eq!(count, 0, "minimize animation must complete and drop its entry");

    // A late set_rectangle finds no active animation to retarget (no-op, no panic).
    f.client(id)
        .foreign_toplevel(0)
        .set_rectangle(&dock, 200, 40, 48, 48);
    f.double_roundtrip(id);

    let mut count = 0;
    f.niri()
        .layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .test_for_each_minimize_restore(|_, _, _| count += 1);
    assert_eq!(count, 0, "retarget must not create an entry");
}

/// T-20: the dock endpoint is shared by both directions, so a mid-restore
/// `set_rectangle` retargets the restore Genie too (same in-place path).
#[test]
fn r16_genie_retarget_updates_active_restore_target() {
    use crate::layout::lifecycle_controller::LifecycleAnimDirection;

    let mut f = Fixture::with_config(linear_lifecycle_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = create_window(&mut f, id, 400, 300);
    let dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48); // A = (20, 1040)

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    f.client(id).foreign_toplevel(0).set_minimized();
    f.double_roundtrip(id);

    // Reverse mid-minimize into restore (reuses the same entry/texture); the
    // dock endpoint becomes the restore Genie's source.
    set_time(f.niri(), Duration::from_millis(150));
    f.niri().advance_animations();
    f.client(id).foreign_toplevel(0).unset_minimized();
    f.double_roundtrip(id);

    let id_before = {
        let mut found = None;
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .scrolling()
            .test_for_each_minimize_restore(|_, dir, anim| {
                assert_eq!(dir, LifecycleAnimDirection::Restore, "reverse → restore");
                found = Some(anim.test_genie_element_id());
            });
        found.expect("active restore Genie")
    };

    // Mid-restore, the dock reflows to B.
    set_time(f.niri(), Duration::from_millis(300));
    f.niri().advance_animations();
    f.client(id)
        .foreign_toplevel(0)
        .set_rectangle(&dock, 200, 40, 48, 48); // B = (200, 1040)
    f.double_roundtrip(id);

    let (id_after, target_b) = {
        let mut found_id = None;
        let mut found_target = None;
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .scrolling()
            .test_for_each_minimize_restore(|_, _, anim| {
                found_id = Some(anim.test_genie_element_id());
                found_target = anim.test_output_local_target();
            });
        (
            found_id.expect("retargeted restore Genie still active"),
            found_target.expect("retargeted restore Genie keeps a dock target"),
        )
    };
    assert_eq!(
        target_b,
        Rectangle::new(Point::from((200., 1040.)), Size::from((48., 48.))),
        "restore Genie retargets to the new dock rect B"
    );
    assert_eq!(
        id_after, id_before,
        "restore retarget is in-place, not a recreate"
    );
}
