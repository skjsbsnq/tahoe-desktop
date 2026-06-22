use niri_config::utils::MergeWith as _;
use niri_config::{Config, LayerRule};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{LayerSurface, PopupKind, PopupManager};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::{remove_pre_commit_hook, HookId};
use smithay::wayland::shell::wlr_layer::{ExclusiveZone, Layer};

use super::ResolvedLayerRules;
use crate::animation::Clock;
use crate::layer::closing_layer::{CloseAnimationStartState, ClosingLayerRenderElement};
use crate::layer::opening_layer::{
    self, OpenAnimation, OpenAnimationState, OpeningLayerRenderElement,
    OpeningLayerSolidColorRenderElement, OpeningLayerWaylandRenderElement,
};
use crate::layout::shadow::Shadow;
use crate::niri_render_elements;
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

    /// Snapshot to use if this layer is unmapped with a close animation.
    unmap_snapshot: Option<LayerSurfaceUnmapSnapshot>,

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
        OpeningWayland = OpeningLayerWaylandRenderElement<R>,
        OpeningSolidColor = OpeningLayerSolidColorRenderElement,
        OpeningShadow = OpeningLayerRenderElement<ShadowRenderElement>,
        OpeningBackgroundEffect = OpeningLayerRenderElement<BackgroundEffectElement>,
        OpeningTahoeGlass = OpeningLayerRenderElement<TahoeGlassElement>,
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
            unmap_snapshot: None,
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
        self.rules.baba_is_float
            || self
                .open_animation
                .as_ref()
                .is_some_and(|open| !open.is_done())
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
    }

    pub fn start_open_animation(&mut self) {
        let Some(anim_config) = self.rules.layer_open else {
            return;
        };
        if self.open_animation.is_some() {
            return;
        }

        self.open_animation = Some(OpenAnimation::new(self.clock.clone(), anim_config));
    }

    fn open_animation_state(&self) -> Option<OpenAnimationState> {
        let animation = self.open_animation.as_ref()?;
        if animation.is_done() {
            return None;
        }

        Some(animation.state())
    }

    pub fn store_unmap_snapshot(&mut self, renderer: &mut GlesRenderer) {
        if !self.should_animate_close() {
            self.unmap_snapshot = None;
            return;
        }

        let _span = tracy_client::span!("MappedLayer::store_unmap_snapshot");
        let close_start = self.close_animation_start_state();

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
            start_offset: open_state.offset(),
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
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);
        let alpha = self.rules.opacity.unwrap_or(1.).clamp(0., 1.);
        let open_alpha = open_state.map_or(1., |state| state.alpha);
        let surface_alpha = alpha * open_alpha;

        let open_offset = open_state.map_or(Point::from((0., 0.)), OpenAnimationState::offset);
        let bob_offset = self.bob_offset();
        let location = location + bob_offset + open_offset;
        let xray_pos = xray_pos.offset(bob_offset + open_offset);
        let anchor = self.surface.cached_state().anchor;
        let open_origin = open_state.map(|state| {
            (
                state,
                state.origin(location, self.block_out_buffer.size(), anchor, scale),
                Point::from((0, 0)),
            )
        });
        let mut push_opening = |elem| {
            if let Some((state, origin, offset)) =
                open_origin.filter(|(state, _, _)| state.should_wrap())
            {
                push(wrap_opening_render_element(elem, state, origin, offset));
            } else {
                push(elem);
            }
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
        let has_tahoe_glass = tahoe_glass::render_for_layer(
            ctx.as_gles(),
            ns,
            surface,
            self.surface.namespace(),
            location,
            self.scale,
            self.blur_config,
            &self.tahoe_glass_config,
            open_alpha,
            xray_pos,
            &mut |elem| push_opening(elem.into()),
        );

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
            push,
        );
    }

    fn render_popups_with_open_state<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        ns: Option<usize>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        open_state: Option<OpenAnimationState>,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        if ctx.target.should_block_out(self.rules.block_out_from) {
            return;
        }

        let scale = Scale::from(self.scale);
        let alpha = self.rules.opacity.unwrap_or(1.).clamp(0., 1.);
        let open_alpha = open_state.map_or(1., |state| state.alpha);
        let surface_alpha = alpha * open_alpha;

        let open_offset = open_state.map_or(Point::from((0., 0.)), OpenAnimationState::offset);
        let bob_offset = self.bob_offset();
        let location = location + bob_offset + open_offset;
        let xray_pos = xray_pos.offset(bob_offset + open_offset);
        let anchor = self.surface.cached_state().anchor;
        let open_origin = open_state.map(|state| {
            (
                state,
                state.origin(location, self.block_out_buffer.size(), anchor, scale),
                Point::from((0, 0)),
            )
        });
        let mut push_opening = |elem| {
            if let Some((state, origin, offset)) =
                open_origin.filter(|(state, _, _)| state.should_wrap())
            {
                push(wrap_opening_render_element(elem, state, origin, offset));
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
                &mut |elem| push_opening(elem.into()),
            );
        }
    }
}

fn wrap_opening_render_element<R: NiriRenderer>(
    elem: LayerSurfaceRenderElement<R>,
    state: OpenAnimationState,
    origin: Point<i32, smithay::utils::Physical>,
    offset: Point<i32, smithay::utils::Physical>,
) -> LayerSurfaceRenderElement<R> {
    match elem {
        LayerSurfaceRenderElement::Wayland(elem) => {
            opening_layer::wrap(elem, state, origin, offset).into()
        }
        LayerSurfaceRenderElement::SolidColor(elem) => {
            opening_layer::wrap(elem, state, origin, offset).into()
        }
        LayerSurfaceRenderElement::Shadow(elem) => {
            opening_layer::wrap(elem, state, origin, offset).into()
        }
        LayerSurfaceRenderElement::BackgroundEffect(elem) => {
            opening_layer::wrap(elem, state, origin, offset).into()
        }
        LayerSurfaceRenderElement::TahoeGlass(elem) => {
            opening_layer::wrap(elem, state, origin, offset).into()
        }
        elem @ LayerSurfaceRenderElement::OpeningWayland(_)
        | elem @ LayerSurfaceRenderElement::OpeningSolidColor(_)
        | elem @ LayerSurfaceRenderElement::OpeningShadow(_)
        | elem @ LayerSurfaceRenderElement::OpeningBackgroundEffect(_)
        | elem @ LayerSurfaceRenderElement::OpeningTahoeGlass(_)
        | elem @ LayerSurfaceRenderElement::Closing(_) => elem,
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
