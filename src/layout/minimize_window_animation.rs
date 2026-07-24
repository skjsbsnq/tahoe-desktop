use std::cell::RefCell;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Kind, RenderElement};
#[cfg(test)]
use smithay::backend::renderer::element::Element;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform, UniformValue};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Transform};

use niri_config::BlockOutFrom;

use crate::animation::Animation;
use crate::layout::coords::{OutputLocalPoint, OutputLocalRectF};
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::shader_element::{uniform_value, ShaderRenderElement};
use crate::render_helpers::shaders::{ProgramType, Shaders};
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_encompassing_texture, RenderCtx, RenderTarget};

const GENIE_AREA_PADDING: f64 = 24.;

#[derive(Debug, Clone, Copy)]
pub enum GenieDirection {
    Minimize,
    Restore,
}

impl GenieDirection {
    fn shader_value(self) -> f32 {
        match self {
            Self::Minimize => 1.,
            Self::Restore => -1.,
        }
    }
}

#[derive(Debug)]
pub struct MinimizeWindowAnimation {
    /// Contents of the window.
    buffer: TextureBuffer<GlesTexture>,

    /// Contents that are not blocked out, but the background is blocked out.
    ///
    /// If `None` then the background doesn't have any blocked-out surfaces, and normal `buffer`
    /// can be used instead.
    buffer_with_blocked_out_bg: Option<TextureBuffer<GlesTexture>>,

    /// Blocked-out contents of the window and its texture offset.
    ///
    /// This is only rendered when the window has a block-out rule.
    blocked_out: Option<(TextureBuffer<GlesTexture>, Point<f64, Logical>)>,

    /// Where the window should be blocked out from.
    block_out_from: Option<BlockOutFrom>,

    /// Window snapshot origin in **output-local** logical coordinates.
    ///
    /// Canonical Genie space: both `pos` and `target_rect` are output-local. Callers must convert
    /// workspace-view tile positions and dock anchors before construction; this type never accepts
    /// bare workspace-content or mixed-space rectangles.
    pos: OutputLocalPoint,

    /// How much the texture should be offset.
    buffer_offset: Point<f64, Logical>,

    /// How much the texture with blocked-out bg should be offset.
    buffer_with_blocked_out_bg_offset: Point<f64, Logical>,

    /// The minimizing animation.
    anim: Animation,

    alpha_from: f32,
    alpha_to: f32,

    /// Dock / restore endpoint in the same output-local space as `pos`.
    target_rect: Option<OutputLocalRectF>,
    direction: GenieDirection,

    /// Stable Genie shader element: `Id` created once; per-frame path only mutates uniforms,
    /// texture binding, geometry and commit counter. Interior mutability so layout render stays
    /// `&self` while still updating dynamic data in place.
    genie_shader: RefCell<ShaderRenderElement>,
}

niri_render_elements! {
    MinimizeWindowAnimationRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<PrimaryGpuTextureRenderElement>>,
        Shader = ShaderRenderElement,
    }
}

