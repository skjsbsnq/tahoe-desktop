use std::sync::Arc;

use niri_config::utils::MergeWith as _;
use niri_config::{Config, LayerRule};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::{Element, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{LayerSurface, PopupKind, PopupManager};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::{remove_pre_commit_hook, with_states, HookId};
use smithay::wayland::shell::wlr_layer::{ExclusiveZone, Layer};

use super::ResolvedLayerRules;
use crate::animation::Clock;
use crate::layer::closing_layer::{
    CloseAnimationRenderState, CloseAnimationStartState, ClosingLayerRenderElement,
};
use crate::layer::opening_layer::{
    self, OpenAnimation, OpenAnimationStartState, OpenAnimationState, OpeningLayerRenderElement,
    OpeningLayerSolidColorRenderElement, OpeningLayerWaylandRenderElement,
};
use crate::layer::transform_animation::PresentationTransformAnimation;
use crate::layout::shadow::Shadow;
use crate::niri_render_elements;
use crate::protocols::tahoe_glass::{
    get_committed_regions, get_transform_directive, PresentationAffine, TahoeGlassRegion,
    TahoeGlassTransformDirective,
};
use crate::render_helpers::background_effect::BackgroundEffectElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::surface::push_elements_from_surface_tree;
use crate::render_helpers::tahoe_glass::TahoeGlassElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{background_effect, tahoe_glass, RenderCtx, RenderTarget};
use crate::utils::{baba_is_float_offset, round_logical_in_physical};

#[derive(Debug)]
pub struct MappedLayer {
    /// The surface itself.
    surface: LayerSurface,

    /// Pre-commit hook that we have on all mapped layer surfaces.
    pre_commit_hook: HookId,

    /// Up-to-date rules.
    rules: ResolvedLayerRules,

    /// Whether to recompute layer rules on the next commit.
    ///
    /// Set in the pre-commit hook when the layer changes; consumed in the commit handler.
    recompute_rules_on_commit: bool,

    /// Buffer to draw instead of the surface when it should be blocked out.
    block_out_buffer: SolidColorBuffer,

    /// The shadow around the surface.
    shadow: Shadow,

    /// The blur config, passed for background effect rendering.
    blur_config: niri_config::Blur,

    /// Tahoe compositor-owned glass material config.
    tahoe_glass_config: niri_config::TahoeGlass,

    /// The view size for the layer surface's output.
    view_size: Size<f64, Logical>,

    /// Scale of the output the layer surface is on (and rounds its sizes to).
    scale: f64,

    /// The animation upon opening this layer.
    open_animation: Option<OpenAnimation>,

    /// Steady-state presentation transform driven by the tahoe-glass protocol
    /// (v4). Identity when the surface renders untransformed.
    presentation_transform: PresentationAffine,

    /// Running presentation-transform animation, if any.
    transform_animation: Option<PresentationTransformAnimation>,

    /// Last consumed tahoe-glass transform directive epoch. Starts at zero;
    /// stale-directive replay across map cycles is prevented by clearing the
    /// published directive on unmap, not by absorbing the epoch here (the
    /// mapping commit itself may legitimately carry a directive).
    seen_transform_epoch: u64,

    /// Snapshot to use if this layer is unmapped with a close animation.
    unmap_snapshot: Option<LayerSurfaceUnmapSnapshot>,

    /// TahoeGlass regions captured before the unmap commit clears surface geometry.
    close_tahoe_glass_regions: Option<Arc<Vec<TahoeGlassRegion>>>,

    /// Clock for driving animations.
    clock: Clock,
}

niri_render_elements! {
    LayerSurfaceRenderElement<R> => {
        Wayland = WaylandSurfaceRenderElement<R>,
        SolidColor = SolidColorRenderElement,
        Shadow = ShadowRenderElement,
        BackgroundEffect = BackgroundEffectElement,
        TahoeGlass = TahoeGlassElement,
        CroppedWayland = CropRenderElement<WaylandSurfaceRenderElement<R>>,
        CroppedSolidColor = CropRenderElement<SolidColorRenderElement>,
        CroppedShadow = CropRenderElement<ShadowRenderElement>,
        CroppedBackgroundEffect = CropRenderElement<BackgroundEffectElement>,
        CroppedTahoeGlass = CropRenderElement<TahoeGlassElement>,
        OpeningWayland = OpeningLayerWaylandRenderElement<R>,
        OpeningSolidColor = OpeningLayerSolidColorRenderElement,
        OpeningShadow = OpeningLayerRenderElement<ShadowRenderElement>,
        OpeningBackgroundEffect = OpeningLayerRenderElement<BackgroundEffectElement>,
        OpeningTahoeGlass = OpeningLayerRenderElement<TahoeGlassElement>,
        // Crop after rescale/relocate so edge-reveal clip and destination share
        // the same absolute physical space when animation scale != 1.0.
        CroppedOpeningWayland = CropRenderElement<OpeningLayerWaylandRenderElement<R>>,
        CroppedOpeningSolidColor = CropRenderElement<OpeningLayerSolidColorRenderElement>,
        CroppedOpeningShadow = CropRenderElement<OpeningLayerRenderElement<ShadowRenderElement>>,
        CroppedOpeningBackgroundEffect =
            CropRenderElement<OpeningLayerRenderElement<BackgroundEffectElement>>,
        CroppedOpeningTahoeGlass = CropRenderElement<OpeningLayerRenderElement<TahoeGlassElement>>,
        Closing = ClosingLayerRenderElement,
    }
}

pub type LayerSurfaceRenderSnapshot = RenderSnapshot<
    LayerSurfaceRenderElement<GlesRenderer>,
    LayerSurfaceRenderElement<GlesRenderer>,
>;

#[derive(Debug)]
pub struct LayerSurfaceUnmapSnapshot {
    pub snapshot: LayerSurfaceRenderSnapshot,
    pub close_start: CloseAnimationStartState,
}

impl MappedLayer {
    pub fn new(
        surface: LayerSurface,
        pre_commit_hook: HookId,
        rules: ResolvedLayerRules,
        view_size: Size<f64, Logical>,
        scale: f64,
        clock: Clock,
        config: &Config,
    ) -> Self {
        let mut shadow_config = config.layout.shadow;
        // Shadows for layer surfaces need to be explicitly enabled.
        shadow_config.on = false;
        shadow_config.merge_with(&rules.shadow);

        Self {
            surface,
            pre_commit_hook,
            rules,
            recompute_rules_on_commit: false,
            block_out_buffer: SolidColorBuffer::new((0., 0.), [0., 0., 0., 1.]),
            view_size,
            scale,
            shadow: Shadow::new(shadow_config),
            blur_config: config.blur,
            tahoe_glass_config: config.tahoe_glass.clone(),
            open_animation: None,
            presentation_transform: PresentationAffine::IDENTITY,
            transform_animation: None,
            // Start at zero rather than absorbing the current epoch: smithay
            // runs post-commit hooks before CompositorHandler::commit, so a
            // directive carried by the mapping commit itself is already
            // published when this constructor runs and must still be applied.
            // Anti-replay across map cycles is handled by clearing the
            // directive on unmap (clear_transform_directive_on_unmap).
            seen_transform_epoch: 0,
            unmap_snapshot: None,
            close_tahoe_glass_regions: None,
            clock,
        }
    }

    pub fn update_config(&mut self, config: &Config) {
        let mut shadow_config = config.layout.shadow;
        // Shadows for layer surfaces need to be explicitly enabled.
        shadow_config.on = false;
        shadow_config.merge_with(&self.rules.shadow);
        self.shadow.update_config(shadow_config);

        self.blur_config = config.blur;
        self.tahoe_glass_config = config.tahoe_glass.clone();
    }

    pub fn update_shaders(&mut self) {
        self.shadow.update_shaders();
    }

    pub fn update_sizes(&mut self, view_size: Size<f64, Logical>, scale: f64) {
        self.view_size = view_size;
        self.scale = scale;
    }

    pub fn update_render_elements(&mut self, size: Size<f64, Logical>) {
        // Round to physical pixels.
        let size = size
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);

        self.block_out_buffer.resize(size);

        let radius = self.rules.geometry_corner_radius.unwrap_or_default();
        // FIXME: is_active based on keyboard focus?
        self.shadow
            .update_render_elements(size, true, radius, self.scale, 1.);
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.open_animation
            .as_ref()
            .is_some_and(|open| !open.is_done())
            || self
                .transform_animation
                .as_ref()
                .is_some_and(|anim| !anim.is_done())
    }

    pub fn should_animate_close(&self) -> bool {
        self.rules
            .layer_close
            .is_some_and(|anim| !layer_close_animation_config_is_disabled(anim))
    }

    pub fn has_non_empty_unmap_snapshot(&self) -> bool {
        self.unmap_snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.snapshot.contents.is_empty())
    }

    pub fn has_tahoe_glass_regions(&self) -> bool {
        self.has_renderable_tahoe_glass_regions()
    }

    pub fn should_render_close_effects_live(&self) -> bool {
        self.tahoe_glass_config
            .namespace_allowed(self.surface.namespace())
    }

    fn has_renderable_tahoe_glass_regions(&self) -> bool {
        if !self
            .tahoe_glass_config
            .namespace_allowed(self.surface.namespace())
        {
            return false;
        }

        self.close_tahoe_glass_regions
            .as_ref()
            .is_some_and(|regions| !regions.is_empty())
            || tahoe_glass::surface_has_regions(self.surface.wl_surface())
    }

    pub fn surface(&self) -> &LayerSurface {
        &self.surface
    }

    pub fn rules(&self) -> &ResolvedLayerRules {
        &self.rules
    }

    /// Recomputes the resolved layer rules and returns whether they changed.
    pub fn recompute_layer_rules(&mut self, rules: &[LayerRule], is_at_startup: bool) -> bool {
        let new_rules = ResolvedLayerRules::compute(rules, &self.surface, is_at_startup);
        if new_rules == self.rules {
            return false;
        }

        self.rules = new_rules;
        true
    }

    pub fn set_recompute_rules_on_commit(&mut self) {
        self.recompute_rules_on_commit = true;
    }

    pub fn take_recompute_rules_on_commit(&mut self) -> bool {
        std::mem::take(&mut self.recompute_rules_on_commit)
    }

    pub fn advance_animations(&mut self) {
        if self
            .open_animation
            .as_ref()
            .is_some_and(OpenAnimation::is_done)
        {
            self.open_animation = None;
        }

        self.consume_transform_directive();

        if let Some(anim) = &self.transform_animation {
            if anim.is_done() {
                self.presentation_transform = anim.to();
                self.transform_animation = None;
            }
        }
    }

    /// Current presentation affine (running animation or steady state).
    pub fn presentation_affine(&self) -> PresentationAffine {
        self.transform_animation
            .as_ref()
            .map_or(self.presentation_transform, |anim| anim.current())
    }

    /// Apply a newly committed tahoe-glass transform directive, if any.
    ///
    /// Runs once per event-loop cycle from [`Self::advance_animations`]; a
    /// mutex lock plus an epoch compare per mapped layer, so the idle cost is
    /// negligible. Each directive is applied exactly once.
    fn consume_transform_directive(&mut self) {
        let directive = with_states(self.surface.wl_surface(), |states| {
            get_transform_directive(states)
        });
        let Some((epoch, directive)) = directive else {
            return;
        };
        if epoch == self.seen_transform_epoch {
            return;
        }
        self.seen_transform_epoch = epoch;

        match directive {
            TahoeGlassTransformDirective::Set(affine) => {
                self.transform_animation = None;
                self.presentation_transform = affine;
            }
            TahoeGlassTransformDirective::Target(target, curve) => {
                let from = self.presentation_affine();
                self.start_transform_animation(from, target, curve);
            }
            TahoeGlassTransformDirective::Morph {
                old_rect,
                new_rect,
                curve,
            } => {
                // Map the new geometry onto the old geometry's current visual
                // footprint, then animate back to identity. Evaluating the
                // running animation here is what makes mid-flight retargeting
                // land exactly where the previous morph currently is.
                let visual = self.presentation_affine().apply_rect(old_rect);
                let Some(from) = PresentationAffine::mapping_rect(new_rect, visual) else {
                    return;
                };
                self.start_transform_animation(from, PresentationAffine::IDENTITY, curve);
            }
        }
    }

    fn start_transform_animation(
        &mut self,
        from: PresentationAffine,
        to: PresentationAffine,
        curve: crate::protocols::tahoe_glass::TahoeTransformCurve,
    ) {
        // The animation settles on `to`; record it as the steady state so a
        // dropped animation can never leave a stale transform behind.
        self.presentation_transform = to;

        if from == to {
            self.transform_animation = None;
            return;
        }

        let velocity = self
            .transform_animation
            .as_ref()
            .map_or(0., |anim| anim.velocity_toward(&from, &to));
        self.transform_animation = Some(PresentationTransformAnimation::new(
            self.clock.clone(),
            from,
            to,
            velocity,
            curve,
        ));
    }

    pub fn start_open_animation(
        &mut self,
        start: Option<OpenAnimationStartState>,
        pointer_origin: Option<Point<f64, Logical>>,
    ) {
        let Some(anim_config) = self.rules.layer_open else {
            return;
        };
        if self.open_animation.is_some() {
            return;
        }

        // Only capture pointer for origin "pointer"; other origins ignore it.
        let pointer = match anim_config.origin {
            niri_config::animations::LayerAnimationOrigin::Pointer => pointer_origin,
            _ => None,
        };

        self.open_animation = Some(OpenAnimation::new_with_pointer(
            self.clock.clone(),
            anim_config,
            start,
            pointer,
        ));
    }

    fn open_animation_state(&self) -> Option<OpenAnimationState> {
        let animation = self.open_animation.as_ref()?;
        if animation.is_done() {
            return None;
        }

        Some(animation.state())
    }

    #[cfg(test)]
    pub fn open_animation_state_for_tests(&self) -> Option<OpenAnimationState> {
        self.open_animation_state()
    }

    pub fn store_unmap_snapshot(&mut self, renderer: &mut GlesRenderer) {
        if !self.should_animate_close() {
            self.unmap_snapshot = None;
            return;
        }

        let _span = tracy_client::span!("MappedLayer::store_unmap_snapshot");
        let close_start = self.close_animation_start_state();
        let tahoe_glass_regions = with_states(self.surface.wl_surface(), get_committed_regions);
        self.close_tahoe_glass_regions =
            (!tahoe_glass_regions.is_empty()).then_some(tahoe_glass_regions);
        let render_close_effects_in_snapshot = !self.should_render_close_effects_live();

        let mut contents = Vec::new();
        self.render_normal_with_open_state(
            RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            },
            None,
            Point::from((0., 0.)),
            XrayPos::default(),
            None,
            None,
            render_close_effects_in_snapshot,
            &mut |elem| contents.push(elem),
        );
        self.render_popups_with_open_state(
            RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            },
            None,
            Point::from((0., 0.)),
            XrayPos::default(),
            None,
            None,
            &mut |elem| contents.push(elem),
        );

        let mut blocked_out_contents = Vec::new();
        self.render_normal_with_open_state(
            RenderCtx {
                renderer,
                target: RenderTarget::Screencast,
                xray: None,
            },
            None,
            Point::from((0., 0.)),
            XrayPos::default(),
            None,
            None,
            render_close_effects_in_snapshot,
            &mut |elem| blocked_out_contents.push(elem),
        );
        self.render_popups_with_open_state(
            RenderCtx {
                renderer,
                target: RenderTarget::Screencast,
                xray: None,
            },
            None,
            Point::from((0., 0.)),
            XrayPos::default(),
            None,
            None,
            &mut |elem| blocked_out_contents.push(elem),
        );

        if contents.is_empty() && blocked_out_contents.is_empty() {
            return;
        }

        let size = self.surface.cached_state().size.to_f64();
        self.unmap_snapshot = Some(LayerSurfaceUnmapSnapshot {
            snapshot: LayerSurfaceRenderSnapshot {
                contents,
                contents_with_blocked_out_bg: None,
                blocked_out_contents,
                block_out_from: self.rules.block_out_from,
                size,
                texture: Default::default(),
                texture_with_blocked_out_bg: Default::default(),
                blocked_out_texture: Default::default(),
            },
            close_start,
        });
    }

    pub fn take_unmap_snapshot(&mut self) -> Option<LayerSurfaceUnmapSnapshot> {
        self.unmap_snapshot.take()
    }

    fn close_animation_start_state(&self) -> CloseAnimationStartState {
        let Some(open_state) = self.open_animation_state() else {
            return CloseAnimationStartState::default();
        };

        CloseAnimationStartState {
            start_alpha: open_state.alpha,
            start_scale: open_state.scale(),
            start_offset: open_state.offset_for_size(self.block_out_buffer.size()),
        }
    }

    pub fn place_within_backdrop(&self) -> bool {
        if !self.rules.place_within_backdrop {
            return false;
        }

        if self.surface.layer() != Layer::Background {
            return false;
        }

        let state = self.surface.cached_state();
        if state.exclusive_zone != ExclusiveZone::DontCare {
            return false;
        }

        true
    }

    pub fn bob_offset(&self) -> Point<f64, Logical> {
        if !self.rules.baba_is_float {
            return Point::from((0., 0.));
        }

        let y = baba_is_float_offset(self.clock.now(), self.view_size.h);
        let y = round_logical_in_physical(self.scale, y);
        Point::from((0., y))
    }

    pub fn render_normal<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        self.render_normal_with_open_state(
            ctx,
            ns,
            location,
            xray_pos,
            self.open_animation_state(),
            Some(self.presentation_affine()),
            true,
            push,
        );
    }

    fn render_normal_with_open_state<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        open_state: Option<OpenAnimationState>,
        presentation: Option<PresentationAffine>,
        render_close_effects: bool,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);
        let alpha = self.rules.opacity.unwrap_or(1.).clamp(0., 1.);
        let open_alpha = open_state.map_or(1., |state| state.alpha);
        let surface_alpha = alpha * open_alpha;

        let open_size = self.block_out_buffer.size();
        let open_offset = open_state.map_or(Point::from((0., 0.)), |state| {
            state.offset_for_size(open_size)
        });
        // P06: while the open animation moves or scales the surface, the
        // glass blit region shifts every frame and the blur pyramid re-runs;
        // run it one downsample tier lower for exactly those frames.
        // Alpha-only tails (fade style, opacity delay) report false so the
        // resting appearance is restored at full quality. `bob_offset` is
        // deliberately excluded: baba-is-float bobs forever and must not
        // permanently soften a panel's blur.
        // P05: a running presentation-transform animation moves the blit
        // region the same way; steady non-identity transforms (e.g. a hidden
        // dock) are static and stay on the full-quality tier.
        let transform_animating = presentation.is_some()
            && self
                .transform_animation
                .as_ref()
                .is_some_and(|anim| !anim.is_done());
        let geometry_animating = open_state.is_some_and(|state| state.should_wrap())
            || open_offset != Point::from((0., 0.))
            || transform_animating;
        let bob_offset = self.bob_offset();
        let base_location = location + bob_offset;
        let crop_rect = open_state
            .and_then(|state| state.edge_reveal_crop_rect(base_location, open_size, scale));
        let location = base_location + open_offset;
        let moving_surface_rect =
            crop_rect.map(|_| Rectangle::new(location, open_size).to_physical_precise_round(scale));
        let xray_pos = xray_pos.offset(bob_offset + open_offset);
        let anchor = self.surface.cached_state().anchor;
        let open_wrap = open_state
            .filter(|state| state.should_wrap())
            .map(|state| WrapSpec {
                scale: Scale::from(state.scale()),
                origin: state.origin(location, open_size, anchor, scale),
                offset: Point::from((0, 0)),
            });
        let transform_wrap = presentation
            .filter(|affine| !affine.is_identity())
            .map(|affine| WrapSpec::from_affine(&affine, location, scale));
        // Transform outer, open inner: the protocol transform must hold exactly
        // in the final presentation (a hidden dock stays fully hidden while its
        // layer-open animation plays inside the translated space); composing
        // the other way would scale the protocol translation by the open scale.
        let wrap = compose_wrap_specs(open_wrap, transform_wrap);
        let mut push_opening = |elem| {
            push_opening_element(elem, wrap, scale, crop_rect, moving_surface_rect, push);
        };

        let surface = self.surface.wl_surface();

        let should_block_out = ctx.target.should_block_out(self.rules.block_out_from);
        if should_block_out {
            // Round to physical pixels.
            let location = location.to_physical_precise_round(scale).to_logical(scale);

            // FIXME: take geometry-corner-radius into account.
            let elem = SolidColorRenderElement::from_buffer(
                &self.block_out_buffer,
                location,
                surface_alpha,
                Kind::Unspecified,
            );
            push_opening(elem.into());
        } else {
            // Layer surfaces don't have extra geometry like windows.
            let buf_pos = location;

            push_elements_from_surface_tree(
                ctx.renderer,
                surface,
                buf_pos.to_physical_precise_round(scale),
                scale,
                surface_alpha,
                Kind::ScanoutCandidate,
                &mut |elem| push_opening(elem.into()),
            );
        }

        let location = location.to_physical_precise_round(scale).to_logical(scale);
        let has_tahoe_glass = if render_close_effects {
            tahoe_glass::render_for_layer(
                ctx.as_gles(),
                ns,
                surface,
                self.surface.namespace(),
                location,
                self.scale,
                &self.tahoe_glass_config,
                open_alpha,
                crop_rect,
                xray_pos,
                geometry_animating,
                &mut |elem| push_opening(elem.into()),
            )
        } else {
            // The close animation renders Tahoe glass or its fallback effect
            // live. Baking framebuffer effects into an offscreen snapshot would
            // sample that transparent snapshot instead of the desktop below.
            true
        };

        let geometry = Rectangle::new(location, self.block_out_buffer.size());
        let surface_off = Point::new(0., 0.); // No geometry on layer surfaces.
        let surface_anim_scale = Scale::from(1.);
        let radius = self.rules.geometry_corner_radius.unwrap_or_default();
        if !has_tahoe_glass {
            self.shadow.render(ctx.renderer, location, &mut |elem| {
                push_opening(elem.with_alpha(open_alpha).into())
            });

            background_effect::render_for_tile(
                ctx.as_gles(),
                ns,
                geometry,
                self.scale,
                false,
                open_alpha,
                surface,
                surface_off,
                surface_anim_scale,
                background_effect::ClientBlurRegionGeometry::BoundingBox,
                self.blur_config,
                radius,
                self.rules.background_effect,
                should_block_out,
                xray_pos,
                geometry_animating,
                &mut |elem| push_opening(elem.into()),
            );
        }
    }

    pub fn render_popups<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        self.render_popups_with_open_state(
            ctx,
            ns,
            location,
            xray_pos,
            self.open_animation_state(),
            Some(self.presentation_affine()),
            push,
        );
    }

    pub fn render_close_effects<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        close_state: CloseAnimationRenderState,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        if ctx.target.should_block_out(self.rules.block_out_from) {
            return;
        }

        let scale = Scale::from(self.scale);
        let alpha = self.rules.opacity.unwrap_or(1.).clamp(0., 1.);
        let surface_alpha = alpha * close_state.alpha;
        let base_location = location;
        let location = location + close_state.offset;
        let xray_pos = xray_pos.offset(close_state.offset);
        // P06: same rule as the open path — only close styles that move or
        // scale the surface (slide/pop/edge-reveal frames with a live offset)
        // engage the blur downsample tier; fade-style closes keep the full
        // tier since their captured pixels stay valid.
        let geometry_animating =
            close_state.should_wrap() || close_state.offset != Point::from((0., 0.));
        let crop_rect =
            close_state.edge_reveal_crop_rect(base_location, self.block_out_buffer.size(), scale);
        let moving_surface_rect = crop_rect.map(|_| {
            Rectangle::new(location, self.block_out_buffer.size()).to_physical_precise_round(scale)
        });
        let anchor = self.surface.cached_state().anchor;
        // Live close effects use Center/Anchor; Pointer needs a captured
        // location which live path does not store — fall back to Center.
        let origin = close_state.should_wrap().then(|| {
            close_animation_origin(
                close_state.origin,
                location,
                self.block_out_buffer.size(),
                anchor,
                scale,
                None,
            )
        });

        let surface = self.surface.wl_surface();
        let regions = self
            .close_tahoe_glass_regions
            .clone()
            .unwrap_or_else(|| with_states(surface, get_committed_regions));
        // When close inherits a non-1.0 scale (interrupted popin → edge-reveal),
        // rescale the moving surface rect with the same pivot so shadow excess
        // is measured in post-wrap absolute physical space.
        let post_wrap_moving_surface_rect = match origin {
            Some(origin) => moving_surface_rect
                .map(|rect| rescale_physical_rect(rect, origin, Scale::from(close_state.scale))),
            None => moving_surface_rect,
        };

        let rendered_tahoe_glass = tahoe_glass::render_frozen_regions_for_layer(
            ctx.as_gles(),
            ns,
            surface,
            self.surface.namespace(),
            location,
            self.scale,
            &self.tahoe_glass_config,
            surface_alpha,
            crop_rect,
            xray_pos,
            regions,
            geometry_animating,
            &mut |elem| {
                let elem = LayerSurfaceRenderElement::TahoeGlass(elem);
                if let Some(origin) = origin {
                    push_close_effect_element(
                        wrap_render_element_with_transform(
                            elem,
                            Scale::from(close_state.scale),
                            origin,
                            Point::from((0, 0)),
                        ),
                        scale,
                        crop_rect,
                        post_wrap_moving_surface_rect,
                        push,
                    );
                } else {
                    push_close_effect_element(elem, scale, crop_rect, moving_surface_rect, push);
                }
            },
        );

        if rendered_tahoe_glass {
            return;
        }

        let geometry = Rectangle::new(location, self.block_out_buffer.size());
        let surface_off = Point::new(0., 0.); // No geometry on layer surfaces.
        let surface_anim_scale = Scale::from(1.);
        let radius = self.rules.geometry_corner_radius.unwrap_or_default();

        self.shadow.render(ctx.renderer, location, &mut |elem| {
            let elem = LayerSurfaceRenderElement::Shadow(elem.with_alpha(close_state.alpha));
            if let Some(origin) = origin {
                push_close_effect_element(
                    wrap_render_element_with_transform(
                        elem,
                        Scale::from(close_state.scale),
                        origin,
                        Point::from((0, 0)),
                    ),
                    scale,
                    crop_rect,
                    post_wrap_moving_surface_rect,
                    push,
                );
            } else {
                push_close_effect_element(elem, scale, crop_rect, moving_surface_rect, push);
            }
        });

        background_effect::render_for_tile(
            ctx.as_gles(),
            ns,
            geometry,
            self.scale,
            false,
            close_state.alpha,
            surface,
            surface_off,
            surface_anim_scale,
            background_effect::ClientBlurRegionGeometry::BoundingBox,
            self.blur_config,
            radius,
            self.rules.background_effect,
            false,
            xray_pos,
            geometry_animating,
            &mut |elem| {
                let elem = LayerSurfaceRenderElement::BackgroundEffect(elem);
                if let Some(origin) = origin {
                    push_close_effect_element(
                        wrap_render_element_with_transform(
                            elem,
                            Scale::from(close_state.scale),
                            origin,
                            Point::from((0, 0)),
                        ),
                        scale,
                        crop_rect,
                        post_wrap_moving_surface_rect,
                        push,
                    );
                } else {
                    push_close_effect_element(elem, scale, crop_rect, moving_surface_rect, push);
                }
            },
        );
    }

    fn render_popups_with_open_state<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        open_state: Option<OpenAnimationState>,
        presentation: Option<PresentationAffine>,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        if ctx.target.should_block_out(self.rules.block_out_from) {
            return;
        }

        let scale = Scale::from(self.scale);
        let alpha = self.rules.opacity.unwrap_or(1.).clamp(0., 1.);
        let open_alpha = open_state.map_or(1., |state| state.alpha);
        let surface_alpha = alpha * open_alpha;

        let open_size = self.block_out_buffer.size();
        let open_offset = open_state.map_or(Point::from((0., 0.)), |state| {
            state.offset_for_size(open_size)
        });
        // P06: popups ride along with the layer during its open animation, so
        // their effect geometry moves with the same offsets/scale — mirror the
        // main surface's geometry-animation predicate (bob excluded likewise).
        let transform_animating = presentation.is_some()
            && self
                .transform_animation
                .as_ref()
                .is_some_and(|anim| !anim.is_done());
        let geometry_animating = open_state.is_some_and(|state| state.should_wrap())
            || open_offset != Point::from((0., 0.))
            || transform_animating;
        let bob_offset = self.bob_offset();
        let location = location + bob_offset + open_offset;
        let xray_pos = xray_pos.offset(bob_offset + open_offset);
        let anchor = self.surface.cached_state().anchor;
        let open_wrap = open_state
            .filter(|state| state.should_wrap())
            .map(|state| WrapSpec {
                scale: Scale::from(state.scale()),
                origin: state.origin(location, self.block_out_buffer.size(), anchor, scale),
                offset: Point::from((0, 0)),
            });
        let transform_wrap = presentation
            .filter(|affine| !affine.is_identity())
            .map(|affine| WrapSpec::from_affine(&affine, location, scale));
        // Transform outer, open inner: the protocol transform must hold exactly
        // in the final presentation (a hidden dock stays fully hidden while its
        // layer-open animation plays inside the translated space); composing
        // the other way would scale the protocol translation by the open scale.
        let wrap = compose_wrap_specs(open_wrap, transform_wrap);
        let mut push_opening = |elem| {
            if let Some(spec) = wrap {
                push(wrap_render_element_with_transform(
                    elem,
                    spec.scale,
                    spec.origin,
                    spec.offset,
                ));
            } else {
                push(elem);
            }
        };

        let surface = self.surface.wl_surface();
        for (popup, offset) in PopupManager::popups_for_surface(surface) {
            let popup_rules = match popup {
                PopupKind::Xdg(_) => self.rules.popups,
                // IME popups aren't affected by rules for regular popups.
                PopupKind::InputMethod(_) => niri_config::ResolvedPopupsRules::default(),
            };
            let alpha = surface_alpha * popup_rules.opacity.unwrap_or(1.).clamp(0., 1.);

            let surface = popup.wl_surface();
            let popup_geo = popup.geometry();
            let surface_loc = location + (offset - popup_geo.loc).to_f64();

            push_elements_from_surface_tree(
                ctx.renderer,
                surface,
                surface_loc.to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::ScanoutCandidate,
                &mut |elem| push_opening(elem.into()),
            );

            let geometry = Rectangle::new(location + offset.to_f64(), popup_geo.size.to_f64());
            let surface_off = popup_geo.loc.upscale(-1).to_f64();
            let surface_anim_scale = Scale::from(1.);
            let mut effect = popup_rules.background_effect;
            // Default xray to false for pop-ups since they're always on top of something.
            if effect.xray.is_none() {
                effect.xray = Some(false);
            }
            let xray_pos = xray_pos.offset(offset.to_f64());
            background_effect::render_for_tile(
                ctx.as_gles(),
                ns,
                geometry,
                self.scale,
                false,
                open_alpha,
                surface,
                surface_off,
                surface_anim_scale,
                background_effect::ClientBlurRegionGeometry::Surface,
                self.blur_config,
                popup_rules.geometry_corner_radius.unwrap_or_default(),
                effect,
                false,
                xray_pos,
                geometry_animating,
                &mut |elem| push_opening(elem.into()),
            );
        }
    }
}

