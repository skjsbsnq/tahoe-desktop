use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use niri_config::{TahoeGlass, TahoeGlassMaterial};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
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
    xray_pos: XrayPos,
    regions: Arc<Vec<TahoeGlassRegion>>,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    if !config.namespace_allowed(namespace) {
        return false;
    }

    with_states(surface, |states| {
        if regions.is_empty() {
            return false;
        }

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
    xray_pos: XrayPos,
    push: &mut dyn FnMut(TahoeGlassElement),
) {
    let rect = region.rect.to_f64();
    let geometry = Rectangle::new(surface_location + rect.loc, rect.size);
    let material_alpha = region.material_alpha.clamp(0., 1.) * layer_alpha.clamp(0., 1.);

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
    let sample_geometry = expand_rect(geometry, sample_padding);

    renderer.background_effect.update_config(blur_config);
    renderer
        .background_effect
        .update_render_elements(region.radius, effect, region.flags.blur);

    if !renderer.background_effect.is_visible() {
        return;
    }

    let params = RenderParams {
        geometry: sample_geometry,
        alpha: material_alpha,
        subregion: None,
        clip: region.flags.clip.then_some((geometry, region.radius)),
        scale,
    };
    let xray_pos = xray_pos.offset(rect.loc - Point::from((sample_padding, sample_padding)));
    renderer
        .background_effect
        .render(ctx.r(), ns, params, xray_pos, &mut |elem| push(elem.into()));
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

fn expand_rect(rect: Rectangle<f64, Logical>, padding: f64) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::new(rect.loc.x - padding, rect.loc.y - padding),
        Size::new(rect.size.w + padding * 2.0, rect.size.h + padding * 2.0),
    )
}