impl MinimizeWindowAnimation {
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: OutputLocalPoint,
        anim: Animation,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            renderer,
            snapshot,
            scale,
            pos,
            anim,
            1.,
            0.,
            None,
            GenieDirection::Minimize,
        )
    }

    pub fn new_with_alpha<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: OutputLocalPoint,
        anim: Animation,
        alpha_from: f32,
        alpha_to: f32,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            renderer,
            snapshot,
            scale,
            pos,
            anim,
            alpha_from,
            alpha_to,
            None,
            GenieDirection::Minimize,
        )
    }

    pub fn new_with_target<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: OutputLocalPoint,
        anim: Animation,
        target_rect: Option<OutputLocalRectF>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            renderer,
            snapshot,
            scale,
            pos,
            anim,
            1.,
            0.,
            target_rect,
            GenieDirection::Minimize,
        )
    }

    pub fn new_with_source<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: OutputLocalPoint,
        anim: Animation,
        source_rect: Option<OutputLocalRectF>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            renderer,
            snapshot,
            scale,
            pos,
            anim,
            0.,
            1.,
            source_rect,
            GenieDirection::Restore,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: OutputLocalPoint,
        anim: Animation,
        alpha_from: f32,
        alpha_to: f32,
        target_rect: Option<OutputLocalRectF>,
        direction: GenieDirection,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("MinimizeWindowAnimation::new");

        let mut render_to_texture = |elements: Vec<E>| -> anyhow::Result<_> {
            let (texture, _sync_point, geo) = render_to_encompassing_texture(
                renderer,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &elements,
            )?;

            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );

            let offset = geo.loc.to_f64().to_logical(scale);

            Ok((buffer, offset))
        };

        let (buffer, buffer_offset) = render_to_texture(snapshot.contents)?;
        let (buffer_with_blocked_out_bg, buffer_with_blocked_out_bg_offset) =
            if let Some(contents) = snapshot.contents_with_blocked_out_bg {
                let (buffer, offset) = render_to_texture(contents)?;
                (Some(buffer), offset)
            } else {
                (None, Point::default())
            };
        let blocked_out = if snapshot.block_out_from.is_some() {
            Some(render_to_texture(snapshot.blocked_out_contents)?)
        } else {
            None
        };

        // Lazy sample: when diagnostics are off, note_genie_create returns after one atomic load
        // and does not touch texture sizes.
        crate::utils::lifecycle_diag::note_genie_create(|| {
            texture_variant_bytes(&buffer)
                + buffer_with_blocked_out_bg
                    .as_ref()
                    .map(texture_variant_bytes)
                    .unwrap_or(0)
                + blocked_out
                    .as_ref()
                    .map(|(buf, _)| texture_variant_bytes(buf))
                    .unwrap_or(0)
        });

        let genie_shader = RefCell::new(seed_genie_shader(buffer.texture().clone()));

        Ok(Self {
            buffer,
            buffer_with_blocked_out_bg,
            blocked_out,
            block_out_from: snapshot.block_out_from,
            pos,
            buffer_offset,
            buffer_with_blocked_out_bg_offset,
            anim,
            alpha_from,
            alpha_to,
            target_rect,
            direction,
            genie_shader,
        })
    }

    pub fn advance_animations(&mut self) {}

    pub fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }

    pub fn reverse_to_restore(
        &mut self,
        config: niri_config::Animation,
        source_rect: Option<OutputLocalRectF>,
    ) {
        let morph = self.morph_progress();
        self.restart_progress(1. - morph, 1., config);
        self.alpha_from = 0.;
        self.alpha_to = 1.;
        self.direction = GenieDirection::Restore;
        self.target_rect = source_rect;
    }

    pub fn reverse_to_minimize(
        &mut self,
        config: niri_config::Animation,
        target_rect: Option<OutputLocalRectF>,
    ) {
        let morph = self.morph_progress();
        self.restart_progress(morph, 1., config);
        self.alpha_from = 1.;
        self.alpha_to = 0.;
        self.direction = GenieDirection::Minimize;
        self.target_rect = target_rect;
    }

    /// Output-local window origin stored for Genie (test/observation only).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_output_local_pos(&self) -> Point<f64, Logical> {
        self.pos.as_point()
    }

    /// Output-local dock/restore endpoint (test/observation only).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_output_local_target(&self) -> Option<Rectangle<f64, Logical>> {
        self.target_rect.map(|r| r.as_rect())
    }

    fn morph_progress(&self) -> f64 {
        let progress = self.anim.clamped_value().clamp(0., 1.);
        match self.direction {
            GenieDirection::Minimize => progress,
            GenieDirection::Restore => 1. - progress,
        }
    }

    /// Exposes the production animation's normalized morph value to the in-tree regression
    /// harness. This is compiled out of normal builds and creates no second animation clock.
    #[cfg(test)]
    pub(crate) fn test_morph_progress(&self) -> f64 {
        self.morph_progress()
    }

    fn restart_progress(&mut self, from: f64, to: f64, config: niri_config::Animation) {
        let mut anim = self.anim.restarted(from.clamp(0., 1.), to, 0.);
        anim.replace_config(config);
        self.anim = anim;
    }

    /// Render into an **output-local** view.
    ///
    /// `view_rect` is the visible output-local viewport of the workspace surface (normally
    /// `loc = (0, 0)` and the workspace/view size). Positions stored on this animation are already
    /// output-local; the only conversion is output-local → render-element location via
    /// `location - view_rect.loc` (identity when `view_rect.loc` is zero). Callers must not pass a
    /// scrolling content-space `view_pos` here — that was the F03 mix.
    pub fn render(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
    ) -> MinimizeWindowAnimationRenderElement {
        let (buffer, offset) = if ctx.target.should_block_out(self.block_out_from) {
            let (buffer, offset) = self
                .blocked_out
                .as_ref()
                .expect("blocked-out buffer must exist when block-out is configured");
            (buffer, *offset)
        } else if ctx.target != RenderTarget::Output && self.buffer_with_blocked_out_bg.is_some() {
            (
                self.buffer_with_blocked_out_bg.as_ref().unwrap(),
                self.buffer_with_blocked_out_bg_offset,
            )
        } else {
            (&self.buffer, self.buffer_offset)
        };

        let progress = self.anim.clamped_value().clamp(0., 1.);
        let alpha = self.alpha_from + (self.alpha_to - self.alpha_from) * progress as f32;

        if let Some(target_rect) = self.target_rect {
            if let Some(elem) =
                self.render_genie(ctx, view_rect, scale, buffer, offset, target_rect)
            {
                return elem.into();
            }
        }

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            alpha,
            None,
            None,
            Kind::Unspecified,
        );

        let elem = PrimaryGpuTextureRenderElement(elem);
        let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), 1.);

        let mut location = self.pos.as_point();
        location.x -= view_rect.loc.x;
        location.y -= view_rect.loc.y;
        let location =
            location.to_physical_precise_round(scale) + offset.to_physical_precise_round(scale);
        let elem = RelocateRenderElement::from_element(elem, location, Relocate::Relative);

        elem.into()
    }

    fn render_genie(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        buffer: &TextureBuffer<GlesTexture>,
        offset: Point<f64, Logical>,
        target_rect: OutputLocalRectF,
    ) -> Option<ShaderRenderElement> {
        if Shaders::get(ctx.renderer)
            .program(ProgramType::Genie)
            .is_none()
        {
            return None;
        }

        let target_rect = target_rect.as_rect();
        if target_rect.size.w < 1. || target_rect.size.h < 1. {
            return None;
        }

        let texture_size = buffer.logical_size();
        if texture_size.w < 1. || texture_size.h < 1. {
            return None;
        }

        // Both endpoints are output-local — never union workspace-content with output-local.
        let window_rect = Rectangle::new(self.pos.as_point() + offset, texture_size);
        let area = genie_area(window_rect, target_rect);
        if area.size.w < 1. || area.size.h < 1. {
            return None;
        }

        let tex_scale = buffer.texture_scale();
        let tex_scale = Vec2::new(tex_scale.x as f32, tex_scale.y as f32);
        let tex_size = buffer.texture().size();
        let tex_size = Vec2::new(tex_size.w as f32, tex_size.h as f32) / tex_scale;
        if tex_size.x < 1. || tex_size.y < 1. {
            return None;
        }

        let window_loc = Vec2::new(window_rect.loc.x as f32, window_rect.loc.y as f32);
        let geo_to_tex =
            Mat3::from_translation(-window_loc / tex_size) * Mat3::from_scale(1. / tex_size);

        let progress = self.anim.value();
        let clamped_progress = self.anim.clamped_value().clamp(0., 1.);

        // Single output-local → render-target step (identity when view origin is zero).
        let mut location = area.loc;
        location -= view_rect.loc;

        let area_rect = rect_uniform(area);
        let window_rect_u = rect_uniform(window_rect);
        let target_rect_u = rect_uniform(target_rect);
        let progress_f = progress as f32;
        let clamped_f = clamped_progress as f32;
        let direction_f = self.direction.shader_value();
        let texture = buffer.texture().clone();

        // Stable element path: no per-frame ShaderRenderElement::new / Id::new, no Rc::new of
        // uniforms, no HashMap::from / String::from for the texture key.
        let mut genie = self.genie_shader.borrow_mut();
        genie.set_geometry(
            Rectangle::new(location, area.size),
            None,
            scale.x as f32,
            1.,
        );
        genie.set_texture(GENIE_TEX_UNIFORM, texture);
        genie.with_uniforms_mut(|uniforms| {
            debug_assert_eq!(uniforms.len(), genie_uniform::COUNT);
            uniform_value::set_rect4(&mut uniforms[genie_uniform::AREA_RECT], area_rect);
            uniform_value::set_rect4(&mut uniforms[genie_uniform::WINDOW_RECT], window_rect_u);
            uniform_value::set_rect4(&mut uniforms[genie_uniform::TARGET_RECT], target_rect_u);
            uniform_value::set_mat3(&mut uniforms[genie_uniform::GEO_TO_TEX], geo_to_tex);
            uniform_value::set_f32(&mut uniforms[genie_uniform::PROGRESS], progress_f);
            uniform_value::set_f32(&mut uniforms[genie_uniform::CLAMPED_PROGRESS], clamped_f);
            uniform_value::set_f32(&mut uniforms[genie_uniform::DIRECTION], direction_f);
        });
        genie.damage_all();

        Some(genie.clone())
    }

    /// Stable Genie element id (test/observation). Present after construction.
    #[cfg(test)]
    pub(crate) fn test_genie_element_id(&self) -> smithay::backend::renderer::element::Id {
        self.genie_shader.borrow().id().clone()
    }

    /// Genie commit counter after the last `render_genie` update (test/observation).
    #[cfg(test)]
    pub(crate) fn test_genie_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        self.genie_shader.borrow().current_commit()
    }
}

