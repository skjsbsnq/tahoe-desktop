use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use niri_config::{CornerRadius, TahoeGlass, TahoeGlassMaterial};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
use smithay::wayland::compositor::{with_states, SurfaceData};

use crate::layout::shadow::Shadow;
use crate::niri_render_elements;
use crate::protocols::tahoe_glass::{get_committed_regions, TahoeGlassRegion};
use crate::render_helpers::background_effect::{
    BackgroundEffect, BackgroundEffectElement, RenderParams,
};
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;

struct SurfaceTahoeGlassRenderer(Mutex<TahoeGlassRenderer>);

struct TahoeGlassRenderer {
    damage: ExtraDamage,
    damaged_regions: Vec<Rectangle<i32, Logical>>,
    regions: HashMap<u32, TahoeGlassRegionRenderer>,
}

struct TahoeGlassRegionRenderer {
    background_effect: BackgroundEffect,
    shadow: Shadow,
}

niri_render_elements! {
    TahoeGlassElement => {
        BackgroundEffect = BackgroundEffectElement,
        Shadow = ShadowRenderElement,
        ExtraDamage = ExtraDamage,
    }
}

impl SurfaceTahoeGlassRenderer {
    fn get(states: &SurfaceData) -> &Self {
        states
            .data_map
            .get_or_insert(|| SurfaceTahoeGlassRenderer(Mutex::new(TahoeGlassRenderer::new())))
    }
}

impl TahoeGlassRenderer {
    fn new() -> Self {
        Self {
            damage: ExtraDamage::new(),
            damaged_regions: Vec::new(),
            regions: HashMap::new(),
        }
    }

    fn damage(&mut self) {
        self.damage.damage_all();
        for region in self.regions.values_mut() {
            region.background_effect.damage();
            region.shadow.update_shaders();
        }
    }

    fn damage_regions(&mut self, old: &[TahoeGlassRegion], new: &[TahoeGlassRegion]) {
        self.damage.damage_all();
        self.damaged_regions
            .extend(old.iter().chain(new).map(|region| region.rect));
    }
}

impl TahoeGlassRegionRenderer {
    fn new(material: TahoeGlassMaterial) -> Self {
        Self {
            background_effect: BackgroundEffect::new(),
            shadow: Shadow::new(material.shadow),
        }
    }
}

pub fn damage_surface(states: &SurfaceData) {
    if let Some(renderer) = states.data_map.get::<SurfaceTahoeGlassRenderer>() {
        renderer.0.lock().unwrap().damage();
    }
}

pub fn damage_surface_regions(
    states: &SurfaceData,
    old: &[TahoeGlassRegion],
    new: &[TahoeGlassRegion],
) {
    if let Some(renderer) = states.data_map.get::<SurfaceTahoeGlassRenderer>() {
        renderer.0.lock().unwrap().damage_regions(old, new);
    }
}

pub fn surface_has_regions(surface: &WlSurface) -> bool {
    with_states(surface, |states| !get_committed_regions(states).is_empty())
}