fn close_animation_origin(
    origin: niri_config::animations::LayerAnimationOrigin,
    location: Point<f64, Logical>,
    size: Size<f64, Logical>,
    anchor: smithay::wayland::shell::wlr_layer::Anchor,
    output_scale: Scale<f64>,
    pointer_origin: Option<Point<f64, Logical>>,
) -> Point<i32, smithay::utils::Physical> {
    let center = location + size.to_point().downscale(2.);
    let origin = match origin {
        niri_config::animations::LayerAnimationOrigin::Center => center,
        niri_config::animations::LayerAnimationOrigin::Anchor => Point::new(
            anchor_axis_origin(
                location.x,
                size.w,
                anchor.contains(smithay::wayland::shell::wlr_layer::Anchor::LEFT),
                anchor.contains(smithay::wayland::shell::wlr_layer::Anchor::RIGHT),
            ),
            anchor_axis_origin(
                location.y,
                size.h,
                anchor.contains(smithay::wayland::shell::wlr_layer::Anchor::TOP),
                anchor.contains(smithay::wayland::shell::wlr_layer::Anchor::BOTTOM),
            ),
        ),
        niri_config::animations::LayerAnimationOrigin::Pointer => pointer_origin.unwrap_or(center),
    };

    origin.to_physical_precise_round(output_scale)
}