const GENIE_TEX_UNIFORM: &str = "niri_tex";

/// Indices into the seeded Genie uniform array (must match [`seed_genie_shader`]).
mod genie_uniform {
    pub const AREA_RECT: usize = 0;
    pub const WINDOW_RECT: usize = 1;
    pub const TARGET_RECT: usize = 2;
    pub const GEO_TO_TEX: usize = 3;
    pub const PROGRESS: usize = 4;
    pub const CLAMPED_PROGRESS: usize = 5;
    pub const DIRECTION: usize = 6;
    pub const COUNT: usize = 7;
}

/// Create the stable Genie element once: single `Id::new`, seed uniforms + texture key.
fn seed_genie_shader(initial_tex: GlesTexture) -> ShaderRenderElement {
    let mut elem = ShaderRenderElement::empty(ProgramType::Genie, Kind::Unspecified);
    // One-time Rc for the seven named slots; frames only mutate `.value`.
    debug_assert_eq!(genie_uniform::COUNT, 7);
    elem.seed_uniforms(Rc::new([
        Uniform::new("niri_area_rect", [0f32; 4]),
        Uniform::new("niri_window_rect", [0f32; 4]),
        Uniform::new("niri_target_rect", [0f32; 4]),
        Uniform {
            name: std::borrow::Cow::Borrowed("niri_geo_to_tex"),
            value: UniformValue::Matrix3x3 {
                matrices: vec![Mat3::IDENTITY.to_cols_array()],
                transpose: false,
            },
        },
        Uniform::new("niri_progress", 0f32),
        Uniform::new("niri_clamped_progress", 0f32),
        Uniform::new("niri_direction", 1f32),
    ]));
    // One-time String key; later frames only replace the GlesTexture value.
    elem.set_texture(GENIE_TEX_UNIFORM, initial_tex);
    elem
}

