use std::cell::RefCell;

use glam::{Mat3, Vec2};
use niri_config::CornerRadius;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{
    ContextId, Frame as _, FrameContext, Offscreen, Renderer as _, Texture as _,
};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::background_effect::{GlassOptions, RenderParams};
use crate::render_helpers::blur::{Blur, BlurOptions};
use crate::render_helpers::renderer::{
    texture_cache_matches, AsGlesFrame as _, ScratchFramebuffer,
};
use crate::render_helpers::shaders::{mat3_uniform, Shaders};
use crate::utils::region::TransformedRegion;

#[derive(Debug)]
pub struct FramebufferEffect {
    id: Id,
    commit: CommitCounter,
}

#[derive(Debug)]
pub struct FramebufferEffectElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    clip_geo: Rectangle<f64, Logical>,
    corner_radius: CornerRadius,
    subregion: Option<TransformedRegion>,
    scale: f32,
    blur_options: Option<BlurOptions>,
    noise: f32,
    saturation: f32,
    glass: GlassOptions,
    alpha: f32,
    draw_clip: Option<Rectangle<i32, Physical>>,
    /// T-32 R-3a: quantized superset band the capture blit actually read
    /// (`None` = precise element geometry, pre-T-32 behavior).
    capture_band: Option<Rectangle<i32, Physical>>,
}

#[derive(Debug)]
struct Inner {
    renderer_context_id: ContextId<GlesTexture>,
    framebuffer: Option<GlesTexture>,
    blur: Option<Blur>,
    intermediate: Option<GlesTexture>,
    /// Reusable storage for subregion-filtered damage rects.
    subregion_damage: Vec<Rectangle<i32, Physical>>,
}

/// T-32 R-3a: whether the element renders at its own geometry — i.e. no
/// crop/rescale/relocate wrapper shifted the visible destination beyond the
/// quantized cell. Wrapped elements must blit the *visible* (post-wrap) band:
/// quantizing the un-wrapped geometry would sample the wrong backdrop (the
/// 61138be-class "sampling band ≠ visible band" failure, which is why the
/// pre-T-32 code always captured at `dst`).
fn capture_band_applies(
    dst: Rectangle<i32, Physical>,
    own_geometry: Rectangle<i32, Physical>,
) -> bool {
    // Exact size, ≤1px location slack: covers f32-scale round-trip wobble on
    // non-dyadic fractional scales without admitting real wrap offsets (a 1px
    // relocation stays inside the 8px cell, so the superset still holds).
    dst.size == own_geometry.size
        && (dst.loc.x - own_geometry.loc.x).abs() <= 1
        && (dst.loc.y - own_geometry.loc.y).abs() <= 1
}

/// T-32 R-3a: the band actually blitted — the quantized superset when the
/// element renders unwrapped, else the precise (post-wrap) destination.
fn resolve_capture_dst(
    dst: Rectangle<i32, Physical>,
    own_geometry: Rectangle<i32, Physical>,
    capture_band: Option<Rectangle<i32, Physical>>,
) -> Rectangle<i32, Physical> {
    match capture_band {
        Some(band) if capture_band_applies(dst, own_geometry) => band,
        _ => dst,
    }
}

impl FramebufferEffect {
    pub fn new() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    pub fn damage(&mut self) {
        self.commit.increment();
    }

    pub fn render(
        &self,
        ns: Option<usize>,
        params: RenderParams,
        blur_options: Option<BlurOptions>,
        noise: f32,
        saturation: f32,
        glass: GlassOptions,
    ) -> FramebufferEffectElement {
        let (clip_geo, corner_radius) = params
            .clip
            .unwrap_or((params.geometry, CornerRadius::default()));

        let mut id = self.id.clone();
        if let Some(ns) = ns {
            id = id.namespaced(ns);
        }

        FramebufferEffectElement {
            id,
            commit: self.commit,
            geometry: params.geometry,
            clip_geo,
            corner_radius,
            subregion: params.subregion,
            scale: params.scale as f32,
            blur_options,
            noise,
            saturation,
            glass,
            alpha: params.alpha,
            draw_clip: params.draw_clip,
            capture_band: params.capture_band,
        }
    }
}