fn anchor_axis_origin(loc: f64, size: f64, anchored_min: bool, anchored_max: bool) -> f64 {
    match (anchored_min, anchored_max) {
        (true, false) => loc,
        (false, true) => loc + size,
        _ => loc + size / 2.,
    }
}

/// A single Rescale+Relocate wrap in absolute physical space.
///
/// Realizes `W(g) = origin + scale ⊙ (g − origin) + offset` for element
/// geometry `g`. Both the layer open animation and the tahoe-glass
/// presentation transform reduce to this form, and two of them compose into
/// one (see [`compose_wrap_specs`]), so the render elements never need nested
/// wrappers.
#[derive(Debug, Clone, Copy)]
struct WrapSpec {
    scale: Scale<f64>,
    origin: Point<i32, smithay::utils::Physical>,
    offset: Point<i32, smithay::utils::Physical>,
}

impl WrapSpec {
    /// Wrap realizing a presentation affine for a surface at `location`:
    /// per-axis scale about the surface origin, then translate.
    fn from_affine(
        affine: &PresentationAffine,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            scale: Scale {
                x: affine.scale_x,
                y: affine.scale_y,
            },
            origin: location.to_physical_precise_round(scale),
            offset: Point::<f64, Logical>::from((affine.x, affine.y))
                .to_physical_precise_round(scale),
        }
    }

    /// Offset form `W(g) = scale ⊙ g + C`, with `C` in f64 physical.
    fn canonical_offset(&self) -> Point<f64, smithay::utils::Physical> {
        Point::from((
            f64::from(self.origin.x) * (1. - self.scale.x) + f64::from(self.offset.x),
            f64::from(self.origin.y) * (1. - self.scale.y) + f64::from(self.offset.y),
        ))
    }
}

