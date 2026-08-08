use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use niri_config::{CornerRadius, TahoeGlass, TahoeGlassMaterial};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
use smithay::wayland::compositor::{with_states, SurfaceData};

use crate::layout::shadow::Shadow;
use crate::niri_render_elements;
use crate::protocols::tahoe_glass::{get_committed_regions, TahoeGlassRegion};
use crate::render_helpers::background_effect::{
    BackgroundEffect, BackgroundEffectElement, RenderParams,
};
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::resolved_effect_plan::ResolvedEffectPlan;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;

/// Maximum number of pending damage rectangles stored per surface while it is
/// not rendered (locked session, DPMS off, hidden layers). Past this budget
/// the pending union collapses to its bounding box — a superset of the union,
/// so the next render still repaints every damaged area, while the storage
/// stays bounded (A03.2).
const MAX_PENDING_DAMAGE_RECTS: usize = 32;

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

    fn damage_regions(&mut self, old: &[TahoeGlassRegion], new: &[TahoeGlassRegion]) {
        let damage = changed_region_damage(old, new);
        let mut added = false;
        for rect in damage {
            let uncovered = rect.subtract_rects(self.damaged_regions.iter().copied());
            added |= !uncovered.is_empty();
            self.damaged_regions.extend(uncovered);
        }

        // A03.2: while the surface is not rendered the damage never drains, so
        // cap the storage. Collapse to the union bounding box — a superset, so
        // repainting it is correct — keeping the pending set bounded no matter
        // how long the client keeps committing.
        if self.damaged_regions.len() > MAX_PENDING_DAMAGE_RECTS {
            let mut min_x = i32::MAX;
            let mut min_y = i32::MAX;
            let mut max_x = i32::MIN;
            let mut max_y = i32::MIN;
            for rect in &self.damaged_regions {
                min_x = min_x.min(rect.loc.x);
                min_y = min_y.min(rect.loc.y);
                max_x = max_x.max(rect.loc.x + rect.size.w);
                max_y = max_y.max(rect.loc.y + rect.size.h);
            }
            self.damaged_regions = vec![Rectangle::new(
                Point::new(min_x, min_y),
                Size::new(max_x - min_x, max_y - min_y),
            )];
        }

        if added {
            self.damage.damage_all();
        }
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

pub fn damage_surface_regions(
    states: &SurfaceData,
    old: &[TahoeGlassRegion],
    new: &[TahoeGlassRegion],
) {
    if let Some(renderer) = states.data_map.get::<SurfaceTahoeGlassRenderer>() {
        renderer.0.lock().unwrap().damage_regions(old, new);
    }
}

fn changed_region_damage(
    old: &[TahoeGlassRegion],
    new: &[TahoeGlassRegion],
) -> Vec<Rectangle<i32, Logical>> {
    let mut changed = Vec::new();

    for region in old {
        if new.iter().find(|candidate| candidate.id == region.id) != Some(region) {
            changed.push(region.rect);
        }
    }
    for region in new {
        if old.iter().find(|candidate| candidate.id == region.id) != Some(region) {
            changed.push(region.rect);
        }
    }

    let mut union = Vec::new();
    for rect in changed {
        let uncovered = rect.subtract_rects(union.iter().copied());
        union.extend(uncovered);
    }
    union
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
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    geometry_animating: bool,
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
        config,
        layer_alpha,
        draw_clip,
        xray_pos,
        regions,
        geometry_animating,
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
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    regions: Arc<Vec<TahoeGlassRegion>>,
    geometry_animating: bool,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    render_regions_for_layer(
        ctx,
        ns,
        surface,
        namespace,
        location,
        scale,
        config,
        layer_alpha,
        draw_clip,
        xray_pos,
        regions,
        geometry_animating,
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
    config: &TahoeGlass,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    regions: Arc<Vec<TahoeGlassRegion>>,
    geometry_animating: bool,
    push: &mut dyn FnMut(TahoeGlassElement),
) -> bool {
    let _span = tracy_client::span!("TahoeGlass::render_regions_for_layer");

    if !config.namespace_allowed(namespace) {
        return false;
    }

    with_states(surface, |states| {
        let surface_id = Some(surface.id().protocol_id());
        // A03.2: drain pending damage *before* the empty-regions check. A
        // surface whose committed regions became empty still owes a repaint of
        // the area its glass used to cover; early-returning here would leave
        // the damage stored forever and the removed glass on screen.
        let renderer = SurfaceTahoeGlassRenderer::get(states);
        let mut renderer = renderer.0.lock().unwrap();
        let damage = std::mem::take(&mut renderer.damaged_regions);
        for rect in damage {
            let rect = rect.to_f64();
            let geometry = Rectangle::new(location + rect.loc, rect.size);
            push(renderer.damage.render(geometry).into());
        }

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

        renderer
            .regions
            .retain(|id, _| regions.iter().any(|region| region.id == *id));

        for region in regions.iter() {
            // R13: material.kernel is the sole blur kernel owner for this region.
            let material = config.material(&region.material);
            let region_renderer = renderer
                .regions
                .entry(region.id)
                .or_insert_with(|| TahoeGlassRegionRenderer::new(material.clone()));

            render_region(
                ctx.r(),
                ns,
                surface_id,
                region,
                region_renderer,
                material,
                location,
                scale,
                layer_alpha,
                draw_clip,
                xray_pos,
                geometry_animating,
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
    surface_id: Option<u32>,
    region: &TahoeGlassRegion,
    renderer: &mut TahoeGlassRegionRenderer,
    material: TahoeGlassMaterial,
    surface_location: Point<f64, Logical>,
    scale: f64,
    layer_alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    xray_pos: XrayPos,
    geometry_animating: bool,
    push: &mut dyn FnMut(TahoeGlassElement),
) {
    let _span = tracy_client::span!("TahoeGlass::render_region");

    let rect = region.rect.to_f64();
    let geometry = Rectangle::new(surface_location + rect.loc, rect.size);
    let material_alpha = region.material_alpha.clamp(0., 1.) * layer_alpha.clamp(0., 1.);

    // A fully transparent material has no visible contribution. Return before
    // resolving a plan or recording a capture so alpha-only fades do not pay
    // for a framebuffer/blur pass that cannot be seen.
    if material_alpha <= 0. {
        return;
    }
    crate::utils::lifecycle_diag::note_tahoe_region_capture();

    // Parse-time resolved kernel (R13); not the global blur owner.
    let blur_kernel = material.kernel;
    let (effect, sample_padding) = resolve_glass_effect_and_padding(
        region,
        material.background_effect,
        blur_kernel,
        material_alpha,
    );
    // Capture/sample geometry may expand beyond the protocol region so blur
    // and refraction have enough context. Draw/visible geometry must stay
    // exactly on the protocol region — sample padding must never become a
    // visible halo. The `clip` flag only selects rounded vs rectangular
    // corner semantics inside that visible bound.
    let sample_geometry = expand_rect(geometry, sample_padding);
    let mut params = glass_region_render_params(
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

    // R12/R13: plan uses the material's resolved kernel only.
    let has_blur_region = region.flags.blur;
    let visual =
        ResolvedEffectPlan::visual_key(blur_kernel, effect, has_blur_region, visible_radius);
    let precise_band = params.geometry.to_physical_precise_round(scale);
    // T-32 R-3a: 8px-grid superset capture while the region animates.
    let capture_band = ResolvedEffectPlan::resolve_capture_band(precise_band, geometry_animating);
    if capture_band != precise_band {
        params.capture_band = Some(capture_band);
    }
    let fast_motion = renderer.background_effect.is_fast_motion(capture_band);
    let capture = ResolvedEffectPlan::capture_key(
        blur_kernel,
        effect,
        has_blur_region,
        geometry_animating,
        fast_motion,
        capture_band,
    );
    renderer.background_effect.note_plan_keys(visual, capture);

    if let Some(plan) = ResolvedEffectPlan::build(
        blur_kernel,
        effect,
        has_blur_region,
        visible_radius,
        params,
        geometry_animating,
        fast_motion,
    ) {
        let xray_pos = xray_pos.offset(rect.loc - Point::from((sample_padding, sample_padding)));
        renderer
            .background_effect
            .render(ctx.r(), ns, surface_id, &plan, xray_pos, &mut |elem| {
                push(elem.into())
            });
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

/// Resolve draw-side material easing separately from structural capture needs.
/// The sample envelope is derived from the maximum interaction state so hover
/// and alpha animation can only change visual uniforms, not the capture geometry.
fn resolve_glass_effect_and_padding(
    region: &TahoeGlassRegion,
    material_effect: niri_config::BackgroundEffect,
    blur_kernel: niri_config::Blur,
    material_alpha: f32,
) -> (niri_config::BackgroundEffect, f64) {
    // Interaction is a draw-side multiplier, but the capture must cover its
    // entire protocol range. Resolve the envelope at the peak interaction
    // value, then keep the actual frame's effect independent below.
    let mut structural_effect = material_effect;
    apply_interaction_boost(&mut structural_effect, 1.0);
    let sample_padding = glass_sample_padding(region, structural_effect, blur_kernel);
    let mut effect = material_effect;
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

    apply_interaction_boost(&mut effect, f64::from(region.interaction));

    (effect, sample_padding)
}

fn apply_interaction_boost(effect: &mut niri_config::BackgroundEffect, interaction: f64) {
    let interaction = interaction.clamp(0., 1.);
    if interaction <= 0. {
        return;
    }

    let boost = |v: Option<f64>| v.map(|x| x * (1. + interaction));
    let boost_from_one = |v: Option<f64>| v.map(|x| 1. + (x - 1.) * (1. + interaction));
    effect.contrast = boost_from_one(effect.contrast);
    effect.edge_highlight = boost(effect.edge_highlight);
    effect.refraction = boost(effect.refraction);
    effect.inner_shadow = boost(effect.inner_shadow);
    effect.chromatic = boost(effect.chromatic);
    effect.lens_depth = boost(effect.lens_depth);
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
    let chromatic = effect.chromatic.unwrap_or(0.).abs();
    if refraction > 0.0 || lens_depth > 0.0 {
        let short_edge = f64::from(region.rect.size.w.min(region.rect.size.h).max(1));
        let refractive_padding = (refraction + lens_depth) * short_edge * 2.0 + 4.0;
        // The RGB split samples farther along the same displacement vector;
        // reserve its bounded multiplier in the same structural envelope.
        padding = padding.max(refractive_padding * (1. + chromatic * 6.));
    }

    padding.clamp(2.0, 64.0)
}

/// Build `RenderParams` that keep sample and visible geometry separated.
///
/// - `sample_geometry` expands capture for blur/refraction padding.
/// - Draw clip is **always** `Some(visible_geometry, …)`. Omitting clip lets `FramebufferEffect` /
///   xray fall back to `params.geometry` and paints the padding as a visible halo when
///   `clip=false`.
/// - The protocol `clip` flag only chooses rounded (`radius`) vs rectangular
///   (`CornerRadius::default`) material inside the visible bound — it must not opt out of clipping
///   sample padding away from the drawable region.
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
        capture_band: None,
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
    use smithay::utils::{Point, Size};

    use super::*;

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

    fn test_region(id: u32, rect: Rectangle<i32, Logical>) -> TahoeGlassRegion {
        TahoeGlassRegion {
            id,
            rect,
            radius: CornerRadius::default(),
            material: "panel".into(),
            flags: crate::protocols::tahoe_glass::TahoeGlassFlags {
                blur: true,
                shadow: true,
                clip: true,
            },
            interaction: 0.0,
            material_alpha: 1.0,
        }
    }

    fn damage_area(rects: &[Rectangle<i32, Logical>]) -> i64 {
        rects
            .iter()
            .map(|rect| i64::from(rect.size.w) * i64::from(rect.size.h))
            .sum()
    }

    #[test]
    fn changed_region_damage_ignores_unchanged_ids_and_deduplicates_overlap() {
        let unchanged = test_region(
            1,
            Rectangle::new(Point::from((600, 20)), Size::from((80, 30))),
        );
        let old = test_region(
            7,
            Rectangle::new(Point::from((100, 4)), Size::from((200, 80))),
        );
        let mut new = old.clone();
        new.rect.loc.x += 20;

        let damage = changed_region_damage(&[unchanged.clone(), old], &[unchanged, new]);
        assert_eq!(damage_area(&damage), 17_600);
        for (index, rect) in damage.iter().enumerate() {
            assert!(
                damage[index + 1..]
                    .iter()
                    .all(|other| rect.intersection(*other).is_none()),
                "damage union must not contain overlapping rectangles: {damage:?}"
            );
        }
    }

    #[test]
    fn material_only_change_damages_region_once() {
        let old = test_region(
            7,
            Rectangle::new(Point::from((100, 4)), Size::from((200, 80))),
        );
        let mut new = old.clone();
        new.material_alpha = 0.5;

        let damage = changed_region_damage(&[old], &[new]);
        assert_eq!(damage_area(&damage), 16_000);
    }

    #[test]
    fn pending_damage_stays_a_union_across_multiple_commits() {
        let old = test_region(
            7,
            Rectangle::new(Point::from((1168, 4)), Size::from((224, 40))),
        );
        let mut middle = old.clone();
        middle.rect = Rectangle::new(Point::from((1120, 4)), Size::from((320, 120)));
        let mut new = middle.clone();
        new.rect = Rectangle::new(Point::from((1060, 4)), Size::from((440, 220)));

        let mut renderer = TahoeGlassRenderer::new();
        renderer.damage_regions(&[old], &[middle.clone()]);
        renderer.damage_regions(&[middle], &[new]);

        assert_eq!(damage_area(&renderer.damaged_regions), 96_800);
        for (index, rect) in renderer.damaged_regions.iter().enumerate() {
            assert!(renderer.damaged_regions[index + 1..]
                .iter()
                .all(|other| rect.intersection(*other).is_none()));
        }
    }

    #[test]
    fn maximum_dynamic_island_union_stays_below_output_budget() {
        let compact = test_region(
            7,
            Rectangle::new(Point::from((1168, 4)), Size::from((224, 40))),
        );
        let expanded = test_region(
            7,
            Rectangle::new(Point::from((1060, 4)), Size::from((440, 220))),
        );

        let damage = changed_region_damage(&[compact], &[expanded]);
        let output_area = 2560_i64 * 1600_i64;
        assert!(damage_area(&damage) * 100 < output_area * 15);
    }

    /// A03.2: on a locked session / DPMS-off / hidden layer, no render ever
    /// drains `damaged_regions`, so 10,000 region commits must still keep the
    /// storage bounded (the pending union is capped and collapses to its
    /// bounding box), and the storage must drain on the next render.
    #[test]
    fn ten_thousand_region_commits_without_render_stay_bounded_and_recoverable() {
        // The implementation collapses the union once the rect count exceeds
        // MAX_PENDING_DAMAGE_RECTS; keep the assert headroom-bound so the
        // test compiles and fails meaningfully on the pre-cap implementation
        // (which grows the union without bound).
        const BOUNDED: usize = 64;

        let mut renderer = TahoeGlassRenderer::new();
        let mut region = test_region(1, Rectangle::new(Point::from((0, 0)), Size::from((64, 32))));
        for i in 0..10_000 {
            // Deterministic walk across a 2560x1600 surface (a lock screen
            // keeps committing regardless of rendering).
            let x = (i * 73) % 2560;
            let y = (i * 41) % 1600;
            let mut new = region.clone();
            new.rect.loc = Point::from((x as i32, y as i32));
            let old = region;
            renderer.damage_regions(&[old], std::slice::from_ref(&new));
            region = new;
        }

        let count = renderer.damaged_regions.len();
        assert!(
            count <= BOUNDED,
            "pending damage must stay bounded without renders, got {count} rects"
        );

        // Recovery: the render path drains the storage (`mem::take`) and the
        // next burst accumulates from an empty union again.
        let drained = std::mem::take(&mut renderer.damaged_regions);
        assert!(
            !drained.is_empty(),
            "drain must return the accumulated damage"
        );
        assert!(
            renderer.damaged_regions.is_empty(),
            "storage must be empty after drain"
        );

        for i in 0..10_000 {
            let x = (i * 89) % 2560;
            let y = (i * 31) % 1600;
            let mut new = region.clone();
            new.rect.loc = Point::from((x as i32, y as i32));
            let old = region;
            renderer.damage_regions(&[old], std::slice::from_ref(&new));
            region = new;
        }
        assert!(
            renderer.damaged_regions.len() <= BOUNDED,
            "post-drain burst must stay bounded, got {} rects",
            renderer.damaged_regions.len()
        );
    }

    /// A03.2: the capped storage must always remain a *covering superset* of
    /// every damaged rect seen so far: the collapse to a bounding box may
    /// over-cover, never under-cover.
    #[test]
    fn cap_collapse_keeps_union_coverage() {
        let mut renderer = TahoeGlassRenderer::new();
        let mut region = test_region(
            1,
            Rectangle::new(Point::from((100, 100)), Size::from((50, 40))),
        );
        let mut origins = Vec::new();
        for i in 0..1_000 {
            let x = 100 + (i % 100) * 90;
            let y = 100 + (i % 50) * 70;
            origins.push(Point::from((x as i32, y as i32)));
            let mut new = region.clone();
            new.rect.loc = Point::from((x as i32, y as i32));
            let old = region;
            renderer.damage_regions(&[old], std::slice::from_ref(&new));
            region = new;
        }

        // Every rect origin ever damaged must be covered by the current
        // storage (collapse is a superset of the pending union).
        for origin in &origins {
            assert!(
                renderer
                    .damaged_regions
                    .iter()
                    .any(|rect| rect.contains(*origin)),
                "damage storage must cover every damaged rect origin: {origin:?} not covered"
            );
        }
        assert!(
            renderer.damaged_regions.len() <= 64,
            "capped storage must stay within the budget, got {} rects",
            renderer.damaged_regions.len()
        );
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
            capture_band: None,
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
        let params =
            glass_region_render_params(visible, sample, false, radius, 0.85, 1.25, draw_clip);

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
        let params = glass_region_render_params(visible, sample, true, radius, 1.0, 1.0, None);

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
        let legacy =
            legacy_then_some_params(visible, sample, false, rounded_radius(), 1.0, 1.0, None);
        assert!(
            legacy.clip.is_none(),
            "documents the pre-fix wiring that painted sample padding"
        );

        let fixed =
            glass_region_render_params(visible, sample, false, rounded_radius(), 1.0, 1.0, None);
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
        let params = glass_region_render_params(
            visible,
            sample,
            false,
            CornerRadius::from(8.0),
            1.0,
            1.0,
            None,
        );

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
        let params = glass_region_render_params(
            visible,
            sample,
            true,
            rounded_radius(),
            1.0,
            2.0,
            draw_clip,
        );
        assert_eq!(params.draw_clip, draw_clip);
        assert_eq!(params.scale, 2.0);
        assert!(params.clip.is_some());
    }

    #[test]
    fn material_animation_keeps_structural_padding_and_changes_visual_effect() {
        use niri_config::{BackgroundEffect, Blur};

        let region = TahoeGlassRegion {
            id: 1,
            rect: Rectangle::new(Point::from((0, 0)), Size::from((320, 180))),
            radius: CornerRadius::default(),
            material: "panel".into(),
            flags: crate::protocols::tahoe_glass::TahoeGlassFlags {
                blur: true,
                shadow: false,
                clip: true,
            },
            interaction: 0.4,
            material_alpha: 1.0,
        };
        let base = BackgroundEffect {
            refraction: Some(0.025),
            lens_depth: Some(0.01),
            tint_amount: Some(0.4),
            ..BackgroundEffect::default()
        };
        let blur = Blur {
            offset: 2.0,
            passes: 2,
            ..Blur::default()
        };

        let at_rest_padding = glass_sample_padding(&region, base, blur);
        let mut peak_effect = base;
        apply_interaction_boost(&mut peak_effect, 1.0);
        let peak_padding = glass_sample_padding(&region, peak_effect, blur);
        let (resting, resting_padding) = resolve_glass_effect_and_padding(&region, base, blur, 1.0);
        let (faded, faded_padding) = resolve_glass_effect_and_padding(&region, base, blur, 0.5);

        assert_eq!(
            resting_padding, faded_padding,
            "material fade and interaction must not change structural capture padding"
        );
        assert_eq!(
            resting_padding, peak_padding,
            "the stable envelope must cover the interaction peak"
        );
        assert!(
            peak_padding > at_rest_padding + f64::EPSILON,
            "test material must exercise a larger interaction envelope"
        );
        assert_ne!(
            resting, faded,
            "material animation must still change draw-side visual parameters"
        );
    }
}
