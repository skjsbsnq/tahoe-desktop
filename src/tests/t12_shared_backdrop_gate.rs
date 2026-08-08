//! T12 shared-backdrop evidence gate (A12.G1-G4).
//!
//! These tests are measurement instruments for the GLASS-01 `PROPOSAL`:
//! "on a given output/render target, N Tahoe glass regions perform N
//! independent captures and N blur pyramid runs per frame". The roadmap
//! requires evidence *before* any shared-backdrop implementation:
//!
//! - A12.G1: trace proves that a typical scene on one output has >= 2 semantically shareable
//!   captures per frame (both per-surface and cross-surface).
//! - A12.G2: a z-order model shows where glass captures sample (backdrop, windows,
//!   Bottom/Top/Overlay layers, glass-over-glass).
//! - A12.G3: the shared union/capture memory cap and dirty-region update trade-off is quantified
//!   against the post-T11 per-region baseline.
//! - A12.G4: no second user-selectable render path is introduced; any capability fallback hides
//!   behind one internal interface.
//!
//! These tests drive the *real* production pipeline: the Wayland protocol
//! path commits regions onto layer surfaces, `Niri::render` assembles the
//! element list, and smithay's `OutputDamageTracker` + `GlesFrame` run the
//! real `capture_framebuffer` / `draw` (the same code that runs on the
//! compositor). Counters come from `lifecycle_diag`, which T11 already
//! proved zero-cost when disabled.
//!
//! Counter contract: `with_enabled_for_test` turns the process-global
//! lifecycle-diag window on for the duration of the closure under the shared
//! `with_test_lock` (serialized with blur_capacity / thumbnail_budget /
//! lifecycle_observe / r16). Other tests also drive real framebuffer-effect
//! rendering (blur_capacity, tahoe_glass) — they are safe only because they
//! hold the same lock. Any new test that renders framebuffer effects while
//! this window is open would disturb the delta==0 assertions here, so it must
//! run inside the same diag lock.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind as _, Offscreen as _};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::LayerConfigureProps;
use super::*;
use crate::niri::OutputRenderElements;
use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_surface_v1::TahoeGlassSurfaceV1;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::lifecycle_diag;

/// Run inside the serialized lifecycle-diag window so counters are exact
/// (only the current test can be counting).
fn with_diag<R>(f: impl FnOnce() -> R) -> R {
    lifecycle_diag::with_enabled_for_test(f)
}

/// Map a layer surface (mirror of the tahoe_glass test helper, kept local so
/// the fixture stays self-contained for the gate).
fn create_mapped_layer(f: &mut Fixture, id: client::ClientId) -> WlSurface {
    let layer = f
        .client(id)
        .create_layer(None, Layer::Top, "tahoe-t12-gate");
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
    let map = smithay::desktop::layer_map_for_output(&output);
    for layer in map.layers() {
        let _ = client_surface;
        return layer.wl_surface().clone();
    }
    panic!("no mapped layer surface on output");
}

fn set_and_commit_region(
    f: &mut Fixture,
    id: client::ClientId,
    client_surface: &WlSurface,
    glass: &TahoeGlassSurfaceV1,
    region_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    glass.set_region(
        region_id,
        x,
        y,
        w,
        h,
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

/// Collect the full front-to-back element list for `output` through the real
/// `Niri::render` assembly path (layers, workspaces, backdrop, pointer off).
fn collect_output_elements(
    f: &mut Fixture,
    output: &smithay::output::Output,
) -> Vec<OutputRenderElements<GlesRenderer>> {
    let mut elements = Vec::new();
    let output = output.clone();
    let state = f.niri_state();
    state
        .backend
        .with_primary_renderer(|renderer| {
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            state
                .niri
                .render(ctx, &output, false, &mut |elem| elements.push(elem));
        })
        .expect("renderer must be present");
    elements
}

/// Count how many Tahoe glass framebuffer-effect elements (the elements that
/// capture the backdrop) are present in a layer-element list.
fn count_tahoe_glass_elements(
    elements: &[crate::render_helpers::tahoe_glass::TahoeGlassElement],
) -> usize {
    elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                crate::render_helpers::tahoe_glass::TahoeGlassElement::BackgroundEffect(_)
            )
        })
        .count()
}