/// Compose two optional wraps into `outer ∘ inner`.
///
/// Single-wrap cases pass through untouched so the pure open-animation and
/// pure presentation-transform paths keep their exact rounding behavior.
fn compose_wrap_specs(inner: Option<WrapSpec>, outer: Option<WrapSpec>) -> Option<WrapSpec> {
    match (inner, outer) {
        (None, None) => None,
        (Some(spec), None) | (None, Some(spec)) => Some(spec),
        (Some(inner), Some(outer)) => {
            let ci = inner.canonical_offset();
            let co = outer.canonical_offset();
            let scale = Scale {
                x: outer.scale.x * inner.scale.x,
                y: outer.scale.y * inner.scale.y,
            };
            let offset = Point::from((
                (outer.scale.x * ci.x + co.x).round() as i32,
                (outer.scale.y * ci.y + co.y).round() as i32,
            ));
            Some(WrapSpec {
                scale,
                origin: Point::from((0, 0)),
                offset,
            })
        }
    }
}

fn wrap_render_element_with_transform<R: NiriRenderer>(
    elem: LayerSurfaceRenderElement<R>,
    scale: Scale<f64>,
    origin: Point<i32, smithay::utils::Physical>,
    offset: Point<i32, smithay::utils::Physical>,
) -> LayerSurfaceRenderElement<R> {
    match elem {
        LayerSurfaceRenderElement::Wayland(elem) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        LayerSurfaceRenderElement::SolidColor(elem) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        LayerSurfaceRenderElement::Shadow(elem) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        LayerSurfaceRenderElement::BackgroundEffect(elem) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        // Peel Tahoe glass shadow into OpeningShadow so post-wrap crop can
        // expand for shadow excess without treating padded glass content as
        // shadow (would widen the reveal viewport).
        LayerSurfaceRenderElement::TahoeGlass(TahoeGlassElement::Shadow(elem)) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        LayerSurfaceRenderElement::TahoeGlass(elem) => {
            opening_layer::wrap_with_transform(elem, origin, scale, offset).into()
        }
        elem @ LayerSurfaceRenderElement::CroppedWayland(_)
        | elem @ LayerSurfaceRenderElement::CroppedSolidColor(_)
        | elem @ LayerSurfaceRenderElement::CroppedShadow(_)
        | elem @ LayerSurfaceRenderElement::CroppedBackgroundEffect(_)
        | elem @ LayerSurfaceRenderElement::CroppedTahoeGlass(_)
        | elem @ LayerSurfaceRenderElement::CroppedOpeningWayland(_)
        | elem @ LayerSurfaceRenderElement::CroppedOpeningSolidColor(_)
        | elem @ LayerSurfaceRenderElement::CroppedOpeningShadow(_)
        | elem @ LayerSurfaceRenderElement::CroppedOpeningBackgroundEffect(_)
        | elem @ LayerSurfaceRenderElement::CroppedOpeningTahoeGlass(_)
        | elem @ LayerSurfaceRenderElement::OpeningWayland(_)
        | elem @ LayerSurfaceRenderElement::OpeningSolidColor(_)
        | elem @ LayerSurfaceRenderElement::OpeningShadow(_)
        | elem @ LayerSurfaceRenderElement::OpeningBackgroundEffect(_)
        | elem @ LayerSurfaceRenderElement::OpeningTahoeGlass(_)
        | elem @ LayerSurfaceRenderElement::Closing(_) => elem,
    }
}