fn rect_uniform(rect: Rectangle<f64, Logical>) -> [f32; 4] {
    [
        rect.loc.x as f32,
        rect.loc.y as f32,
        rect.size.w as f32,
        rect.size.h as f32,
    ]
}

fn texture_variant_bytes(buffer: &TextureBuffer<GlesTexture>) -> u64 {
    let size = buffer.texture().size();
    let w = u64::try_from(size.w.max(0)).unwrap_or(0);
    let h = u64::try_from(size.h.max(0)).unwrap_or(0);
    // Abgr8888 snapshots used by Genie creation.
    w.saturating_mul(h).saturating_mul(4)
}

/// Test export of the production `genie_area` union (same function used by render).
#[cfg(test)]
pub mod tests_export {
    use super::*;

    pub fn genie_area_for_test(
        window_rect: Rectangle<f64, Logical>,
        target_rect: Rectangle<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        genie_area(window_rect, target_rect)
    }
}

fn genie_area(
    window_rect: Rectangle<f64, Logical>,
    target_rect: Rectangle<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let min_x = window_rect.loc.x.min(target_rect.loc.x) - GENIE_AREA_PADDING;
    let min_y = window_rect.loc.y.min(target_rect.loc.y) - GENIE_AREA_PADDING;
    let max_x = (window_rect.loc.x + window_rect.size.w)
        .max(target_rect.loc.x + target_rect.size.w)
        + GENIE_AREA_PADDING;
    let max_y = (window_rect.loc.y + window_rect.size.h)
        .max(target_rect.loc.y + target_rect.size.h)
        + GENIE_AREA_PADDING;

    Rectangle::from_extremities(Point::from((min_x, min_y)), Point::from((max_x, max_y)))
}

