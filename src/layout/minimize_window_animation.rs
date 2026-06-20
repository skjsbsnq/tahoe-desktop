use std::collections::HashMap;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Transform};

use niri_config::BlockOutFrom;

use crate::animation::Animation;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::shader_element::ShaderRenderElement;
use crate::render_helpers::shaders::{mat3_uniform, ProgramType, Shaders};
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_encompassing_texture, RenderCtx, RenderTarget};

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

    /// Blocked-out contents of the window.
    blocked_out_buffer: TextureBuffer<GlesTexture>,

    /// Where the window should be blocked out from.
    block_out_from: Option<BlockOutFrom>,

    /// Position in the workspace.
    pos: Point<f64, Logical>,

    /// How much the texture should be offset.
    buffer_offset: Point<f64, Logical>,

    /// How much the texture with blocked-out bg should be offset.
    buffer_with_blocked_out_bg_offset: Point<f64, Logical>,

    /// How much the blocked-out texture should be offset.
    blocked_out_buffer_offset: Point<f64, Logical>,

    /// The minimizing animation.
    anim: Animation,

    alpha_from: f32,
    alpha_to: f32,

    target_rect: Option<Rectangle<f64, Logical>>,
    direction: GenieDirection,
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
        pos: Point<f64, Logical>,
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
        pos: Point<f64, Logical>,
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
        pos: Point<f64, Logical>,
        anim: Animation,
        target_rect: Option<Rectangle<f64, Logical>>,
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
        pos: Point<f64, Logical>,
        anim: Animation,
        source_rect: Option<Rectangle<f64, Logical>>,
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
        pos: Point<f64, Logical>,
        anim: Animation,
        alpha_from: f32,
        alpha_to: f32,
        target_rect: Option<Rectangle<f64, Logical>>,
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
        let (blocked_out_buffer, blocked_out_buffer_offset) =
            render_to_texture(snapshot.blocked_out_contents)?;

        Ok(Self {
            buffer,
            buffer_with_blocked_out_bg,
            blocked_out_buffer,
            block_out_from: snapshot.block_out_from,
            pos,
            buffer_offset,
            buffer_with_blocked_out_bg_offset,
            blocked_out_buffer_offset,
            anim,
            alpha_from,
            alpha_to,
            target_rect,
            direction,
        })
    }

    pub fn advance_animations(&mut self) {}

    pub fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }

    pub fn render(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
    ) -> MinimizeWindowAnimationRenderElement {
        let (buffer, offset) = if ctx.target.should_block_out(self.block_out_from) {
            (&self.blocked_out_buffer, self.blocked_out_buffer_offset)
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

        let mut location = self.pos;
        location.x -= view_rect.loc.x;
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
        target_rect: Rectangle<f64, Logical>,
    ) -> Option<ShaderRenderElement> {
        if Shaders::get(ctx.renderer)
            .program(ProgramType::Genie)
            .is_none()
        {
            return None;
        }

        if target_rect.size.w < 1. || target_rect.size.h < 1. {
            return None;
        }

        let texture_size = buffer.logical_size();
        if texture_size.w < 1. || texture_size.h < 1. {
            return None;
        }

        let window_rect = Rectangle::new(self.pos + offset, texture_size);
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

        let uniforms = Rc::new([
            Uniform::new("niri_area_rect", rect_uniform(area)),
            Uniform::new("niri_window_rect", rect_uniform(window_rect)),
            Uniform::new("niri_target_rect", rect_uniform(target_rect)),
            mat3_uniform("niri_geo_to_tex", geo_to_tex),
            Uniform::new("niri_progress", progress as f32),
            Uniform::new("niri_clamped_progress", clamped_progress as f32),
            Uniform::new("niri_direction", self.direction.shader_value()),
        ]);

        let mut location = area.loc;
        location -= view_rect.loc;

        Some(
            ShaderRenderElement::new(
                ProgramType::Genie,
                area.size,
                None,
                scale.x as f32,
                1.,
                uniforms,
                HashMap::from([(String::from("niri_tex"), buffer.texture().clone())]),
                Kind::Unspecified,
            )
            .with_location(location),
        )
    }
}

fn rect_uniform(rect: Rectangle<f64, Logical>) -> [f32; 4] {
    [
        rect.loc.x as f32,
        rect.loc.y as f32,
        rect.size.w as f32,
        rect.size.h as f32,
    ]
}

fn genie_area(
    window_rect: Rectangle<f64, Logical>,
    target_rect: Rectangle<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let padding = 24.;
    let min_x = window_rect.loc.x.min(target_rect.loc.x) - padding;
    let min_y = window_rect.loc.y.min(target_rect.loc.y) - padding;
    let max_x = (window_rect.loc.x + window_rect.size.w)
        .max(target_rect.loc.x + target_rect.size.w)
        + padding;
    let max_y = (window_rect.loc.y + window_rect.size.h)
        .max(target_rect.loc.y + target_rect.size.h)
        + padding;

    Rectangle::from_extremities(Point::from((min_x, min_y)), Point::from((max_x, max_y)))
}
