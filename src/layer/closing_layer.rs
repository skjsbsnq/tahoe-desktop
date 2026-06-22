use anyhow::Context as _;
use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shell::wlr_layer::Anchor;

use crate::animation::Animation;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_encompassing_texture, RenderTarget};

#[derive(Debug)]
pub struct ClosingLayer {
    buffer: TextureBuffer<GlesTexture>,
    blocked_out_buffer: TextureBuffer<GlesTexture>,
    block_out_from: Option<BlockOutFrom>,
    geo_size: Size<f64, Logical>,
    pos: Point<f64, Logical>,
    buffer_offset: Point<f64, Logical>,
    blocked_out_buffer_offset: Point<f64, Logical>,
    anim: Animation,
    config: niri_config::animations::LayerCloseAnim,
    anchor: Anchor,
}

niri_render_elements! {
    ClosingLayerRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<PrimaryGpuTextureRenderElement>>,
    }
}

impl ClosingLayer {
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        mut geo_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        anim: Animation,
        config: niri_config::animations::LayerCloseAnim,
        anchor: Anchor,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("ClosingLayer::new");

        let mut render_to_texture = |elements: Vec<E>| -> anyhow::Result<_> {
            let (texture, _sync_point, geo) = render_to_encompassing_texture(
                renderer,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &elements,
            )
            .context("error rendering to texture")?;

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

        let (buffer, buffer_offset) =
            render_to_texture(snapshot.contents).context("error rendering contents")?;
        let (blocked_out_buffer, blocked_out_buffer_offset) =
            render_to_texture(snapshot.blocked_out_contents)
                .context("error rendering blocked-out contents")?;

        if geo_size.w <= 0. || geo_size.h <= 0. {
            geo_size = snapshot.size;
        }

        if geo_size.w <= 0. || geo_size.h <= 0. {
            let tex_size = buffer.texture().size().to_f64();
            geo_size = Size::new(
                (tex_size.w / scale.x).max(1.),
                (tex_size.h / scale.y).max(1.),
            );
        }

        Ok(Self {
            buffer,
            blocked_out_buffer,
            block_out_from: snapshot.block_out_from,
            geo_size,
            pos,
            buffer_offset,
            blocked_out_buffer_offset,
            anim,
            config,
            anchor,
        })
    }

    pub fn advance_animations(&mut self) {
        // Force evaluation so time-based animation state reaches the done condition.
        self.anim.value();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }

    pub fn render(
        &self,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        target: RenderTarget,
    ) -> ClosingLayerRenderElement {
        let (buffer, offset) = if target.should_block_out(self.block_out_from) {
            (&self.blocked_out_buffer, self.blocked_out_buffer_offset)
        } else {
            (&self.buffer, self.buffer_offset)
        };

        let progress = self.anim.clamped_value().clamp(0., 1.);
        let config = self.config;
        let alpha = 1. - (1. - config.opacity_to) * progress as f32;
        let scale_factor = match config.style {
            niri_config::animations::LayerCloseAnimationStyle::Popout => {
                1. + (config.scale_to - 1.) * progress
            }
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Slide => 1.,
        };
        let animation_offset = match config.style {
            niri_config::animations::LayerCloseAnimationStyle::Slide => {
                edge_offset(config.edge, config.distance * progress)
            }
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Popout => Point::from((0., 0.)),
        };

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            alpha,
            None,
            None,
            Kind::Unspecified,
        );
        let elem = PrimaryGpuTextureRenderElement(elem);

        let origin = match config.origin {
            niri_config::animations::LayerAnimationOrigin::Center => {
                self.geo_size.to_point().downscale(2.)
            }
            niri_config::animations::LayerAnimationOrigin::Anchor => Point::new(
                anchor_axis_origin(
                    self.geo_size.w,
                    self.anchor.contains(Anchor::LEFT),
                    self.anchor.contains(Anchor::RIGHT),
                ),
                anchor_axis_origin(
                    self.geo_size.h,
                    self.anchor.contains(Anchor::TOP),
                    self.anchor.contains(Anchor::BOTTOM),
                ),
            ),
        };
        let elem = RescaleRenderElement::from_element(
            elem,
            (origin - offset).to_physical_precise_round(scale),
            scale_factor.max(0.),
        );

        let mut location = self.pos + offset + animation_offset;
        location.x -= view_rect.loc.x;
        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        elem.into()
    }
}

fn edge_offset(
    edge: niri_config::animations::LayerAnimationEdge,
    distance: f64,
) -> Point<f64, Logical> {
    match edge {
        niri_config::animations::LayerAnimationEdge::Top => Point::new(0., -distance),
        niri_config::animations::LayerAnimationEdge::Right => Point::new(distance, 0.),
        niri_config::animations::LayerAnimationEdge::Bottom => Point::new(0., distance),
        niri_config::animations::LayerAnimationEdge::Left => Point::new(-distance, 0.),
    }
}

fn anchor_axis_origin(size: f64, anchored_min: bool, anchored_max: bool) -> f64 {
    match (anchored_min, anchored_max) {
        (true, false) => 0.,
        (false, true) => size,
        _ => size / 2.,
    }
}