impl FramebufferEffectElement {
    fn compute_uniforms(
        &self,
        crop: Rectangle<f64, Logical>,
        transform: Transform,
        band_subrect: Option<(Vec2, Vec2)>,
    ) -> [Uniform<'static>; 16] {
        let offset = crop.loc - (self.clip_geo.loc - self.geometry.loc);
        let offset = Vec2::new(offset.x as f32, offset.y as f32);
        let crop_size = Vec2::new(crop.size.w as f32, crop.size.h as f32);
        let clip_size = Vec2::new(self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        // Our v_coords are [0, 1] inside crop. We want them to be [0, 1] inside clip_geo.
        let mut input_to_clip_geo =
            Mat3::from_scale(crop_size / clip_size) * Mat3::from_translation(offset / crop_size);

        // T-32 R-3a: with a quantized superset capture, v_coords span only the
        // sub-rectangle of the texture that holds the visible (precise) band.
        // Remap them so geometry coords stay anchored to the visible band —
        // otherwise the glass SDF (corner rounding, rim, highlights, clip)
        // shifts by the band expansion (0–7px) and snaps at animation end.
        // `band_subrect` = (q/p, -rel/q) per axis in texture-native units.
        if let Some((scale, translate)) = band_subrect {
            input_to_clip_geo =
                input_to_clip_geo * Mat3::from_scale(scale) * Mat3::from_translation(translate);
        }

        // Revert the effect of the texture transform.
        let transform_mat = Mat3::from_translation(Vec2::new(0.5, 0.5))
            * Mat3::from_cols_array(transform.matrix().as_ref())
            * Mat3::from_translation(Vec2::new(-0.5, -0.5));
        let input_to_clip_geo = input_to_clip_geo * transform_mat;
        let clip_geo_to_input = input_to_clip_geo.inverse();

        let clip_geo_size = (self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        [
            Uniform::new("niri_scale", self.scale),
            Uniform::new("geo_size", clip_geo_size),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            mat3_uniform("input_to_geo", input_to_clip_geo),
            mat3_uniform("geo_to_input", clip_geo_to_input),
            Uniform::new("noise", self.noise),
            Uniform::new("saturation", self.saturation),
            Uniform::new("bg_color", [0f32, 0., 0., 0.]),
            Uniform::new("tint_color", self.glass.tint_color),
            Uniform::new("tint_amount", self.glass.tint_amount),
            Uniform::new("contrast", self.glass.contrast),
            Uniform::new("edge_highlight", self.glass.edge_highlight),
            Uniform::new("refraction", self.glass.refraction),
            Uniform::new("inner_shadow", self.glass.inner_shadow),
            Uniform::new("chromatic", self.glass.chromatic),
            Uniform::new("lens_depth", self.glass.lens_depth),
        ]
    }
}

impl Element for FramebufferEffectElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        // We don't use src for drawing but we can use it to figure out how we were cropped.
        let size = self.geometry.size.to_buffer(1., Transform::Normal);
        Rectangle::from_size(size)
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

impl RenderElement<GlesRenderer> for FramebufferEffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("FramebufferEffectElement::capture_framebuffer");
        crate::utils::lifecycle_diag::note_fb_effect_capture();
        let location = gpu_span_location!("FramebufferEffectElement::capture_framebuffer");
        frame.with_gpu_span(location, |frame| {
            let output_rect = Rectangle::from_size(frame.output_size());
            let transform = frame.transformation();

            let mut guard = frame.renderer();

            let renderer = guard.as_mut();
            let renderer_context_id = renderer.context_id();
            let inner =
                cache.get_or_insert::<RefCell<Inner>, _>(|| RefCell::new(Inner::new(renderer)));
            let mut inner = inner.borrow_mut();
            let inner = &mut *inner;
            if inner.renderer_context_id != renderer_context_id {
                debug!("recreating framebuffer effect resources: renderer context changed");
                *inner = Inner::new(renderer);
            }

            inner.intermediate = None;

            // We want clamp-to-edge behavior for out-of-bounds pixels. However, glBlitFramebuffer
            // seems to skip out-of-bounds pixels, even though my reading of the docs suggests
            // otherwise (we use GL_LINEAR filter). So, clamp dst to the framebuffer bounds
            // ourselves.
            //
            // T-32 R-3a: while the element carries a quantized superset band and renders at
            // its own geometry (no wrap shifted the visible destination), blit that band —
            // the capture key was built from it, so the cached texture stays valid for
            // sub-cell band motion. Wrapped/cropped elements fall back to the precise
            // (post-wrap) destination: the backdrop must be sampled where the band is
            // actually visible (61138be lesson).
            let own_geometry = self
                .geometry
                .to_physical_precise_round(Scale::from(f64::from(self.scale)));
            let capture_dst = resolve_capture_dst(dst, own_geometry, self.capture_band);
            // Rotated outputs keep the precise path: the sub-rectangle mapping is
            // only derived/verified for the Normal transform (conservative).
            let quantized = transform == Transform::Normal
                && capture_band_applies(dst, own_geometry)
                && self.capture_band.is_some();
            let clamped_dst = match capture_dst.intersection(output_rect) {
                Some(clamped) => clamped,
                None => return Ok(()),
            };
            let clamp_scale = clamped_dst.size.to_f64() / capture_dst.size.to_f64();

            let dst = transform.transform_rect_in(clamped_dst, &output_rect.size);

            // Compute size from our geometry and scale.
            //
            // The "correct" size is always dst.size since that's the pixel region we're actually
            // blitting. However, using dst.size causes two undesirable things when zooming out for
            // the overview:
            // 1. dst.size shrinks every frame, causing a texture realloaction for every fb effect
            //    element every frame.
            // 2. The underlying blur visually expands. This is technically correct, since the
            //    underlying contents shrink, but it's not what you visually expect: you expect the
            //    blur to also shrink as the windows zoom out, to give the zooming out effect.
            //
            // Using size computed from geometry and scale solves both of those problems (even
            // though there's a bit of a cost in that zoomed-out elements still blur the entire
            // unzoomed texture size, and even though the blur ends up slightly wrong as there's two
            // layers of texture resampling, up and back down).
            //
            // Here we use src.size rather than geometry directly because src takes into account
            // cropping.
            //
            // T-32 R-3a: when the quantized band is blitted, size the capture texture from
            // that band (1:1 with the blit source) instead of src.size — the overview
            // zoom-out rationale does not apply (quantization only engages during geometry
            // animations, never the overview), and src.size would squeeze the superset band
            // into a precise-sized texture (visible softening on blur-off paths).
            let size = if quantized {
                clamped_dst.size
            } else {
                src.size
                    .to_logical(1., Transform::Normal)
                    .upscale(clamp_scale)
                    .to_physical_precise_round(self.scale)
            };
            let size = transform.transform_size(size);

            let size = size.to_logical(1).to_buffer(1, Transform::Normal);

            // P06: during geometry animations the blur pyramid starts one
            // tier down — the framebuffer blit below performs that extra
            // downsample for free into a smaller capture texture, while
            // draw() keeps mapping the (smaller) blurred result onto the
            // same destination. Blur-off captures stay at full resolution:
            // their texture is shown unfiltered.
            let size = match &self.blur_options {
                Some(options) => options.capture_size(size),
                None => size,
            };

            // Recreate framebuffer if needed.
            if inner.framebuffer.as_ref().is_some_and(|fb| {
                !texture_cache_matches(fb.size(), fb.format(), size, Fourcc::Abgr8888)
            }) {
                inner.framebuffer = None;
            }
            let framebuffer = if let Some(fb) = &inner.framebuffer {
                fb
            } else {
                trace!("creating framebuffer texture sized {} × {}", size.w, size.h);
                let texture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
                inner.framebuffer.insert(texture)
            };

            // Prepare blur textures.
            let mut blur = Option::zip(inner.blur.as_mut(), self.blur_options);
            if let Some((b, options)) = &mut blur {
                if let Err(err) = b.prepare_textures(
                    |fourcc, size| renderer.create_buffer(fourcc, size),
                    framebuffer,
                    *options,
                ) {
                    warn!("error preparing blur textures: {err:?}");
                    blur = None;
                }
            }

            // We can't use renderer.with_context() as that will reset the GlesFrame binding that we
            // want to blit from.
            let scratch_framebuffer = ScratchFramebuffer::for_renderer(renderer);
            drop(guard);

            // Blit the framebuffer contents.
            frame.with_context(|gl| unsafe {
                while gl.GetError() != ffi::NO_ERROR {}

                let mut current_fbo = 0i32;
                gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut current_fbo as *mut _);

                // BlitFramebuffer is affected by the scissor test, we don't want that.
                gl.Disable(ffi::SCISSOR_TEST);

                let fbo = scratch_framebuffer.get_or_create(gl);
                gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, fbo);

                gl.FramebufferTexture2D(
                    ffi::DRAW_FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    framebuffer.tex_id(),
                    0,
                );

                gl.BlitFramebuffer(
                    dst.loc.x,
                    dst.loc.y,
                    dst.loc.x + dst.size.w,
                    dst.loc.y + dst.size.h,
                    0,
                    0,
                    size.w,
                    size.h,
                    ffi::COLOR_BUFFER_BIT,
                    ffi::LINEAR,
                );

                gl.FramebufferTexture2D(
                    ffi::DRAW_FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    0,
                    0,
                );

                // Restore state set by GlesFrame that we just modified.
                gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, current_fbo as u32);
                gl.Enable(ffi::SCISSOR_TEST);

                if gl.GetError() != ffi::NO_ERROR {
                    Err(GlesError::BlitError)
                } else {
                    Ok(())
                }
            })??;