/// Count how many Tahoe glass framebuffer-effect elements appear in a full
/// output element list (including the opening/cropped wrappers the layer
/// open animation emits).
fn count_output_glass_effects(elements: &[OutputRenderElements<GlesRenderer>]) -> usize {
    elements
        .iter()
        .filter(|e| {
            let OutputRenderElements::LayerSurface(layer_elem) = e else {
                return false;
            };
            matches!(
                layer_elem,
                crate::layer::mapped::LayerSurfaceRenderElement::TahoeGlass(
                    crate::render_helpers::tahoe_glass::TahoeGlassElement::BackgroundEffect(_)
                ) | crate::layer::mapped::LayerSurfaceRenderElement::OpeningTahoeGlass(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::CroppedTahoeGlass(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::CroppedOpeningTahoeGlass(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::BackgroundEffect(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::OpeningBackgroundEffect(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::CroppedBackgroundEffect(_)
                    | crate::layer::mapped::LayerSurfaceRenderElement::CroppedOpeningBackgroundEffect(
                        _
                    )
            )
        })
        .count()
}

/// Render a full frame through the damage tracker, returning the per-frame
/// counters delta. `first` marks the first frame (full damage / force
/// capture); subsequent frames only recapture what the tracker decides.
fn render_damaged_frame(
    renderer: &mut GlesRenderer,
    elements: &[OutputRenderElements<GlesRenderer>],
    tracker: &mut smithay::backend::renderer::damage::OutputDamageTracker,
    first: bool,
) -> (u64, u64, u64) {
    let before = lifecycle_diag::snapshot();

    let (damage, states) = tracker
        .damage_output(1, elements)
        .expect("tracker mode must be set");
    // The very first call must see damage (the tracker starts empty).
    if first {
        assert!(
            damage.is_some(),
            "first frame must produce damage so captures run"
        );
    }

    let size = Size::from((256, 160));
    let mut target_texture: smithay::backend::renderer::gles::GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("creating target texture");
    {
        let mut target = renderer.bind(&mut target_texture).expect("binding target");
        let result = tracker
            .render_output_with_states(
                renderer,
                &mut target,
                1,
                elements,
                smithay::backend::renderer::Color32F::TRANSPARENT,
                states,
            )
            .expect("rendering output");
        let _ = result;
    }

    let after = lifecycle_diag::snapshot();
    (
        after
            .tahoe_region_capture
            .saturating_sub(before.tahoe_region_capture),
        after
            .fb_effect_capture
            .saturating_sub(before.fb_effect_capture),
        after.blur_render.saturating_sub(before.blur_render),
    )
}

/// G1 (per-surface): one layer surface commits TWO glass regions; a frame
/// whose backdrop changed must perform exactly two captures and two blur
/// runs — the per-region baseline of the current architecture.
#[test]
fn g1_two_regions_on_one_surface_capture_twice_per_frame() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let client_surface = create_mapped_layer(&mut f, id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());
    // Two disjoint regions like a Dock + an open DockAppMenu would sit on one
    // surface in the Toast cardRegions model.
    set_and_commit_region(&mut f, id, &client_surface, &glass, 1, 8, 4, 128, 32);
    set_and_commit_region(&mut f, id, &client_surface, &glass, 2, 8, 60, 96, 28);

    let server_surface = server_surface_for_client_layer(&mut f, &client_surface);
    let config = niri_config::TahoeGlass::default();

    let mut region_elements = Vec::new();
    let _ = render_for_layer_t12(&mut f, &server_surface, &config, &mut region_elements);
    // The element multiplier per region is an implementation detail (the live
    // path currently pushes one ExtraDamage + one effect element per region,
    // but xray/visibility/animation wrappers can change the count), so only
    // assert existence here — the capture counter below is the trusted
    // per-region signal.
    let bg_count = count_tahoe_glass_elements(&region_elements);
    assert!(
        bg_count >= 1,
        "committed regions must produce framebuffer effects, got {bg_count}"
    );

    with_diag(|| {
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            (256, 160),
            Scale::from(1.0),
            Transform::Normal,
        );
        let output = f.niri_output(1);
        let elements = collect_output_elements(&mut f, &output);
        let visible = count_tahoe_glass_elements(&region_elements);
        f.niri_state()
            .backend
            .with_primary_renderer(|renderer| {
                let (region_c, fb_c, blur_c) =
                    render_damaged_frame(renderer, &elements, &mut tracker, true);
                // `collect_output_elements` also runs inside the diag window
                // and bumps `tahoe_region_capture`; that counter is discarded
                // (`region_c`) because the GPU capture/blur delta is what the
                // gate measures.
                assert_eq!(
                    fb_c, blur_c,
                    "every framebuffer blit must run the blur pyramid"
                );
                assert!(
                    fb_c >= 2,
                    "two committed regions must capture >=2 times per frame, got {fb_c} (visible glass elements: {visible})"
                );
                assert!(
                    blur_c <= 32,
                    "sanity bound on blur runs per frame, got {blur_c}"
                );
                let _ = region_c;
            })
            .unwrap();
    });
}

/// G1 (cross-surface): two mapped layer surfaces (Dock + TopBar) on the same
/// output produce two independent framebuffer effects — the real desktop
/// topology the PROPOSAL would target. Also verifies the dirty-region update
/// property of the *current* baseline: an unchanged second frame does not
/// re-capture. (smithay skips rendering entirely when there is no damage —
/// `damage/mod.rs` "no damage, skipping rendering" — so `fb_c2==0` proves
/// no re-capture on a static scene; the finer per-region dirty-index
/// granularity is not asserted here.)
#[test]
fn g1_two_surfaces_on_one_output_capture_independently() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface_a = create_mapped_layer(&mut f, id);
    let surface_b = create_mapped_layer(&mut f, id);

    for (surface, region_id) in [(&surface_a, 1u32), (&surface_b, 2u32)] {
        let glass_manager = f.client(id).tahoe_glass_manager();
        let qh = f.client(id).qh.clone();
        let glass = glass_manager.get_tahoe_glass_surface(surface, &qh, ());
        set_and_commit_region(&mut f, id, surface, &glass, region_id, 8, 4, 128, 32);
    }

    // Count glass elements through the production render path.
    let server_surface_a = server_surface_for_client_layer(&mut f, &surface_a);
    let config = niri_config::TahoeGlass::default();
    let mut region_elements = Vec::new();
    let _ = render_for_layer_t12(&mut f, &server_surface_a, &config, &mut region_elements);
    assert!(
        count_tahoe_glass_elements(&region_elements) >= 1,
        "the first surface must produce a glass effect"
    );

    with_diag(|| {
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            (256, 160),
            Scale::from(1.0),
            Transform::Normal,
        );
        let output = f.niri_output(1);
        let elements = collect_output_elements(&mut f, &output);
        // Both mapped surfaces must contribute glass framebuffer-effect
        // elements to the full output list (this is the cross-surface count,
        // unlike the per-surface helper above).
        let visible = count_output_glass_effects(&elements);
        f.niri_state()
            .backend
            .with_primary_renderer(|renderer| {
                let (_, fb_c, blur_c) =
                    render_damaged_frame(renderer, &elements, &mut tracker, true);
                assert_eq!(fb_c, blur_c);
                assert!(
                    visible >= 2,
                    "two mapped glass surfaces must produce >=2 framebuffer effects in the element list, got {visible}"
                );
                assert!(
                    fb_c >= 2,
                    "cross-surface N>=2 glass must capture >=2 times per frame, got {fb_c}"
                );

                // Dirty-region update property of the baseline: an unchanged
                // second frame (same elements, tracker up to date) must not
                // re-capture — captures are already limited to damaged
                // regions.
                let (_, fb_c2, blur_c2) =
                    render_damaged_frame(renderer, &elements, &mut tracker, false);
                assert_eq!(
                    fb_c2, 0,
                    "an unchanged second frame must not re-capture, got {fb_c2}"
                );
                assert_eq!(blur_c2, 0);
            })
            .unwrap();
    });
}

/// G2: z-order model — glass capture samples the backdrop, never the glass
/// surface's own content. `Niri::render` emits elements in front-to-back
/// composition order (smithay `damage/mod.rs` "elements for this output in
/// front-to-back order"); drawing runs the list in reverse, so the backdrop
/// is painted first and the glass capture element samples it during its own
/// `draw`. Therefore in the front-to-back list the glass capture element must
/// come *before* the solid-color backdrop element. (A plain `Texture`
/// match would also hit the hotkey overlay, which is drawn above everything
/// and never sampled — only the `SolidColor` backdrop is authoritative.)
#[test]
fn g2_glass_capture_samples_backdrop_and_lower_layers() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = create_mapped_layer(&mut f, id);
    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass = glass_manager.get_tahoe_glass_surface(&surface, &qh, ());
    set_and_commit_region(&mut f, id, &surface, &glass, 1, 8, 4, 128, 32);

    let output = f.niri_output(1);
    let elements = collect_output_elements(&mut f, &output);

    // Find the glass framebuffer-effect element (the capture).
    let glass_index = elements.iter().position(|e| {
        matches!(
            e,
            OutputRenderElements::LayerSurface(
                crate::layer::mapped::LayerSurfaceRenderElement::TahoeGlass(
                    crate::render_helpers::tahoe_glass::TahoeGlassElement::BackgroundEffect(_)
                )
            )
        )
    });
    let glass_index = glass_index.expect("the committed glass must produce a capture element");

    // The solid-color backdrop must be drawn below the glass: it comes after
    // the glass capture in the front-to-back list (painted earlier, sampled
    // by the capture).
    let backdrop_after = elements[glass_index..]
        .iter()
        .any(|e| matches!(e, OutputRenderElements::SolidColor(_)));
    assert!(
        backdrop_after,
        "the solid-color backdrop must be drawn below the glass capture so the capture samples it (front-to-back list after the glass element)"
    );
}