#[cfg(test)]
mod tests {
    use smithay::utils::Size;

    use super::*;
    use crate::layout::coords::{GenieEndpointResolve, OutputLocalRect, WorkspaceContentPoint};

    #[test]
    fn genie_area_is_window_target_union_with_padding() {
        let window = Rectangle::new(Point::from((100., 100.)), Size::from((400., 300.)));
        let target = Rectangle::new(Point::from((40., 700.)), Size::from((48., 48.)));

        let area = genie_area(window, target);

        assert_eq!(area.loc, Point::from((16., 76.)));
        assert_eq!(area.size, Size::from((508., 696.)));
    }

    #[test]
    fn genie_area_does_not_expand_beyond_local_union() {
        let window = Rectangle::new(Point::from((100., 100.)), Size::from((400., 300.)));
        let target = Rectangle::new(Point::from((200., 320.)), Size::from((48., 48.)));

        let area = genie_area(window, target);

        assert_eq!(area.loc, Point::from((76., 76.)));
        assert_eq!(area.size, Size::from((448., 348.)));
    }

    /// Research F03 numerical case: both endpoints must share output-local space so the dock is
    /// not shifted by `-view_pos` when the window content x was previously stored as view+screen.
    #[test]
    fn f03_genie_area_with_scrolled_view_keeps_dock_on_screen() {
        let view_pos = 1000.0;
        let window_content = WorkspaceContentPoint::from_point(Point::from((1100.0, 100.0)));
        let window_view = window_content.to_view(view_pos);
        let resolve = GenieEndpointResolve::identity();
        let window_ol = resolve.window_from_view_pos(window_view.as_point());
        let anchor = OutputLocalRect::new(Point::from((900, 700)), Size::from((48, 48)));
        let anchor_ol = resolve.anchor_from_output_local(anchor);

        let window_rect = Rectangle::new(window_ol.as_point(), Size::from((400.0, 300.0)));
        let area = genie_area(window_rect, anchor_ol.as_rect());

        // Output-local viewport origin is zero — single convert leaves dock at 900.
        let view_rect: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((1920.0, 1080.0)));
        let dock_on_screen = anchor_ol.loc().x - view_rect.loc.x;
        let window_on_screen = window_ol.as_point().x - view_rect.loc.x;
        assert_eq!(window_on_screen, 100.0);
        assert_eq!(dock_on_screen, 900.0);
        assert!(area.loc.x < window_on_screen.min(dock_on_screen));
        assert!((area.loc.x + area.size.w) > window_on_screen.max(dock_on_screen));