            // If blur is off, use the unblurred texture.
            if self.blur_options.is_none() {
                inner.intermediate = Some(framebuffer.clone());
                return Ok(());
            }

            if let Some((blur, options)) = blur {
                let mut guard = frame.renderer();
                let renderer = guard.as_mut();
                match blur.render(renderer, framebuffer, options) {
                    Ok(blurred) => inner.intermediate = Some(blurred),
                    Err(err) => {
                        warn!("error rendering blur: {err:?}");
                    }
                }
            }

            Ok(())
        })
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(cache) = cache else {
            return Ok(());
        };
        let Some(inner) = cache.get::<RefCell<Inner>>() else {
            return Ok(());
        };
        let mut inner = inner.borrow_mut();
        let inner = &mut *inner;

        let Some(texture) = &inner.intermediate else {
            return Ok(());
        };

        // Clamp the same way as in capture_framebuffer().
        let output_rect = Rectangle::from_size(frame.output_size());
        let clamped_dst = match dst.intersection(output_rect) {
            Some(clamped) => clamped,
            None => return Ok(()),
        };
        let clamp_offset = clamped_dst.loc - dst.loc;

        // T-32 R-3a: when the capture texture holds the quantized superset band, draw only
        // the sub-rectangle covering the precise clamped destination. The mapping goes
        // through the output transform so rotated outputs stay consistent with capture.
        // Wrapped/cropped elements fall back to the full texture (capture also used the
        // precise post-wrap destination).
        let own_geometry = self
            .geometry
            .to_physical_precise_round(Scale::from(f64::from(self.scale)));
        // Rotated outputs keep the precise path (see capture_framebuffer).
        let unwrapped =
            frame.transformation() == Transform::Normal && capture_band_applies(dst, own_geometry);
        let texture_rect = match (unwrapped, self.capture_band) {
            (true, Some(quantized)) => {
                let Some(q_clamped) = quantized.intersection(output_rect) else {
                    return Ok(());
                };
                // Precise clamped band relative to the quantized band, in output space.
                let rel = Rectangle::new(clamped_dst.loc - q_clamped.loc, clamped_dst.size);
                // Same transform the capture blit applied; maps into texture-native coords.
                let rel = frame
                    .transformation()
                    .transform_rect_in(rel, &q_clamped.size);
                let q_native = frame
                    .transformation()
                    .transform_size(q_clamped.size)
                    .to_f64();
                let x_ratio = texture.size().w as f64 / q_native.w;
                let y_ratio = texture.size().h as f64 / q_native.h;
                Rectangle::new(
                    Point::<f64, Buffer>::from((
                        rel.loc.x as f64 * x_ratio,
                        rel.loc.y as f64 * y_ratio,
                    )),
                    Size::<f64, Buffer>::from((
                        rel.size.w as f64 * x_ratio,
                        rel.size.h as f64 * y_ratio,
                    )),
                )
            }
            _ => Rectangle::from_size(texture.size().to_f64()),
        };

        // Filter damage by subregion, reusing the stored Vec to avoid allocation.
        let filtered = &mut inner.subregion_damage;
        filtered.clear();

        if let Some(subregion) = &self.subregion {
            // Convert to subregion coordinates.
            let mut crop = src.to_logical(1., Transform::Normal, &src.size);
            crop.loc += self.geometry.loc;
            subregion.filter_damage(crop, dst, damage, filtered);
        } else {
            filtered.extend(damage.iter());
        };

        // Adjust for clamped dst.
        if clamped_dst != dst {
            let r = Rectangle::new(clamp_offset, clamped_dst.size);
            filtered.retain_mut(|d| {
                if let Some(mut crop) = d.intersection(r) {
                    crop.loc -= clamp_offset;
                    *d = crop;
                    true
                } else {
                    false
                }
            });
        }

        // Keep framebuffer capture on the full effect geometry, but restrict
        // rasterization to the edge-reveal viewport. Damage rectangles are
        // relative to clamped_dst at this point, so clipping damage preserves
        // the full texture-to-destination mapping and works for transformed
        // outputs without inventing a second sampling coordinate system.
        if let Some(draw_clip) = self.draw_clip {
            clip_damage(filtered, clamped_dst, draw_clip);
        }

        if filtered.is_empty() {
            return Ok(());
        }
        let damage = &filtered[..];

        // Adjust src proportionally to the dst clamping.
        let src_loc = src.loc.to_logical(1., Transform::Normal, &src.size);
        let dst_to_src = src.size / dst.size.to_f64();
        let crop = Rectangle::new(
            src_loc + clamp_offset.to_f64().upscale(dst_to_src).to_logical(1.),
            clamped_dst.size.to_f64().upscale(dst_to_src).to_logical(1.),
        );

        let program = Shaders::get_from_frame(frame).postprocess_and_clip.clone();
        let uniforms = program.is_some().then(|| {
            // T-32 R-3a: compensate the shader geometry mapping for the quantized
            // sub-rectangle (q/p scale, -rel/q translation, texture-native units).
            let band_subrect = match (unwrapped, self.capture_band) {
                (true, Some(quantized)) => {
                    if let Some(q_clamped) = quantized.intersection(output_rect) {
                        let rel = Rectangle::new(clamped_dst.loc - q_clamped.loc, clamped_dst.size);
                        let rel = frame
                            .transformation()
                            .transform_rect_in(rel, &q_clamped.size);
                        let q_native = frame
                            .transformation()
                            .transform_size(q_clamped.size)
                            .to_f64();
                        let p_native = frame
                            .transformation()
                            .transform_size(clamped_dst.size)
                            .to_f64();
                        Some((
                            Vec2::new(
                                q_native.w as f32 / p_native.w as f32,
                                q_native.h as f32 / p_native.h as f32,
                            ),
                            Vec2::new(
                                -(rel.loc.x as f32) / q_native.w as f32,
                                -(rel.loc.y as f32) / q_native.h as f32,
                            ),
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            self.compute_uniforms(crop, frame.transformation(), band_subrect)
        });
        let uniforms = uniforms.as_ref().map_or(&[][..], |x| &x[..]);

        frame.render_texture_from_to(
            texture,
            texture_rect,
            clamped_dst,
            damage,
            &[],
            // The intermediate texture has the same transform as the frame.
            frame.transformation().invert(),
            self.alpha,
            program.as_ref(),
            uniforms,
        )
    }
}

fn clip_damage(
    damage: &mut Vec<Rectangle<i32, Physical>>,
    dst: Rectangle<i32, Physical>,
    clip: Rectangle<i32, Physical>,
) {
    let Some(mut clip) = dst.intersection(clip) else {
        damage.clear();
        return;
    };
    clip.loc -= dst.loc;
    damage.retain_mut(|d| {
        if let Some(crop) = d.intersection(clip) {
            *d = crop;
            true
        } else {
            false
        }
    });
}

impl<'render> RenderElement<TtyRenderer<'render>> for FramebufferEffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::capture_framebuffer(&self, gles_frame, src, dst, cache)?;
        Ok(())
    }

    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            &self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Size};

    use super::*;

    /// T-32 R-3a: quantization applies only when the element renders at its own
    /// geometry; wrapped/cropped elements keep capturing the precise destination
    /// (the backdrop must be sampled where the band is visible — 61138be lesson).
    #[test]
    fn resolve_capture_dst_quantizes_only_unwrapped() {
        let own = Rectangle::new(Point::from((100, 40)), Size::from((360, 480)));
        let quantized = Rectangle::new(Point::from((96, 40)), Size::from((368, 480)));

        // Unwrapped (dst == own geometry): the quantized superset band is blitted.
        assert_eq!(resolve_capture_dst(own, own, Some(quantized)), quantized);

        // Wrapped (visible dst moved by a transform wrapper): precise dst wins.
        let wrapped = Rectangle::new(Point::from((108, 40)), Size::from((360, 480)));
        assert_eq!(resolve_capture_dst(wrapped, own, Some(quantized)), wrapped);

        // No quantized band: dst always (pre-T-32 behavior).
        assert_eq!(resolve_capture_dst(own, own, None), own);
        assert_eq!(resolve_capture_dst(wrapped, own, None), wrapped);

        // Cropped (visible dst smaller than own geometry): precise dst wins.
        let cropped = Rectangle::new(Point::from((100, 40)), Size::from((200, 100)));
        assert_eq!(resolve_capture_dst(cropped, own, Some(quantized)), cropped);
    }

    #[test]
    fn draw_clip_is_relative_to_clamped_destination() {
        let dst = Rectangle::new(Point::from((90, 80)), Size::from((240, 160)));
        let clip = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let mut damage = vec![Rectangle::from_size(dst.size)];

        clip_damage(&mut damage, dst, clip);

        assert_eq!(
            damage,
            vec![Rectangle::new(
                Point::from((10, 20)),
                Size::from((200, 100))
            )]
        );
    }

    #[test]
    fn draw_clip_discards_damage_outside_reveal_viewport() {
        let dst = Rectangle::new(Point::from((40, 50)), Size::from((100, 80)));
        let clip = Rectangle::new(Point::from((200, 200)), Size::from((30, 30)));
        let mut damage = vec![Rectangle::from_size(dst.size)];

        clip_damage(&mut damage, dst, clip);

        assert!(damage.is_empty());
    }

    #[test]
    fn draw_clip_does_not_crop_framebuffer_capture_geometry() {
        let geometry = Rectangle::new(Point::from((84., 84.)), Size::from((232., 132.)));
        let draw_clip = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let effect = FramebufferEffect::new();
        let element = effect.render(
            None,
            RenderParams {
                geometry,
                alpha: 1.,
                subregion: None,
                clip: None,
                scale: 1.25,
                draw_clip: Some(draw_clip),
                capture_band: None,
            },
            None,
            0.,
            1.,
            GlassOptions::default(),
        );

        assert_eq!(
            element.src(),
            Rectangle::from_size(geometry.size.to_buffer(1., Transform::Normal))
        );
        assert_eq!(
            element.geometry(Scale::from(1.25)),
            geometry.to_physical_precise_round(Scale::from(1.25))
        );
        assert_eq!(element.draw_clip, Some(draw_clip));
    }

    /// When a popin scale is inherited into edge-reveal, RescaleRenderElement
    /// remaps destination to post-scale absolute physical space while draw_clip
    /// stays the absolute reveal viewport. clip_damage must still restrict
    /// rasterization to that viewport (no magic padding / scale=1 force).
    #[test]
    fn draw_clip_intersects_rescaled_destination_with_reveal_viewport() {
        // Post-rescale glass destination (shrunk around a pivot, partially
        // overlapping the rest reveal slot).
        let rescaled_dst = Rectangle::new(Point::from((125, 70)), Size::from((150, 120)));
        // Absolute edge-reveal viewport at rest.
        let draw_clip = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let mut damage = vec![Rectangle::from_size(rescaled_dst.size)];

        clip_damage(&mut damage, rescaled_dst, draw_clip);

        // Intersection of rescaled_dst and draw_clip is (125,100)-(275,190)
        // size 150x90, then made relative to rescaled_dst.loc (125,70).
        assert_eq!(
            damage,
            vec![Rectangle::new(Point::from((0, 30)), Size::from((150, 90)))]
        );
    }

    #[test]
    fn draw_clip_clears_when_rescaled_destination_misses_reveal_viewport() {
        let rescaled_dst = Rectangle::new(Point::from((100, 0)), Size::from((200, 50)));
        let draw_clip = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let mut damage = vec![Rectangle::from_size(rescaled_dst.size)];

        clip_damage(&mut damage, rescaled_dst, draw_clip);

        assert!(damage.is_empty());
    }

    /// Task 16 negative contract: omitting `clip` makes draw bounds fall back
    /// to capture geometry. For Tahoe glass that means sample padding becomes
    /// a visible halo — callers must always pass `Some(visible_region)`.
    #[test]
    fn missing_clip_falls_back_to_geometry_as_clip_geo() {
        let sample = Rectangle::new(Point::from((76., 26.)), Size::from((248., 128.)));
        let effect = FramebufferEffect::new();
        let element = effect.render(
            None,
            RenderParams {
                geometry: sample,
                alpha: 1.,
                subregion: None,
                clip: None,
                scale: 1.25,
                draw_clip: None,
                capture_band: None,
            },
            None,
            0.,
            1.,
            GlassOptions::default(),
        );

        assert_eq!(
            element.clip_geo, sample,
            "clip=None must fall back to params.geometry (sample) — the halo path"
        );
        assert_eq!(element.corner_radius, CornerRadius::default());
    }

    /// Task 16 positive contract: an explicit visible clip keeps capture on the
    /// expanded sample while draw clip_geo stays on the protocol region.
    #[test]
    fn explicit_clip_keeps_clip_geo_on_visible_not_sample() {
        let visible = Rectangle::new(Point::from((100., 50.)), Size::from((200., 80.)));
        let sample = Rectangle::new(Point::from((76., 26.)), Size::from((248., 128.)));
        let radius = CornerRadius {
            top_left: 12.,
            top_right: 12.,
            bottom_right: 12.,
            bottom_left: 12.,
        };
        let effect = FramebufferEffect::new();
        let element = effect.render(
            None,
            RenderParams {
                geometry: sample,
                alpha: 1.,
                subregion: None,
                clip: Some((visible, radius)),
                scale: 1.25,
                draw_clip: None,
                capture_band: None,
            },
            None,
            0.,
            1.,
            GlassOptions::default(),
        );

        assert_eq!(element.geometry, sample, "capture geometry stays expanded");
        assert_eq!(
            element.clip_geo, visible,
            "draw clip must stay on the protocol-visible region"
        );
        assert_ne!(
            element.clip_geo, sample,
            "draw clip must not equal sample padding bounds"
        );
        assert_eq!(element.corner_radius, radius);
    }
}

impl Inner {
    fn new(renderer: &mut GlesRenderer) -> Self {
        Inner {
            renderer_context_id: renderer.context_id(),
            framebuffer: None,
            blur: Blur::new(renderer),
            intermediate: None,
            subregion_damage: Vec::new(),
        }
    }
}