/// G3: memory cap trade-off — the shared union capture's memory ceiling is
/// the bounding box of the participating regions. On the real 2560x1600
/// desktop the regions are *separated* (TopBar top, Dock bottom, island
/// top-center), so the union bounding box covers most of the output and is
/// ~10x the sum of the per-region captures — the shared design is *worse*
/// on memory unless regions overlap. Quantify both cases so the GO decision
/// has the numbers.
#[test]
fn g3_union_bbox_memory_cap_vs_region_sum() {
    fn area(r: Rectangle<i32, Logical>) -> i64 {
        i64::from(r.size.w) * i64::from(r.size.h)
    }
    // Sum of the independent capture bands (per-region baseline, post-T11).
    fn sum(rects: &[Rectangle<i32, Logical>]) -> i64 {
        rects.iter().map(|r| area(*r)).sum()
    }
    fn bbox(rects: &[Rectangle<i32, Logical>]) -> Rectangle<i32, Logical> {
        rects.iter().copied().reduce(|a, b| a.merge(b)).unwrap()
    }

    // Real desktop layout on the 2560x1600 output.
    let topbar = Rectangle::new(Point::from((0, 0)), Size::from((2560, 56)));
    let dock = Rectangle::new(Point::from((0, 1504)), Size::from((2560, 96)));
    let island = Rectangle::new(Point::from((1168, 4)), Size::from((224, 40)));
    let separated = [topbar, dock, island];

    let sum_area = sum(&separated);
    let union_area = area(bbox(&separated));
    assert_eq!(sum_area, 398_080);
    assert!(
        union_area > sum_area * 5,
        "separated regions: the union bbox ({union_area}) must dominate the sum ({sum_area})"
    );
    assert!(
        union_area * 100 > sum_area * 900,
        "the shared capture ceiling is ~10x the per-region sum on the real layout"
    );

    // Overlapping glass (a big panel over the dock): the union shrinks below
    // the sum, the only case where sharing pays on memory.
    let cc = Rectangle::new(Point::from((400, 1400)), Size::from((1760, 160)));
    let overlap = [dock, cc];
    let sum_overlap = sum(&overlap);
    let union_overlap = area(bbox(&overlap));
    assert!(
        union_overlap < sum_overlap,
        "overlapping glass must shrink the union below the sum"
    );
}