/// Whether this element already carries edge-reveal draw_clip internally.
///
/// Framebuffer glass expands capture beyond the panel for sample padding.
/// Cropping it would shrink capture and change the sampled texture when the
/// animation settles; draw_clip keeps capture full-sized and only restricts
/// rasterization. draw_clip is absolute physical (pre-rescale destination
/// space); when the element is later rescaled, RescaleRenderElement remaps
/// dst to the post-scale destination, and clip_damage intersects absolute
/// draw_clip with that dst — so reveal clipping remains correct after wrap.
fn tahoe_glass_uses_internal_draw_clip(elem: &TahoeGlassElement) -> bool {
    matches!(
        elem,
        TahoeGlassElement::BackgroundEffect(BackgroundEffectElement::FramebufferEffect(_))
    )
}

fn push_opening_element<R: NiriRenderer>(
    elem: LayerSurfaceRenderElement<R>,
    wrap: Option<WrapSpec>,
    scale: Scale<f64>,
    crop_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    moving_surface_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
) {
    // Coordinate contract (edge-reveal + optional inherited popin scale and/or
    // tahoe-glass presentation transform, pre-composed into one WrapSpec):
    // 1. crop_rect / draw_clip: absolute physical, reveal viewport at rest (base_location, unscaled
    //    size).
    // 2. Element geometry before wrap: absolute physical of the *moving* surface (base_location +
    //    open_offset).
    // 3. Rescale wraps around a pivot in absolute physical space; geometry after wrap is still
    //    absolute physical (post-scale destination).
    // 4. Crop after wrap so crop_rect and destination share one space.
    // 5. Internal draw_clip stays absolute physical and is applied against the post-scale dst by
    //    FramebufferEffectElement::draw.
    // 6. moving_surface_rect used for shadow excess must be transformed with the same rescale so
    //    excess is measured in post-scale space.
    let (elem, moving_surface_rect) = if let Some(spec) = wrap {
        let elem = wrap_render_element_with_transform(elem, spec.scale, spec.origin, spec.offset);
        let moving_surface_rect = moving_surface_rect.map(|rect| {
            let mut rect = rescale_physical_rect(rect, spec.origin, spec.scale);
            rect.loc += spec.offset;
            rect
        });
        (elem, moving_surface_rect)
    } else {
        (elem, moving_surface_rect)
    };

    crop_layer_element(elem, scale, crop_rect, moving_surface_rect, push);
}