        // Contrast: old mixed path subtracted view_pos from bare dock coords.
        let buggy_dock = 900.0 - view_pos;
        assert_eq!(buggy_dock, -100.0);
        assert!(buggy_dock < 0.0);
    }

    #[test]
    fn seed_genie_shader_has_stable_id_and_seven_uniforms() {
        // Cannot construct GlesTexture without GL; exercise seed structure via empty + seed.
        let mut elem = ShaderRenderElement::empty(ProgramType::Genie, Kind::Unspecified);
        let id0 = elem.id().clone();
        elem.seed_uniforms(Rc::new([
            Uniform::new("niri_area_rect", [0f32; 4]),
            Uniform::new("niri_window_rect", [0f32; 4]),
            Uniform::new("niri_target_rect", [0f32; 4]),
            Uniform {
                name: std::borrow::Cow::Borrowed("niri_geo_to_tex"),
                value: UniformValue::Matrix3x3 {
                    matrices: vec![Mat3::IDENTITY.to_cols_array()],
                    transpose: false,
                },
            },
            Uniform::new("niri_progress", 0f32),
            Uniform::new("niri_clamped_progress", 0f32),
            Uniform::new("niri_direction", 1f32),
        ]));
        assert_eq!(elem.id(), &id0, "seed must not create a new Id");

        let c0 = elem.current_commit();
        elem.with_uniforms_mut(|u| {
            assert_eq!(u.len(), 7);
            uniform_value::set_f32(&mut u[4], 0.5);
            uniform_value::set_mat3(&mut u[3], Mat3::from_scale(Vec2::new(2., 2.)));
        });
        elem.set_geometry(
            Rectangle::new(Point::from((10., 20.)), Size::from((100., 200.))),
            None,
            1.5,
            1.,
        );
        elem.damage_all();
        let c1 = elem.current_commit();
        assert_ne!(c0, c1, "dynamic commit must advance on damage");
        assert_eq!(elem.id(), &id0, "update must keep stable Id");

        // Second frame: still same Id, commit advances again.
        elem.with_uniforms_mut(|u| uniform_value::set_f32(&mut u[4], 0.75));
        elem.damage_all();
        assert_eq!(elem.id(), &id0);
        assert_ne!(elem.current_commit(), c1);
    }

    #[test]
    fn uniform_value_set_mat3_reuses_matrices_slot() {
        let mut u = Uniform {
            name: std::borrow::Cow::Borrowed("niri_geo_to_tex"),
            value: UniformValue::Matrix3x3 {
                matrices: vec![Mat3::IDENTITY.to_cols_array()],
                transpose: false,
            },
        };
        let ptr_before = match &u.value {
            UniformValue::Matrix3x3 { matrices, .. } => matrices.as_ptr(),
            _ => panic!("expected matrix"),
        };
        uniform_value::set_mat3(&mut u, Mat3::from_translation(Vec2::new(1., 2.)));
        match &u.value {
            UniformValue::Matrix3x3 {
                matrices,
                transpose,
            } => {
                assert_eq!(matrices.len(), 1);
                assert!(!*transpose);
                assert_eq!(
                    matrices.as_ptr(),
                    ptr_before,
                    "in-place mat3 update must not reallocate the matrices Vec"
                );
            }
            _ => panic!("expected matrix after set"),
        }
    }
}