#[allow(clippy::too_many_arguments)]
pub fn render_for_layer(
    mut ctx: RenderCtx<GlesRenderer>,
    ns: Option<usize>,
    surface: &WlSurface,
    namespace: &str,
    location: Point<f64, Logical>,
    scale: f64,
    blur_config: niri_config::Blur,
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    let regions = with_states(surface, get_committed_regions);
    render_regions_for_layer(
        ctx.r(),
        ns,
        surface,
        namespace,
        location,
        scale,
        blur_config,
        config,
        layer_alpha,
        draw_clip,
        xray_pos,
        regions,
        push,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_frozen_regions_for_layer(
    ctx: RenderCtx<GlesRenderer>,
    ns: Option<usize>,
    surface: &WlSurface,
    namespace: &str,
    location: Point<f64, Logical>,
    scale: f64,
    blur_config: niri_config::Blur,
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    regions: Arc<Vec<TahoeGlassRegion>>,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    render_regions_for_layer(
        ctx,
        ns,
        surface,
        namespace,
        location,
        scale,
        blur_config,
        config,
        layer_alpha,
        draw_clip,
        xray_pos,
        regions,
        push,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_regions_for_layer(
    mut ctx: RenderCtx<GlesRenderer>,
    ns: Option<usize>,
    surface: &WlSurface,
    namespace: &str,
    location: Point<f64, Logical>,
    scale: f64,
    blur_config: niri_config::Blur,
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    regions: Arc<Vec<TahoeGlassRegion>>,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    let _span = tracy_client::span!("TahoeGlass::render_regions_for_layer");

    if !config.namespace_allowed(namespace) {
        return false;
    }

    with_states(surface, |states| {
        if regions.is_empty() {
            return false;
        }

        let region_count = regions.len();
        let total_area: i64 = regions.iter().map(region_area).sum();
        trace!(
            namespace,
            region_count,
            total_area,
            "rendering Tahoe glass regions"
        );

        let renderer = SurfaceTahoeGlassRenderer::get(states);
        let mut renderer = renderer.0.lock().unwrap();
        let damage = std::mem::take(&mut renderer.damaged_regions);
        for rect in damage {
            let rect = rect.to_f64();
            let geometry = Rectangle::new(location + rect.loc, rect.size);
            push(renderer.damage.render(geometry).into());
        }

        renderer
            .regions
            .retain(|id, _| regions.iter().any(|region| region.id == *id));

        for region in regions.iter() {
            let material = config.material(&region.material);
            let region_renderer = renderer
                .regions
                .entry(region.id)
                .or_insert_with(|| TahoeGlassRegionRenderer::new(material));

            render_region(
                ctx.r(),
                ns,
                region,
                region_renderer,
                material,
                location,
                scale,
                blur_config,
                layer_alpha,
                draw_clip,
                xray_pos,
                push,
            );
        }

        true
    })
}

#[allow(clippy::too_many_arguments)]
fn render_region(
    mut ctx: RenderCtx<GlesRenderer>,
    ns: Option<usize>,
    region: &TahoeGlassRegion,
    renderer: &mut TahoeGlassRegionRenderer,
    material: TahoeGlassMaterial,
    surface_location: Point<f64, Logical>,
    scale: f64,
    blur_config: niri_config::Blur,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    push: &mut dyn FnMut(TahoeGlassElement),
) {
    let _span = tracy_client::span!("TahoeGlass::render_region");

    let rect = region.rect.to_f64();
    let geometry = Rectangle::new(surface_location + rect.loc, rect.size);
    let material_alpha = region.material_alpha.clamp(0., 1.) * layer_alpha.clamp(0., 1.);

    let mut effect = material.background_effect;
    if !region.flags.blur {
        effect.blur = Some(false);
    }

    // Compositor-side material easing: `material_alpha` fades the material in
    // and out for popup/backdrop enter/exit without touching region geometry.
    // `interaction` then boosts the refractive terms for hover/press/active states.
    let fade = |v: Option<f64>| v.map(|x| x * f64::from(material_alpha));
    let fade_from_one = |v: Option<f64>| v.map(|x| 1.0 + (x - 1.0) * f64::from(material_alpha));
    effect.tint_amount = fade(effect.tint_amount);
    effect.contrast = fade_from_one(effect.contrast);
    effect.edge_highlight = fade(effect.edge_highlight);
    effect.refraction = fade(effect.refraction);
    effect.inner_shadow = fade(effect.inner_shadow);
    effect.chromatic = fade(effect.chromatic);
    effect.lens_depth = fade(effect.lens_depth);

    let interaction = region.interaction as f64;
    if interaction > 0.0 && material_alpha > 0.0 {
        let boost = |v: Option<f64>| v.map(|x| x * (1.0 + interaction));
        let boost_from_one = |v: Option<f64>| v.map(|x| 1.0 + (x - 1.0) * (1.0 + interaction));
        effect.contrast = boost_from_one(effect.contrast);
        effect.edge_highlight = boost(effect.edge_highlight);
        effect.refraction = boost(effect.refraction);
        effect.inner_shadow = boost(effect.inner_shadow);
        effect.chromatic = boost(effect.chromatic);
        effect.lens_depth = boost(effect.lens_depth);
    }

    let sample_padding = glass_sample_padding(region, effect, blur_config);
    // Capture/sample geometry may expand beyond the protocol region so blur
    // and refraction have enough context. Draw/visible geometry must stay
    // exactly on the protocol region — sample padding must never become a
    // visible halo. The `clip` flag only selects rounded vs rectangular
    // corner semantics inside that visible bound.
    let sample_geometry = expand_rect(geometry, sample_padding);
    let params = glass_region_render_params(
        geometry,
        sample_geometry,
        region.flags.clip,
        region.radius,
        material_alpha,
        scale,
        draw_clip,
    );
    let visible_radius = params
        .clip
        .as_ref()
        .map(|(_, radius)| *radius)
        .unwrap_or(region.radius);
    trace!(
        material = %region.material,
        area = region_area(region),
        sample_padding,
        blur = region.flags.blur,
        clip = region.flags.clip,
        material_alpha,
        "rendering Tahoe glass region"
    );

    renderer.background_effect.update_config(blur_config);
    // Corner radius stored on the effect is what render() writes into clip;
    // keep it consistent with the visible clip decision above.
    renderer
        .background_effect
        .update_render_elements(visible_radius, effect, region.flags.blur);

    if renderer.background_effect.is_visible() {
        let xray_pos = xray_pos.offset(rect.loc - Point::from((sample_padding, sample_padding)));
        renderer
            .background_effect
            .render(ctx.r(), ns, params, xray_pos, &mut |elem| push(elem.into()));
    }

    // niri collects render elements front-to-back. Tahoe glass regions are
    // below the QML layer surface, but inside the region the material must sit
    // above its drop shadow; otherwise the shadow is composited over the glass
    // and shows up as dark rounded-corner artifacts on transparent panels.
    if region.flags.shadow && material_alpha > 0. {
        renderer.shadow.update_config(material.shadow);
        renderer.shadow.update_render_elements(
            geometry.size,
            true,
            region.radius,
            scale,
            material_alpha,
        );
        renderer
            .shadow
            .render(ctx.renderer, geometry.loc, &mut |elem| push(elem.into()));
    }
}

fn region_area(region: &TahoeGlassRegion) -> i64 {
    i64::from(region.rect.size.w.max(0)) * i64::from(region.rect.size.h.max(0))
}

fn glass_sample_padding(
    region: &TahoeGlassRegion,
    effect: niri_config::BackgroundEffect,
    blur_config: niri_config::Blur,
) -> f64 {
    let mut padding: f64 = 2.0;

    if region.flags.blur && !blur_config.off {
        let passes = f64::from(blur_config.passes.clamp(1, 31));
        padding = padding.max(blur_config.offset * passes);
    }

    let refraction = effect.refraction.unwrap_or(0.).abs();
    let lens_depth = effect.lens_depth.unwrap_or(0.).abs();
    if refraction > 0.0 || lens_depth > 0.0 {
        let short_edge = f64::from(region.rect.size.w.min(region.rect.size.h).max(1));
        padding = padding.max((refraction + lens_depth) * short_edge * 2.0 + 4.0);
    }

    padding.clamp(2.0, 64.0)
}

/// Build `RenderParams` that keep sample and visible geometry separated.
///
/// - `sample_geometry` expands capture for blur/refraction padding.
/// - Draw clip is **always** `Some(visible_geometry, …)`. Omitting clip lets
///   `FramebufferEffect` / xray fall back to `params.geometry` and paints the
///   padding as a visible halo when `clip=false`.
/// - The protocol `clip` flag only chooses rounded (`radius`) vs rectangular
///   (`CornerRadius::default`) material inside the visible bound — it must not
///   opt out of clipping sample padding away from the drawable region.
fn glass_region_render_params(
    visible_geometry: Rectangle<f64, Logical>,
    sample_geometry: Rectangle<f64, Logical>,
    clip: bool,
    radius: CornerRadius,
    alpha: f32,
    scale: f64,
    draw_clip: Option<Rectangle<i32, Physical>>,
) -> RenderParams {
    let corner = if clip {
        radius
    } else {
        CornerRadius::default()
    };
    RenderParams {
        geometry: sample_geometry,
        alpha,
        subregion: None,
        clip: Some((visible_geometry, corner)),
        scale,
        draw_clip,
    }
}

fn expand_rect(rect: Rectangle<f64, Logical>, padding: f64) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::new(rect.loc.x - padding, rect.loc.y - padding),
        Size::new(rect.size.w + padding * 2.0, rect.size.h + padding * 2.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Size};

    fn visible_rect() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((100.0, 50.0)), Size::from((200.0, 80.0)))
    }

    fn rounded_radius() -> CornerRadius {
        CornerRadius {
            top_left: 12.0,
            top_right: 12.0,
            bottom_right: 12.0,
            bottom_left: 12.0,
        }
    }

    /// Old broken wiring: `clip` only when the protocol flag is set. When the
    /// flag is false this yields `clip=None`, and FramebufferEffect falls back
    /// to sample geometry — the Task 16 halo regression.
    fn legacy_then_some_params(
        visible: Rectangle<f64, Logical>,
        sample: Rectangle<f64, Logical>,
        clip: bool,
        radius: CornerRadius,
        alpha: f32,
        scale: f64,
        draw_clip: Option<Rectangle<i32, Physical>>,
    ) -> RenderParams {
        RenderParams {
            geometry: sample,
            alpha,
            subregion: None,
            clip: clip.then_some((visible, radius)),
            scale,
            draw_clip,
        }
    }

    #[test]
    fn sample_geometry_expands_by_padding_while_visible_stays_put() {
        let visible = visible_rect();
        let padding = 24.0;
        let sample = expand_rect(visible, padding);

        assert_eq!(
            sample,
            Rectangle::new(Point::from((76.0, 26.0)), Size::from((248.0, 128.0)))
        );
        // Visible geometry is never expanded — padding is sample-only.
        assert_eq!(visible.size, Size::from((200.0, 80.0)));
        assert!(sample.size.w > visible.size.w);
        assert!(sample.size.h > visible.size.h);
    }

    #[test]
    fn clip_false_params_always_clip_to_visible_not_sample() {
        // Regression: clip=false used to leave clip=None, so FramebufferEffect
        // fell back to sample_geometry and painted the blur/refraction padding
        // as a visible halo outside the protocol region.
        let visible = visible_rect();
        let radius = rounded_radius();
        let padding = 32.0;
        let sample = expand_rect(visible, padding);
        let draw_clip = Some(Rectangle::new(
            Point::from((90, 40)),
            Size::from((220, 100)),
        ));
        let params = glass_region_render_params(
            visible, sample, false, radius, 0.85, 1.25, draw_clip,
        );

        assert_eq!(params.geometry, sample, "capture stays on expanded sample");
        let (clip_geo, clip_radius) = params
            .clip
            .expect("clip must always be Some so padding is never drawn");
        assert_eq!(
            clip_geo, visible,
            "draw clip must stay on the protocol region"
        );
        assert_ne!(
            clip_geo, sample,
            "draw clip must not include sample padding"
        );
        assert_eq!(
            clip_radius,
            CornerRadius::default(),
            "clip=false is rectangular material, not rounded"
        );
        assert_eq!(params.alpha, 0.85);
        assert_eq!(params.scale, 1.25);
        assert_eq!(params.draw_clip, draw_clip);
        // Sample padding is still present for capture quality.
        assert!(sample.size.w - visible.size.w >= padding * 2.0 - f64::EPSILON);
    }

    #[test]
    fn clip_true_params_use_region_radius_on_visible_geometry() {
        let visible = visible_rect();
        let radius = CornerRadius {
            top_left: 16.0,
            top_right: 8.0,
            bottom_right: 4.0,
            bottom_left: 2.0,
        };
        let sample = expand_rect(visible, 16.0);
        let params =
            glass_region_render_params(visible, sample, true, radius, 1.0, 1.0, None);

        assert_eq!(params.geometry, sample);
        let (clip_geo, clip_radius) = params.clip.expect("clip always Some");
        assert_eq!(clip_geo, visible);
        assert_eq!(clip_radius, radius);
    }

    #[test]
    fn legacy_then_some_wiring_would_omit_clip_when_flag_false() {
        // Old call site contract that must stay red: only Some when flag true.
        let visible = visible_rect();
        let sample = expand_rect(visible, 24.0);
        let legacy = legacy_then_some_params(
            visible,
            sample,
            false,
            rounded_radius(),
            1.0,
            1.0,
            None,
        );
        assert!(
            legacy.clip.is_none(),
            "documents the pre-fix wiring that painted sample padding"
        );

        let fixed = glass_region_render_params(
            visible,
            sample,
            false,
            rounded_radius(),
            1.0,
            1.0,
            None,
        );
        assert!(
            fixed.clip.is_some(),
            "fixed wiring must always supply a visible clip"
        );
        assert_ne!(
            fixed.clip.as_ref().map(|(g, _)| *g),
            Some(sample),
            "fixed clip must not equal sample geometry"
        );
    }

    #[test]
    fn sample_padding_is_never_zeroed_for_blur_quality() {
        // Task forbids zeroing sample padding / disabling blur to hide the halo.
        // Minimum padding stays at least 2.0 even with blur off and no refraction.
        use crate::protocols::tahoe_glass::{TahoeGlassFlags, TahoeGlassRegion};

        let region = TahoeGlassRegion {
            id: 1,
            rect: Rectangle::new(Point::from((0, 0)), Size::from((100, 40))),
            radius: CornerRadius::default(),
            material: "panel".into(),
            flags: TahoeGlassFlags {
                blur: false,
                shadow: false,
                clip: false,
            },
            interaction: 0.0,
            material_alpha: 1.0,
        };
        let padding = glass_sample_padding(
            &region,
            niri_config::BackgroundEffect::default(),
            niri_config::Blur::default(),
        );
        assert!(
            padding >= 2.0,
            "sample padding must remain available for quality, got {padding}"
        );
        assert!(padding <= 64.0);
    }

    #[test]
    fn edge_region_sample_expands_outside_visible_bounds() {
        // A region sitting on the edge of a surface still samples outside its
        // visible rect; drawing must remain clipped to that rect.
        let visible = Rectangle::new(Point::from((0.0, 0.0)), Size::from((120.0, 32.0)));
        let padding = 16.0;
        let sample = expand_rect(visible, padding);
        let params =
            glass_region_render_params(visible, sample, false, CornerRadius::from(8.0), 1.0, 1.0, None);

        assert_eq!(params.geometry, sample);
        let (clip_geo, _) = params.clip.unwrap();
        assert_eq!(clip_geo.loc, Point::from((0.0, 0.0)));
        assert_eq!(clip_geo.size, Size::from((120.0, 32.0)));
        assert_eq!(sample.loc, Point::from((-16.0, -16.0)));
        assert_eq!(sample.size, Size::from((152.0, 64.0)));
    }

    #[test]
    fn edge_reveal_draw_clip_is_passed_through_unchanged() {
        let visible = visible_rect();
        let sample = expand_rect(visible, 20.0);
        let draw_clip = Some(Rectangle::new(
            Point::from((100, 100)),
            Size::from((200, 100)),
        ));
        let params =
            glass_region_render_params(visible, sample, true, rounded_radius(), 1.0, 2.0, draw_clip);
        assert_eq!(params.draw_clip, draw_clip);
        assert_eq!(params.scale, 2.0);
        assert!(params.clip.is_some());
    }
}