fn push_close_effect_element<R: NiriRenderer>(
    elem: LayerSurfaceRenderElement<R>,
    scale: Scale<f64>,
    crop_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    moving_surface_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
) {
    // Close path wraps (rescale) before calling this helper when scale != 1.
    // Callers must pass a moving_surface_rect already expressed in the same
    // post-wrap absolute physical space as `elem`.
    crop_layer_element(elem, scale, crop_rect, moving_surface_rect, push);
}

/// Match [`RescaleRenderElement`] geometry transform: translate by -origin,
/// upscale, translate by +origin. Used so shadow excess and crop share the
/// post-animation-scale absolute physical space with the wrapped element.
fn rescale_physical_rect(
    mut rect: Rectangle<i32, Physical>,
    origin: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    rect.loc -= origin;
    rect = rect.to_f64().upscale(scale).to_i32_round();
    rect.loc += origin;
    rect
}

fn crop_layer_element<R: NiriRenderer>(
    elem: LayerSurfaceRenderElement<R>,
    scale: Scale<f64>,
    crop_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    moving_surface_rect: Option<Rectangle<i32, smithay::utils::Physical>>,
    push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
) {
    let Some(crop_rect) = crop_rect else {
        push(elem);
        return;
    };

    match elem {
        LayerSurfaceRenderElement::Wayland(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::SolidColor(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::Shadow(elem) => {
            let crop_rect = shadow_crop_rect(crop_rect, moving_surface_rect, elem.geometry(scale));
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::BackgroundEffect(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::TahoeGlass(elem) => {
            if tahoe_glass_uses_internal_draw_clip(&elem) {
                push(elem.into());
                return;
            }
            let crop_rect = match &elem {
                TahoeGlassElement::Shadow(_) => {
                    shadow_crop_rect(crop_rect, moving_surface_rect, elem.geometry(scale))
                }
                _ => crop_rect,
            };
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::OpeningWayland(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::OpeningSolidColor(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::OpeningShadow(elem) => {
            let crop_rect = shadow_crop_rect(crop_rect, moving_surface_rect, elem.geometry(scale));
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::OpeningBackgroundEffect(elem) => {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        LayerSurfaceRenderElement::OpeningTahoeGlass(elem) => {
            // Framebuffer glass still relies on internal draw_clip — do not crop
            // capture geometry. is_framebuffer_effect is forwarded through the
            // rescale/relocate wrappers.
            if elem.is_framebuffer_effect() {
                push(LayerSurfaceRenderElement::OpeningTahoeGlass(elem));
                return;
            }
            // Non-shadow glass content only (shadow was peeled to OpeningShadow
            // at wrap time). Never expand the reveal crop for sample padding /
            // ExtraDamage / xray geometry — that would leak past edge-reveal.
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_rect) {
                push(elem.into());
            }
        }
        elem => push(elem),
    }
}

/// Crop decision for a layer element after optional animation rescale.
///
/// Returns whether the element was accepted (intersects crop or no crop) and
/// the geometry that would be drawn. Used by unit tests to lock wrap-then-crop
/// control flow without a GPU. Mirrors [`crop_layer_element`] policy for the
/// stub content / shadow / padded-glass cases.
#[cfg(test)]
fn crop_policy_geometry(
    elem_geo: Rectangle<i32, Physical>,
    crop_rect: Option<Rectangle<i32, Physical>>,
    moving_surface_rect: Option<Rectangle<i32, Physical>>,
    kind: CropPolicyKind,
) -> Option<Rectangle<i32, Physical>> {
    let Some(crop_rect) = crop_rect else {
        return Some(elem_geo);
    };
    let crop_rect = match kind {
        CropPolicyKind::Content | CropPolicyKind::PaddedGlass => crop_rect,
        CropPolicyKind::Shadow => shadow_crop_rect(crop_rect, moving_surface_rect, elem_geo),
    };
    elem_geo.intersection(crop_rect).filter(|r| !r.is_empty())
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CropPolicyKind {
    Content,
    Shadow,
    /// Sample-padded / xray glass content: must NOT expand reveal crop.
    PaddedGlass,
}

fn shadow_crop_rect(
    crop_rect: Rectangle<i32, Physical>,
    moving_surface_rect: Option<Rectangle<i32, Physical>>,
    shadow_geo: Rectangle<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let Some(moving_surface_rect) = moving_surface_rect else {
        return crop_rect;
    };

    let left = (i64::from(moving_surface_rect.loc.x) - i64::from(shadow_geo.loc.x)).max(0);
    let top = (i64::from(moving_surface_rect.loc.y) - i64::from(shadow_geo.loc.y)).max(0);
    let right = (rect_max_x(shadow_geo) - rect_max_x(moving_surface_rect)).max(0);
    let bottom = (rect_max_y(shadow_geo) - rect_max_y(moving_surface_rect)).max(0);

    expand_rect_i32(crop_rect, left, top, right, bottom)
}

fn rect_max_x(rect: Rectangle<i32, Physical>) -> i64 {
    i64::from(rect.loc.x) + i64::from(rect.size.w)
}

fn rect_max_y(rect: Rectangle<i32, Physical>) -> i64 {
    i64::from(rect.loc.y) + i64::from(rect.size.h)
}

fn expand_rect_i32(
    rect: Rectangle<i32, Physical>,
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
) -> Rectangle<i32, Physical> {
    let min_x = i64::from(rect.loc.x) - left;
    let min_y = i64::from(rect.loc.y) - top;
    let max_x = rect_max_x(rect) + right;
    let max_y = rect_max_y(rect) + bottom;

    let loc = Point::from((clamp_i64_to_i32(min_x), clamp_i64_to_i32(min_y)));
    let size = Size::from((
        clamp_i64_to_i32((max_x - min_x).max(0)),
        clamp_i64_to_i32((max_y - min_y).max(0)),
    ));

    Rectangle::new(loc, size)
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::element::utils::{
        Relocate, RelocateRenderElement, RescaleRenderElement,
    };
    use smithay::backend::renderer::element::{Element, Id, Kind};
    use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
    use smithay::utils::Buffer;

    use super::*;

    /// Minimal Element used to exercise rescale + crop composition without a GPU.
    #[derive(Debug)]
    struct StubElement {
        id: Id,
        geo: Rectangle<i32, Physical>,
    }

    impl StubElement {
        fn new(geo: Rectangle<i32, Physical>) -> Self {
            Self { id: Id::new(), geo }
        }
    }

    impl Element for StubElement {
        fn id(&self) -> &Id {
            &self.id
        }

        fn current_commit(&self) -> CommitCounter {
            CommitCounter::default()
        }

        fn src(&self) -> Rectangle<f64, Buffer> {
            // Physical size as a unit-scale buffer rectangle for stub tests.
            Rectangle::from_size(Size::from((
                f64::from(self.geo.size.w),
                f64::from(self.geo.size.h),
            )))
        }

        fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
            self.geo
        }

        fn damage_since(
            &self,
            _scale: Scale<f64>,
            _commit: Option<CommitCounter>,
        ) -> DamageSet<i32, Physical> {
            DamageSet::from_slice(&[Rectangle::from_size(self.geo.size)])
        }

        fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
            OpaqueRegions::default()
        }

        fn alpha(&self) -> f32 {
            1.
        }

        fn kind(&self) -> Kind {
            Kind::Unspecified
        }
    }

    #[test]
    fn shadow_crop_rect_expands_by_shadow_excess() {
        let crop = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let moving_surface = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        let shadow = Rectangle::new(Point::from((88, 22)), Size::from((238, 146)));

        let expanded = shadow_crop_rect(crop, Some(moving_surface), shadow);

        assert_eq!(
            expanded,
            Rectangle::new(Point::from((88, 82)), Size::from((238, 146)))
        );
    }

    #[test]
    fn shadow_crop_rect_keeps_content_crop_without_shadow_excess() {
        let crop = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let moving_surface = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        let shadow = Rectangle::new(Point::from((120, 60)), Size::from((80, 40)));

        let expanded = shadow_crop_rect(crop, Some(moving_surface), shadow);

        assert_eq!(expanded, crop);
    }

    #[test]
    fn rescale_physical_rect_matches_rescale_render_element_geometry() {
        let output_scale = Scale::from(1.);
        let origin = Point::from((200, 150));
        let anim_scale = 0.5;
        let pre = Rectangle::new(Point::from((100, 50)), Size::from((200, 100)));

        let stub = StubElement::new(pre);
        let wrapped = RescaleRenderElement::from_element(stub, origin, anim_scale);
        let expected = wrapped.geometry(output_scale);

        assert_eq!(
            rescale_physical_rect(pre, origin, Scale::from(anim_scale)),
            expected,
            "helper must mirror RescaleRenderElement geometry transform"
        );
    }

    /// Regression: edge-reveal crop must apply AFTER rescale so content and
    /// crop share absolute physical destination space when anim scale != 1.
    ///
    /// Old open path returned early on should_wrap and never cropped. Old close
    /// path wrapped first then failed to match Opening* variants for crop.
    /// Crop-after-wrap keeps the reveal viewport and rescaled destination
    /// consistent: geometry is the intersection of post-scale dst with crop.
    #[test]
    fn edge_reveal_crop_after_rescale_clips_to_reveal_viewport() {
        let output_scale = Scale::from(1.);
        // Rest reveal viewport (edge-reveal crop_rect / draw_clip space).
        let crop_rect = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        // Moving surface currently above the rest slot (sliding from top).
        let pre_geo = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        let origin = Point::from((200, 90)); // center of pre_geo
        let anim_scale = 0.5;

        let stub = StubElement::new(pre_geo);
        let wrapped = RelocateRenderElement::from_element(
            RescaleRenderElement::from_element(stub, origin, anim_scale),
            Point::from((0, 0)),
            Relocate::Relative,
        );
        let post_geo = wrapped.geometry(output_scale);
        let cropped = CropRenderElement::from_element(wrapped, output_scale, crop_rect)
            .expect("rescaled element still intersects the reveal viewport");

        let expected = post_geo
            .intersection(crop_rect)
            .expect("post-scale geometry must overlap reveal crop");
        assert!(!expected.is_empty());
        assert_eq!(cropped.geometry(output_scale), expected);

        // Content that scaled entirely outside the reveal viewport is dropped.
        let far = StubElement::new(Rectangle::new(
            Point::from((100, -200)),
            Size::from((200, 100)),
        ));
        let far_wrapped = RescaleRenderElement::from_element(far, Point::from((200, -150)), 0.5);
        assert!(
            CropRenderElement::from_element(far_wrapped, output_scale, crop_rect).is_none(),
            "elements fully outside the reveal viewport must not be drawn"
        );
    }

    /// Shadow excess expansion must use a moving_surface_rect already expressed
    /// in the same post-rescale space as the shadow geometry.
    #[test]
    fn shadow_crop_uses_rescaled_moving_surface_rect() {
        let crop = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let pre_surface = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        let origin = Point::from((200, 90));
        let anim_scale = 0.5;

        // Shadow extends 12px past the pre-scale surface on each side.
        let pre_shadow = expand_rect_i32(pre_surface, 12, 12, 12, 12);
        let post_surface = rescale_physical_rect(pre_surface, origin, Scale::from(anim_scale));
        let post_shadow = rescale_physical_rect(pre_shadow, origin, Scale::from(anim_scale));

        let expanded = shadow_crop_rect(crop, Some(post_surface), post_shadow);

        // Excess after scale is half the pre-scale excess (scale=0.5).
        let expected = expand_rect_i32(crop, 6, 6, 6, 6);
        assert_eq!(expanded, expected);
    }

    /// Control-flow regression: after wrap, crop must still apply. Encodes the
    /// open early-return bug (wrap then skip crop) and close Opening* mismatch.
    #[test]
    fn wrap_then_crop_policy_still_clips_content() {
        let crop = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let pre = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        let origin = Point::from((200, 90));
        let post = rescale_physical_rect(pre, origin, Scale::from(0.5));

        // Old bug: wrap and push without crop → full post geometry drawn.
        let uncropped = crop_policy_geometry(post, None, Some(post), CropPolicyKind::Content);
        assert_eq!(uncropped, Some(post));

        // Fixed: wrap then crop → intersection with rest reveal viewport.
        let cropped =
            crop_policy_geometry(post, Some(crop), Some(post), CropPolicyKind::Content).unwrap();
        assert_eq!(cropped, post.intersection(crop).unwrap());
        assert_ne!(
            cropped, post,
            "content must not draw outside reveal viewport"
        );
    }

    /// Padded glass / ExtraDamage must not expand the reveal crop the way shadow does.
    #[test]
    fn padded_glass_does_not_expand_reveal_crop() {
        let crop = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let surface = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        // Sample padding extends 16px past the panel.
        let padded = expand_rect_i32(surface, 16, 16, 16, 16);

        let as_content = crop_policy_geometry(
            padded,
            Some(crop),
            Some(surface),
            CropPolicyKind::PaddedGlass,
        )
        .unwrap();
        let as_shadow =
            crop_policy_geometry(padded, Some(crop), Some(surface), CropPolicyKind::Shadow)
                .unwrap();

        // Content/padded glass: strict rest crop.
        assert_eq!(as_content, padded.intersection(crop).unwrap());
        // Shadow path expands crop by excess — larger than rest crop.
        assert!(
            as_shadow.size.w > as_content.size.w || as_shadow.size.h > as_content.size.h,
            "shadow excess expands crop; padded glass must not use that path"
        );
    }

    /// Fractional output scale: crop_rect and geometry stay in the same physical
    /// space after to_physical_precise_round + animation rescale.
    #[test]
    fn edge_reveal_crop_after_rescale_with_fractional_output_scale() {
        let output_scale = Scale::from(1.25);
        let base_loc = Point::from((80., 80.));
        let size = Size::from((160., 80.));
        let crop_rect = Rectangle::new(base_loc, size).to_physical_precise_round(output_scale);

        // Moving surface shifted up (edge-reveal from top).
        let moving_loc = Point::from((80., 20.));
        let pre_geo = Rectangle::new(moving_loc, size).to_physical_precise_round(output_scale);
        let origin =
            (moving_loc + size.to_point().downscale(2.)).to_physical_precise_round(output_scale);
        let anim_scale = 0.6;

        let stub = StubElement::new(pre_geo);
        let wrapped = RescaleRenderElement::from_element(stub, origin, anim_scale);
        let post_geo = wrapped.geometry(output_scale);
        let cropped = CropRenderElement::from_element(wrapped, output_scale, crop_rect)
            .expect("rescaled content still intersects reveal viewport at 1.25 scale");

        assert_eq!(
            cropped.geometry(output_scale),
            post_geo.intersection(crop_rect).unwrap()
        );
    }

    /// Composed wrap must equal applying inner then outer wraps sequentially
    /// (i.e. what nesting two Rescale+Relocate layers would render).
    #[test]
    fn composed_wrap_spec_matches_nested_application() {
        fn apply(spec: WrapSpec, p: (f64, f64)) -> (f64, f64) {
            (
                f64::from(spec.origin.x)
                    + spec.scale.x * (p.0 - f64::from(spec.origin.x))
                    + f64::from(spec.offset.x),
                f64::from(spec.origin.y)
                    + spec.scale.y * (p.1 - f64::from(spec.origin.y))
                    + f64::from(spec.offset.y),
            )
        }

        // Open popin (uniform, about its own origin) as inner; anisotropic
        // presentation transform (about the surface origin, translated) outer.
        let inner = WrapSpec {
            scale: Scale { x: 0.85, y: 0.85 },
            origin: Point::from((960, 22)),
            offset: Point::from((0, 0)),
        };
        let outer = WrapSpec {
            scale: Scale { x: 0.3, y: 0.2 },
            origin: Point::from((744, 4)),
            offset: Point::from((120, -6)),
        };

        let composed = compose_wrap_specs(Some(inner), Some(outer)).unwrap();

        for p in [(0., 0.), (744., 4.), (1176., 176.), (1920., 220.)] {
            let nested = apply(outer, apply(inner, p));
            let direct = apply(composed, p);
            // The composed offset rounds once to i32 physical; allow 1px.
            assert!(
                (nested.0 - direct.0).abs() <= 1. && (nested.1 - direct.1).abs() <= 1.,
                "composed wrap diverged at {p:?}: nested {nested:?} vs direct {direct:?}"
            );
        }

        // Single-wrap cases pass the spec through untouched (exact rounding
        // preservation for the pure open / pure transform paths).
        assert!(compose_wrap_specs(None, None).is_none());
        let only = compose_wrap_specs(Some(inner), None).unwrap();
        assert_eq!(only.origin, inner.origin);
        assert_eq!(only.offset, inner.offset);
        let only = compose_wrap_specs(None, Some(outer)).unwrap();
        assert_eq!(only.origin, outer.origin);
        assert_eq!(only.offset, outer.offset);
    }
}

fn animation_config_is_disabled(config: niri_config::Animation) -> bool {
    config.off
        || matches!(
            config.kind,
            niri_config::animations::Kind::Easing(params) if params.duration_ms == 0
        )
}

fn layer_close_animation_config_is_disabled(
    config: niri_config::animations::LayerCloseAnim,
) -> bool {
    animation_config_is_disabled(config.transform_anim)
        && animation_config_is_disabled(config.opacity_anim)
}

impl Drop for MappedLayer {
    fn drop(&mut self) {
        remove_pre_commit_hook(self.surface.wl_surface(), &self.pre_commit_hook);
    }
}
