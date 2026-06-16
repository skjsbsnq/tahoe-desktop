use std::collections::HashMap;
use std::sync::Mutex;

use niri_config::{TahoeGlass, TahoeGlassMaterial};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
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
    xray_pos: XrayPos,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    if !config.namespace_allowed(namespace) {
        return false;
    }

    with_states(surface, |states| {
        let regions = get_committed_regions(states);
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
    xray_pos: XrayPos,
    push: &mut dyn FnMut(TahoeGlassElement),
) {
    let rect = region.rect.to_f64();
    let geometry = Rectangle::new(surface_location + rect.loc, rect.size);

    if region.flags.shadow {
        renderer.shadow.update_config(material.shadow);
        renderer
            .shadow
            .update_render_elements(geometry.size, true, region.radius, scale, 1.);
        renderer
            .shadow
            .render(ctx.renderer, geometry.loc, &mut |elem| push(elem.into()));
    }

    let mut effect = material.background_effect;
    if !region.flags.blur {
        effect.blur = Some(false);
    }

    // Compositor-side material easing: a per-region interaction scalar in
    // [0, 1] pushes the glass beyond its rest values. We scale the
    // highlight/refraction/inner-shadow/lens params so hover/press/enter states
    // intensify the glass without any shader or region-geometry change.
    let interaction = region.interaction as f64;
    if interaction > 0.0 {
        let boost = |v: Option<f64>| v.map(|x| x * (1.0 + interaction));
        effect.edge_highlight = boost(effect.edge_highlight);
        effect.refraction = boost(effect.refraction);
        effect.inner_shadow = boost(effect.inner_shadow);
        effect.chromatic = boost(effect.chromatic);
        effect.lens_depth = boost(effect.lens_depth);
    }

    renderer.background_effect.update_config(blur_config);
    renderer
        .background_effect
        .update_render_elements(region.radius, effect, region.flags.blur);

    if !renderer.background_effect.is_visible() {
        return;
    }

    let params = RenderParams {
        geometry,
        subregion: None,
        clip: region.flags.clip.then_some((geometry, region.radius)),
        scale,
    };
    let xray_pos = xray_pos.offset(rect.loc);
    renderer
        .background_effect
        .render(ctx.r(), ns, params, xray_pos, &mut |elem| push(elem.into()));
}