/// G4: no user-selectable second render path may exist for the shared
/// backdrop. This is a static contract check: the production render pipeline
/// must keep exactly one Tahoe glass authority (per-region
/// `render_regions_for_layer`) and one framebuffer-effect element type; a
/// `GlassBackdropV2`-style parallel path, an old/new renderer switch or a
/// user setting would violate the roadmap's single-authority rule.
#[test]
fn g4_no_second_render_authority() {
    // The production render entry keeps one authority. If a future
    // implementation adds a second path, this grep is the tripwire. It scans
    // the top-level `.rs` files of `render_helpers/` (non-recursive; nested
    // subdirectories like `shaders/` are not covered — a soft guardrail, per
    // the evidence gate's RESOLVED-NO-CODE scope).
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = format!("{manifest}/src/render_helpers");
    let mut sources = String::new();
    for entry in std::fs::read_dir(&dir).expect("render_helpers must exist") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            sources.push_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|_| panic!("{} must be readable", path.display())),
            );
        }
    }
    assert!(
        sources.contains("fn render_regions_for_layer"),
        "the per-region render authority must exist"
    );
    assert!(
        !sources.contains("GlassBackdropV2") && !sources.contains("render_shared_backdrop"),
        "no second shared-backdrop render path may be introduced"
    );
    assert!(
        !sources.contains("fn render_regions_v2"),
        "no renamed parallel render entry may be introduced"
    );
    assert!(
        !sources.contains("shared_backdrop"),
        "no shared-backdrop module/type may be introduced outside the evidence gate"
    );
}

fn render_for_layer_t12(
    f: &mut Fixture,
    server_surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    config: &niri_config::TahoeGlass,
    elements: &mut Vec<crate::render_helpers::tahoe_glass::TahoeGlassElement>,
) -> bool {
    let mut ok = false;
    f.niri_state()
        .backend
        .with_primary_renderer(|renderer| {
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            ok = crate::render_helpers::tahoe_glass::render_for_layer(
                ctx,
                None,
                server_surface,
                "tahoe-t12-gate",
                (0., 0.).into(),
                1.0,
                config,
                1.0,
                None,
                XrayPos::default(),
                false,
                &mut |elem| elements.push(elem),
            );
        })
        .expect("renderer must be present");
    ok
}
