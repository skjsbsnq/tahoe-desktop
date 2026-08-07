use std::cmp::max;
use std::iter::{once, successors};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{ensure, Context as _};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, link_program, GlesError, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer as _, Texture as _};
use smithay::gpu_span_location;
use smithay::utils::{Buffer, Rectangle, Size};

use crate::render_helpers::renderer::ScratchFramebuffer;
use crate::render_helpers::shaders::Shaders;

#[derive(Debug)]
pub struct Blur {
    program: BlurProgram,
    /// Context ID of the renderer that created the program and the textures.
    renderer_context_id: ContextId<GlesTexture>,
    /// Output texture followed by intermediate textures, large to small.
    ///
    /// Created lazily and stored here to avoid recreating blur textures frequently.
    textures: Vec<GlesTexture>,
    /// Number of consecutive prepares that requested a smaller capacity.
    shrink_streak: u8,
    prepared_passes: Option<u8>,
    /// One reservation per retained pyramid level. This lets a candidate
    /// pyramid reuse existing levels while charging new levels before their
    /// GL allocation is attempted.
    texture_budgets: Vec<Arc<BlurBudgetReservation>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct BlurOptions {
    pub passes: u8,
    pub offset: f64,
    /// Animation-period downsample tier (P06).
    ///
    /// Each step halves the capture resolution feeding the pyramid and trades
    /// away one blur pass, so the effective blur radius stays aligned with the
    /// static tier while the pyramid processes a quarter of the pixels.
    /// 0 = full quality (static surfaces).
    pub downsample_shift: u8,
}

/// Depth of the animation-period downsample tier: one step (P06).
///
/// A named policy constant so plan resolution and tests agree on the tier.
pub const ANIM_DOWNSAMPLE_SHIFT: u8 = 1;

/// Depth of the fast-motion downsample tier (T-32 R-3b): two steps while the
/// captured band moves more than [`FAST_MOTION_DISPLACEMENT_PX`] per frame.
/// MAX_DOWNSAMPLE_SHIFT=4 leaves headroom; the tier is temporary — it flips
/// back through the P06 capture-key invalidation contract. When the kernel
/// cannot trade two passes, fast frames fall back to the animation tier.
pub const FAST_MOTION_DOWNSAMPLE_SHIFT: u8 = 2;

/// Band displacement per frame that promotes the capture to the fast-motion
/// downsample tier. Measured as Manhattan distance between the *quantized*
/// band locations (multiples of the 8px capture cell), i.e. three cells.
pub const FAST_MOTION_DISPLACEMENT_PX: i32 = 24;

/// Hard bound on `downsample_shift`; a 1/256-area capture is far past any
/// sensible quality/perf trade and only guards programmatic misuse.
const MAX_DOWNSAMPLE_SHIFT: u8 = 4;

/// Small client-side resize steps stay inside one modest capacity bucket.
const CAPACITY_BUCKET_PX: i32 = 64;
const MAX_REUSABLE_TEXTURE_DIMENSION: i32 = 4096;
/// Per-pyramid retained byte cap. Sized so that a single 8K (7680x4320,
/// ~168 MiB) pyramid still fits; anything larger is rejected in
/// prepare_textures and the consumers fall back to the unblurred framebuffer.
const MAX_RETAINED_PYRAMID_BYTES: usize = 256 * 1024 * 1024;
const MAX_GLOBAL_BLUR_TEXTURE_BYTES: usize = 512 * 1024 * 1024;
const SHRINK_HYSTERESIS_PREPARES: u8 = 8;

static RETAINED_BLUR_TEXTURE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn retained_blur_texture_bytes() -> usize {
    RETAINED_BLUR_TEXTURE_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Serializes the process-global byte-budget accounting between the blur unit
/// tests and the integration tests (`tests::blur_capacity`), so exact counter
/// assertions never observe another test's reservation.
#[cfg(test)]
pub(crate) static BLUR_BUDGET_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug)]
struct BlurBudgetReservation {
    bytes: usize,
}

struct GlStateGuard<'a> {
    gl: &'a ffi::Gles2,
    framebuffer: ffi::types::GLint,
    viewport: [ffi::types::GLint; 4],
    active_texture: ffi::types::GLint,
    texture_binding: ffi::types::GLint,
    current_program: ffi::types::GLint,
    array_buffer: ffi::types::GLint,
    scissor_box: [ffi::types::GLint; 4],
    blend: bool,
    scissor: bool,
    attributes: Vec<GlAttributeState>,
}

struct GlAttributeState {
    index: ffi::types::GLuint,
    enabled: bool,
    size: ffi::types::GLint,
    type_: ffi::types::GLenum,
    normalized: ffi::types::GLboolean,
    stride: ffi::types::GLsizei,
    buffer: ffi::types::GLint,
    pointer: *const std::ffi::c_void,
}

impl<'a> GlStateGuard<'a> {
    unsafe fn new(
        gl: &'a ffi::Gles2,
        attribs: impl IntoIterator<Item = ffi::types::GLint>,
    ) -> Self {
        let mut framebuffer = 0;
        let mut viewport = [0; 4];
        let mut active_texture = 0;
        let mut texture_binding = 0;
        let mut current_program = 0;
        let mut array_buffer = 0;
        let mut scissor_box = [0; 4];
        gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut framebuffer);
        gl.GetIntegerv(ffi::VIEWPORT, viewport.as_mut_ptr());
        gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active_texture);
        gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut texture_binding);
        gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut current_program);
        gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut array_buffer);
        gl.GetIntegerv(ffi::SCISSOR_BOX, scissor_box.as_mut_ptr());

        let attributes = attribs
            .into_iter()
            .filter(|index| *index >= 0)
            .map(|index| {
                let index = index as ffi::types::GLuint;
                let mut enabled = 0;
                let mut size = 0;
                let mut type_ = 0;
                let mut normalized = 0;
                let mut stride = 0;
                let mut buffer = 0;
                let mut pointer = std::ptr::null_mut();
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_ENABLED, &mut enabled);
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_SIZE, &mut size);
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_TYPE, &mut type_);
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_NORMALIZED, &mut normalized);
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_STRIDE, &mut stride);
                gl.GetVertexAttribiv(index, ffi::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING, &mut buffer);
                gl.GetVertexAttribPointerv(
                    index,
                    ffi::VERTEX_ATTRIB_ARRAY_POINTER,
                    &mut pointer as *mut _ as *const _,
                );
                GlAttributeState {
                    index,
                    enabled: enabled != ffi::FALSE as i32,
                    size,
                    type_: type_ as ffi::types::GLenum,
                    normalized: normalized as ffi::types::GLboolean,
                    stride,
                    buffer,
                    pointer,
                }
            })
            .collect();

        Self {
            gl,
            framebuffer,
            viewport,
            active_texture,
            texture_binding,
            current_program,
            array_buffer,
            scissor_box,
            blend: gl.IsEnabled(ffi::BLEND) != ffi::FALSE,
            scissor: gl.IsEnabled(ffi::SCISSOR_TEST) != ffi::FALSE,
            attributes,
        }
    }
}

impl Drop for GlStateGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl
                .BindFramebuffer(ffi::FRAMEBUFFER, self.framebuffer as _);
            self.gl.Viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            self.gl.Scissor(
                self.scissor_box[0],
                self.scissor_box[1],
                self.scissor_box[2],
                self.scissor_box[3],
            );
            self.gl.ActiveTexture(self.active_texture as _);
            self.gl
                .BindTexture(ffi::TEXTURE_2D, self.texture_binding as _);
            self.gl.UseProgram(self.current_program as _);
            for attribute in &self.attributes {
                self.gl.BindBuffer(ffi::ARRAY_BUFFER, attribute.buffer as _);
                self.gl.VertexAttribPointer(
                    attribute.index,
                    attribute.size,
                    attribute.type_,
                    attribute.normalized,
                    attribute.stride,
                    attribute.pointer,
                );
                if attribute.enabled {
                    self.gl.EnableVertexAttribArray(attribute.index);
                } else {
                    self.gl.DisableVertexAttribArray(attribute.index);
                }
            }
            self.gl
                .BindBuffer(ffi::ARRAY_BUFFER, self.array_buffer as _);
            if self.blend {
                self.gl.Enable(ffi::BLEND);
            } else {
                self.gl.Disable(ffi::BLEND);
            }
            if self.scissor {
                self.gl.Enable(ffi::SCISSOR_TEST);
            } else {
                self.gl.Disable(ffi::SCISSOR_TEST);
            }
        }
    }
}

struct TextureParameterState {
    texture: ffi::types::GLuint,
    min_filter: ffi::types::GLint,
    mag_filter: ffi::types::GLint,
    wrap_s: ffi::types::GLint,
    wrap_t: ffi::types::GLint,
}

struct TextureParameterGuard<'a> {
    gl: &'a ffi::Gles2,
    active_texture: ffi::types::GLint,
    texture_binding: ffi::types::GLint,
    states: Vec<TextureParameterState>,
}

impl<'a> TextureParameterGuard<'a> {
    unsafe fn new(
        gl: &'a ffi::Gles2,
        textures: impl IntoIterator<Item = ffi::types::GLuint>,
    ) -> Self {
        let mut active_texture = 0;
        gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active_texture);
        gl.ActiveTexture(ffi::TEXTURE0);
        let mut texture_binding = 0;
        gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut texture_binding);
        let states = textures
            .into_iter()
            .map(|texture| {
                gl.BindTexture(ffi::TEXTURE_2D, texture);
                let mut params = [0; 4];
                gl.GetTexParameteriv(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, &mut params[0]);
                gl.GetTexParameteriv(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, &mut params[1]);
                gl.GetTexParameteriv(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, &mut params[2]);
                gl.GetTexParameteriv(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, &mut params[3]);
                TextureParameterState {
                    texture,
                    min_filter: params[0],
                    mag_filter: params[1],
                    wrap_s: params[2],
                    wrap_t: params[3],
                }
            })
            .collect();
        Self {
            gl,
            active_texture,
            texture_binding,
            states,
        }
    }
}

impl Drop for TextureParameterGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl.ActiveTexture(ffi::TEXTURE0);
            for state in &self.states {
                self.gl.BindTexture(ffi::TEXTURE_2D, state.texture);
                self.gl
                    .TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, state.min_filter);
                self.gl
                    .TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, state.mag_filter);
                self.gl
                    .TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, state.wrap_s);
                self.gl
                    .TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, state.wrap_t);
            }
            self.gl
                .BindTexture(ffi::TEXTURE_2D, self.texture_binding as _);
            self.gl.ActiveTexture(self.active_texture as _);
        }
    }
}

impl Drop for BlurBudgetReservation {
    fn drop(&mut self) {
        RETAINED_BLUR_TEXTURE_BYTES.fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BlurTrace {
    pub(crate) surface_id: Option<u32>,
    pub(crate) namespace: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct BlurOutput {
    pub(crate) texture: GlesTexture,
    pub(crate) active_size: Size<i32, Buffer>,
    _budget: Option<Arc<BlurBudgetReservation>>,
}

impl BlurOutput {
    pub(crate) fn from_full_texture(texture: GlesTexture) -> Self {
        Self {
            active_size: texture.size(),
            texture,
            _budget: None,
        }
    }

    pub(crate) fn source_rect(&self) -> Rectangle<f64, Buffer> {
        active_texture_rect(self.active_size, self.texture.size())
    }

    pub(crate) fn map_source_rect(&self, source: Rectangle<f64, Buffer>) -> Rectangle<f64, Buffer> {
        // Blur passes write to the active pixels at the origin of the capacity
        // texture. Capacity is retained storage, not an extra coordinate space.
        source
            .intersection(self.source_rect())
            .unwrap_or_else(|| Rectangle::from_size(Size::default()))
    }
}

fn reserve_blur_bytes(bytes: usize) -> anyhow::Result<Arc<BlurBudgetReservation>> {
    let mut current = RETAINED_BLUR_TEXTURE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(bytes)
            .context("blur texture byte count overflow")?;
        ensure!(
            next <= MAX_GLOBAL_BLUR_TEXTURE_BYTES,
            "blur texture global byte budget exceeded: requested {bytes} bytes with {current} retained"
        );
        match RETAINED_BLUR_TEXTURE_BYTES.compare_exchange_weak(
            current,
            next,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(Arc::new(BlurBudgetReservation { bytes })),
            Err(observed) => current = observed,
        }
    }
}

/// `NIRI_DISABLE_ANIM_BLUR_DOWNSAMPLE=1` disables the animation-period blur
/// downsample tier for A/B measurement, mirroring the `NIRI_LIFECYCLE_DIAG`
/// diagnostics idiom. Checked once per process.
pub fn anim_downsample_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var_os("NIRI_DISABLE_ANIM_BLUR_DOWNSAMPLE")
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// `NIRI_DISABLE_FAST_MOTION_DOWNSAMPLE=1` disables the T-32 fast-motion
/// downsample tier for A/B measurement (same idiom as the P06 switch).
pub fn fast_motion_downsample_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var_os("NIRI_DISABLE_FAST_MOTION_DOWNSAMPLE")
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// `NIRI_DISABLE_BAND_QUANTIZATION=1` disables the T-32 8px capture-band
/// quantization for A/B measurement (strict superset capture stays off).
pub fn band_quantization_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var_os("NIRI_DISABLE_BAND_QUANTIZATION")
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}

impl From<niri_config::Blur> for BlurOptions {
    fn from(config: niri_config::Blur) -> Self {
        Self {
            passes: config.passes,
            offset: config.offset,
            downsample_shift: 0,
        }
    }
}

impl BlurOptions {
    fn shift(&self) -> u8 {
        self.downsample_shift.min(MAX_DOWNSAMPLE_SHIFT)
    }

    /// Pass count actually run: one pass is traded per downsample step so the
    /// effective blur radius matches the static tier.
    pub fn effective_passes(&self) -> u8 {
        self.passes.clamp(1, 31).saturating_sub(self.shift()).max(1)
    }

    /// Shrink a capture texture size by the downsample tier. The capture blit
    /// into the smaller texture performs the actual downsampling for free.
    pub fn capture_size(&self, size: Size<i32, Buffer>) -> Size<i32, Buffer> {
        let shift = self.shift();
        if shift == 0 {
            return size;
        }
        Size::new(max(1, size.w >> shift), max(1, size.h >> shift))
    }
}

fn texture_sizes(size: Size<i32, Buffer>, passes: u8) -> impl Iterator<Item = Size<i32, Buffer>> {
    successors(Some(size), |size| {
        Some(Size::new(max(1, size.w / 2), max(1, size.h / 2)))
    })
    .take(passes.clamp(1, 31) as usize + 1)
}

fn round_up_to_bucket(value: i32) -> i32 {
    let value = value.max(1);
    if value > MAX_REUSABLE_TEXTURE_DIMENSION {
        return value;
    }

    let bucket =
        value.saturating_add(CAPACITY_BUCKET_PX - 1) / CAPACITY_BUCKET_PX * CAPACITY_BUCKET_PX;
    if bucket <= MAX_REUSABLE_TEXTURE_DIMENSION {
        bucket
    } else {
        value
    }
}

fn capacity_size_for(size: Size<i32, Buffer>) -> Size<i32, Buffer> {
    Size::from((round_up_to_bucket(size.w), round_up_to_bucket(size.h)))
}

fn texture_bytes(size: Size<i32, Buffer>) -> usize {
    (size.w.max(0) as usize)
        .saturating_mul(size.h.max(0) as usize)
        .saturating_mul(4)
}

fn pyramid_bytes(sizes: &[Size<i32, Buffer>]) -> usize {
    sizes
        .iter()
        .map(|size| texture_bytes(*size))
        .fold(0usize, usize::saturating_add)
}

fn pyramid_capacity_sizes(size: Size<i32, Buffer>, passes: u8) -> Vec<Size<i32, Buffer>> {
    let active: Vec<_> = texture_sizes(size, passes).collect();
    let bucketed: Vec<_> = active.iter().copied().map(capacity_size_for).collect();

    // Avoid retaining bucket slack when the pyramid would exceed the per-pyramid
    // cap. The exact plan is checked as a hard limit by prepare_textures().
    if pyramid_bytes(&bucketed) <= MAX_RETAINED_PYRAMID_BYTES {
        bucketed
    } else {
        active
    }
}

fn active_texture_rect(
    active_size: Size<i32, Buffer>,
    _capacity_size: Size<i32, Buffer>,
) -> Rectangle<f64, Buffer> {
    Rectangle::from_size(active_size.to_f64())
}

fn shrink_ready(streak: u8) -> bool {
    streak >= SHRINK_HYSTERESIS_PREPARES
}

fn blur_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("NIRI_BLUR_TRACE").is_some_and(|value| !value.is_empty() && value != "0")
    })
}

#[derive(Debug, Clone)]
pub struct BlurProgram(Rc<BlurProgramInner>);

#[derive(Debug)]
struct BlurProgramInner {
    down: BlurProgramInternal,
    up: BlurProgramInternal,
}

#[derive(Debug)]
struct BlurProgramInternal {
    program: ffi::types::GLuint,
    uniform_tex: ffi::types::GLint,
    uniform_tex_scale: ffi::types::GLint,
    uniform_tex_bounds: ffi::types::GLint,
    uniform_half_pixel: ffi::types::GLint,
    uniform_offset: ffi::types::GLint,
    attrib_vert: ffi::types::GLint,
}

unsafe fn compile_program(gl: &ffi::Gles2, src: &str) -> Result<BlurProgramInternal, GlesError> {
    let program = unsafe { link_program(gl, include_str!("shaders/blur.vert"), src)? };

    let vert = c"vert";
    let tex = c"tex";
    let tex_scale = c"tex_scale";
    let tex_bounds = c"tex_bounds";
    let half_pixel = c"half_pixel";
    let offset = c"offset";

    Ok(BlurProgramInternal {
        program,
        uniform_tex: gl.GetUniformLocation(program, tex.as_ptr()),
        uniform_tex_scale: gl.GetUniformLocation(program, tex_scale.as_ptr()),
        uniform_tex_bounds: gl.GetUniformLocation(program, tex_bounds.as_ptr()),
        uniform_half_pixel: gl.GetUniformLocation(program, half_pixel.as_ptr()),
        uniform_offset: gl.GetUniformLocation(program, offset.as_ptr()),
        attrib_vert: gl.GetAttribLocation(program, vert.as_ptr()),
    })
}

impl BlurProgram {
    pub fn compile(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        renderer
            .with_context(move |gl| unsafe {
                let down = compile_program(gl, include_str!("shaders/blur_down.frag"))
                    .context("error compiling blur_down shader")?;
                let up = compile_program(gl, include_str!("shaders/blur_up.frag"))
                    .context("error compiling blur_up shader")?;
                Ok(Self(Rc::new(BlurProgramInner { down, up })))
            })
            .context("error making GL context current")?
    }

    pub fn destroy(self, renderer: &mut GlesRenderer) -> Result<(), GlesError> {
        renderer.with_context(move |gl| unsafe {
            gl.DeleteProgram(self.0.down.program);
            gl.DeleteProgram(self.0.up.program);
        })
    }
}

impl Blur {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let program = Shaders::get(renderer).blur.clone()?;
        Some(Self {
            program,
            renderer_context_id: renderer.context_id(),
            textures: Vec::new(),
            texture_budgets: Vec::new(),
            shrink_streak: 0,
            prepared_passes: None,
        })
    }

    pub fn context_id(&self) -> ContextId<GlesTexture> {
        self.renderer_context_id.clone()
    }

    pub(crate) fn prepare_textures(
        &mut self,
        mut create_texture: impl FnMut(Fourcc, Size<i32, Buffer>) -> Result<GlesTexture, GlesError>,
        source: &GlesTexture,
        options: BlurOptions,
        trace_context: BlurTrace,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Blur::prepare_textures");

        let passes = options.effective_passes() as usize;
        let size = source.size();
        let pass_count = options.effective_passes();
        let pass_count_changed = self.prepared_passes != Some(pass_count);
        if pass_count_changed {
            self.shrink_streak = 0;
        }
        let active_sizes: Vec<_> = texture_sizes(size, options.effective_passes()).collect();
        let capacity_sizes = pyramid_capacity_sizes(size, options.effective_passes());
        let capacity_bytes = pyramid_bytes(&capacity_sizes);
        ensure!(
            capacity_bytes <= MAX_RETAINED_PYRAMID_BYTES,
            "blur pyramid exceeds the per-pyramid hard budget: {capacity_bytes} bytes"
        );
        let needs_shrink = self
            .textures
            .iter()
            .zip(&capacity_sizes)
            .any(|(texture, target)| texture.size().w > target.w || texture.size().h > target.h);
        if needs_shrink {
            self.shrink_streak = self.shrink_streak.saturating_add(1);
        } else {
            self.shrink_streak = 0;
        }
        let apply_shrink = shrink_ready(self.shrink_streak);
        let mut reallocated = pass_count_changed;
        let mut reason = if pass_count_changed {
            "pass_count_changed"
        } else {
            "reuse"
        };

        // Build a candidate pyramid before replacing the committed one. This
        // keeps the previous output available when a budget or GL allocation
        // fails during a pass/format/context change.
        let mut candidate = Vec::with_capacity(passes + 1);
        let mut candidate_budgets = Vec::with_capacity(passes + 1);
        let mut can_reuse_prefix = !pass_count_changed;

        // Reuse a capacity-compatible texture pyramid while format, pass count,
        // context and output ownership stay compatible.
        for (i, (active_size, capacity_size)) in active_sizes
            .iter()
            .copied()
            .zip(capacity_sizes.iter().copied())
            .enumerate()
        {
            let mut reused_texture = None;
            if let Some(texture) = self.textures.get_mut(i) {
                let actual_size = texture.size();
                let actual_format = texture.format();
                let format_changed = actual_format != Some(Fourcc::Abgr8888);
                let output_is_shared = i == 0 && !texture.is_unique_reference();
                let capacity_insufficient =
                    actual_size.w < active_size.w || actual_size.h < active_size.h;
                let shrink_required = apply_shrink && actual_size != capacity_size;
                let reusable = can_reuse_prefix
                    && !format_changed
                    && !output_is_shared
                    && !capacity_insufficient
                    && !shrink_required;
                if reusable {
                    reused_texture = Some(texture.clone());
                } else {
                    reallocated = true;
                    if reason == "reuse" {
                        reason = if format_changed {
                            "format_changed"
                        } else if output_is_shared {
                            "shared_reference"
                        } else if capacity_insufficient {
                            "capacity_insufficient"
                        } else {
                            "shrink_hysteresis"
                        };
                    }
                    debug!(
                        step = i,
                        requested = ?active_size,
                        capacity = ?capacity_size,
                        actual = ?actual_size,
                        format_changed,
                        output_is_shared,
                        "recreating blur texture capacity"
                    );
                    can_reuse_prefix = false;
                }
            }

            if let Some(texture) = reused_texture {
                candidate.push(texture);
                candidate_budgets.push(
                    self.texture_budgets
                        .get(i)
                        .cloned()
                        .context("missing blur texture budget for reusable level")?,
                );
                continue;
            }

            let budget = match reserve_blur_bytes(texture_bytes(capacity_size)) {
                Ok(budget) => budget,
                Err(err) => {
                    crate::utils::lifecycle_diag::note_blur_budget_reservation_failure();
                    return Err(err).context("error reserving blur texture budget");
                }
            };
            let texture: GlesTexture = match create_texture(Fourcc::Abgr8888, capacity_size) {
                Ok(texture) => texture,
                Err(err) => {
                    crate::utils::lifecycle_diag::note_blur_gpu_error();
                    return Err(err).context("error creating texture");
                }
            };
            crate::utils::lifecycle_diag::note_blur_texture_allocation(
                texture_bytes(capacity_size) as u64,
            );
            candidate.push(texture);
            candidate_budgets.push(budget);
        }

        self.textures = candidate;
        self.texture_budgets = candidate_budgets;
        self.prepared_passes = Some(pass_count);

        let trace_streak = self.shrink_streak;
        if apply_shrink {
            self.shrink_streak = 0;
        }
        if !reallocated {
            crate::utils::lifecycle_diag::note_blur_texture_reuse();
        }
        if blur_trace_enabled() {
            debug!(
                surface_id = ?trace_context.surface_id,
                namespace = ?trace_context.namespace,
                requested = ?size,
                capacity = ?capacity_sizes.first(),
                format = ?Some(Fourcc::Abgr8888),
                passes,
                operation = if reallocated { "reallocate" } else { "reuse" },
                reason,
                shrink_streak = trace_streak,
                "blur texture capacity trace"
            );
        }

        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        source: &GlesTexture,
        options: BlurOptions,
    ) -> anyhow::Result<BlurOutput> {
        let _span = tracy_client::span!("Blur::render");
        crate::utils::lifecycle_diag::note_blur_render();
        trace!("rendering blur");

        ensure!(
            renderer.context_id() == self.renderer_context_id,
            "wrong renderer"
        );

        let passes = options.effective_passes() as usize;
        let size = source.size();
        let active_sizes: Vec<_> = texture_sizes(size, options.effective_passes()).collect();

        ensure!(
            self.textures.len() == passes + 1,
            "wrong textures len: expected {}, got {}",
            passes + 1,
            self.textures.len()
        );

        let output = &mut self.textures[0];
        ensure!(
            output.size().w >= size.w && output.size().h >= size.h,
            "wrong output texture capacity: expected at least {size:?}, got {:?}",
            output.size()
        );

        ensure!(
            output.is_unique_reference(),
            "output texture has a non-unique reference"
        );

        let scratch_framebuffer = ScratchFramebuffer::for_renderer(renderer);
        renderer.with_profiled_context(gpu_span_location!("Blur::render"), |gl| unsafe {
            while gl.GetError() != ffi::NO_ERROR {}

            let _state = GlStateGuard::new(
                gl,
                [
                    self.program.0.down.attrib_vert,
                    self.program.0.up.attrib_vert,
                ],
            );

            gl.Disable(ffi::BLEND);
            gl.Disable(ffi::SCISSOR_TEST);

            gl.ActiveTexture(ffi::TEXTURE0);

            let texture_ids = once(source.tex_id())
                .chain(self.textures.iter().map(GlesTexture::tex_id))
                .collect::<Vec<_>>();
            let _texture_params = TextureParameterGuard::new(gl, texture_ids);

            let framebuffer = scratch_framebuffer.get_or_create(gl);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, framebuffer);

            let program = &self.program.0.down;
            gl.UseProgram(program.program);
            gl.Uniform1i(program.uniform_tex, 0);
            gl.Uniform1f(program.uniform_offset, options.offset as f32);

            let vertices: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];
            gl.EnableVertexAttribArray(program.attrib_vert as u32);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.VertexAttribPointer(
                program.attrib_vert as u32,
                2,
                ffi::FLOAT,
                ffi::FALSE,
                0,
                vertices.as_ptr().cast(),
            );

            for level in 0..passes {
                let src = if level == 0 {
                    source.clone()
                } else {
                    self.textures[level].clone()
                };
                let src_active = active_sizes[level];
                let dst_active = active_sizes[level + 1];
                let dst = &mut self.textures[level + 1];
                ensure!(
                    dst.size().w >= dst_active.w && dst.size().h >= dst_active.h,
                    "downsample destination is smaller than active size"
                );
                let w = dst_active.w;
                let h = dst_active.h;
                gl.Viewport(0, 0, w, h);

                // During downsampling, half_pixel is half of the destination pixel.
                gl.Uniform2f(program.uniform_half_pixel, 0.5 / w as f32, 0.5 / h as f32);
                gl.Uniform2f(
                    program.uniform_tex_scale,
                    src_active.w as f32 / src.size().w as f32,
                    src_active.h as f32 / src.size().h as f32,
                );
                gl.Uniform4f(
                    program.uniform_tex_bounds,
                    0.5 / src.size().w as f32,
                    0.5 / src.size().h as f32,
                    (src_active.w as f32 - 0.5) / src.size().w as f32,
                    (src_active.h as f32 - 0.5) / src.size().h as f32,
                );

                let src = src.tex_id();
                let dst = dst.tex_id();

                trace!("drawing down {src} to {dst}");
                gl.FramebufferTexture2D(
                    ffi::FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    dst,
                    0,
                );

                gl.BindTexture(ffi::TEXTURE_2D, src);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );

                gl.DrawArrays(ffi::TRIANGLES, 0, 6);
            }

            gl.DisableVertexAttribArray(program.attrib_vert as u32);

            // Up
            let program = &self.program.0.up;
            gl.UseProgram(program.program);
            gl.Uniform1i(program.uniform_tex, 0);
            gl.Uniform1f(program.uniform_offset, options.offset as f32);

            let vertices: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];
            gl.EnableVertexAttribArray(program.attrib_vert as u32);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.VertexAttribPointer(
                program.attrib_vert as u32,
                2,
                ffi::FLOAT,
                ffi::FALSE,
                0,
                vertices.as_ptr().cast(),
            );

            for level in (1..=passes).rev() {
                let src = self.textures[level].clone();
                let src_active = active_sizes[level];
                let dst_active = active_sizes[level - 1];
                let dst = &mut self.textures[level - 1];
                ensure!(
                    dst.size().w >= dst_active.w && dst.size().h >= dst_active.h,
                    "upsample destination is smaller than active size"
                );
                let w = dst_active.w;
                let h = dst_active.h;
                gl.Viewport(0, 0, w, h);

                // During upsampling, half_pixel is half of the source pixel.
                let src_w = src_active.w as f32;
                let src_h = src_active.h as f32;
                gl.Uniform2f(program.uniform_half_pixel, 0.5 / src_w, 0.5 / src_h);
                gl.Uniform2f(
                    program.uniform_tex_scale,
                    src_active.w as f32 / src.size().w as f32,
                    src_active.h as f32 / src.size().h as f32,
                );
                gl.Uniform4f(
                    program.uniform_tex_bounds,
                    0.5 / src.size().w as f32,
                    0.5 / src.size().h as f32,
                    (src_active.w as f32 - 0.5) / src.size().w as f32,
                    (src_active.h as f32 - 0.5) / src.size().h as f32,
                );

                let src = src.tex_id();
                let dst = dst.tex_id();

                trace!("drawing up {src} to {dst}");
                gl.FramebufferTexture2D(
                    ffi::FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    dst,
                    0,
                );

                gl.BindTexture(ffi::TEXTURE_2D, src);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );

                gl.DrawArrays(ffi::TRIANGLES, 0, 6);
            }

            gl.DisableVertexAttribArray(program.attrib_vert as u32);

            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                0,
                0,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, framebuffer);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                0,
                0,
            );
            if gl.GetError() != ffi::NO_ERROR {
                crate::utils::lifecycle_diag::note_blur_gpu_error();
                anyhow::bail!("blur pass reported an OpenGL error");
            }
            Ok(())
        })??;

        Ok(BlurOutput {
            texture: self.textures[0].clone(),
            active_size: active_sizes[0],
            _budget: self.texture_budgets.first().cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::Size;

    use super::{
        active_texture_rect, capacity_size_for, pyramid_bytes, pyramid_capacity_sizes,
        reserve_blur_bytes, shrink_ready, texture_sizes, BlurOptions, ANIM_DOWNSAMPLE_SHIFT,
        MAX_GLOBAL_BLUR_TEXTURE_BYTES, MAX_RETAINED_PYRAMID_BYTES,
    };

    #[test]
    fn anim_tier_trades_one_pass_per_downsample_step() {
        let base = BlurOptions {
            passes: 3,
            offset: 4.,
            downsample_shift: 0,
        };
        assert_eq!(base.effective_passes(), 3);

        let tier = BlurOptions {
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
            ..base
        };
        assert_eq!(tier.effective_passes(), 2);

        // Never below one pass, even for minimal kernels.
        let minimal = BlurOptions {
            passes: 1,
            offset: 4.,
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
        };
        assert_eq!(minimal.effective_passes(), 1);

        // passes = 0 clamps up to one pass first.
        let zero = BlurOptions {
            passes: 0,
            offset: 4.,
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
        };
        assert_eq!(zero.effective_passes(), 1);
    }

    #[test]
    fn capture_size_halves_per_step_and_never_hits_zero() {
        let base = BlurOptions {
            passes: 3,
            offset: 4.,
            downsample_shift: 0,
        };
        assert_eq!(
            base.capture_size(Size::from((1920, 1080))),
            Size::from((1920, 1080))
        );

        let tier = BlurOptions {
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
            ..base
        };
        assert_eq!(
            tier.capture_size(Size::from((1920, 1080))),
            Size::from((960, 540))
        );
        assert_eq!(tier.capture_size(Size::from((1, 1))), Size::from((1, 1)));

        // Excessive programmatic shifts are clamped, not amplified.
        let wild = BlurOptions {
            downsample_shift: 200,
            ..base
        };
        assert_eq!(
            wild.capture_size(Size::from((1920, 1080))),
            Size::from((120, 67))
        );
    }

    /// Radius parity between tiers, structurally: the deepest pyramid level
    /// (which sets the coarsest blur octave and thus the perceived radius)
    /// must be identical between the static tier and the animation tier.
    #[test]
    fn anim_tier_keeps_deepest_pyramid_level() {
        let base = BlurOptions {
            passes: 3,
            offset: 4.,
            downsample_shift: 0,
        };
        let tier = BlurOptions {
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
            ..base
        };

        let source = Size::from((1920, 1080));
        let full: Vec<_> = texture_sizes(source, base.effective_passes()).collect();
        let low: Vec<_> =
            texture_sizes(tier.capture_size(source), tier.effective_passes()).collect();

        assert_eq!(full.last(), Some(&Size::from((240, 135))));
        assert_eq!(low.last(), Some(&Size::from((240, 135))));
        assert_eq!(low.len() + 1, full.len());
    }

    /// The animation tier must process roughly a quarter of the pyramid
    /// pixels (down + up pass writes), before even counting the smaller blit.
    #[test]
    fn anim_tier_processes_about_a_quarter_of_the_pixels() {
        fn pyramid_write_pixels(sizes: &[Size<i32, smithay::utils::Buffer>]) -> i64 {
            let area = |s: &Size<i32, smithay::utils::Buffer>| i64::from(s.w) * i64::from(s.h);
            let down: i64 = sizes[1..].iter().map(area).sum();
            let up: i64 = sizes[..sizes.len() - 1].iter().map(area).sum();
            down + up
        }

        let base = BlurOptions {
            passes: 3,
            offset: 4.,
            downsample_shift: 0,
        };
        let tier = BlurOptions {
            downsample_shift: ANIM_DOWNSAMPLE_SHIFT,
            ..base
        };

        let source = Size::from((1920, 1080));
        let full: Vec<_> = texture_sizes(source, base.effective_passes()).collect();
        let low: Vec<_> =
            texture_sizes(tier.capture_size(source), tier.effective_passes()).collect();

        let full_pixels = pyramid_write_pixels(&full);
        let low_pixels = pyramid_write_pixels(&low);
        assert!(
            low_pixels * 4 <= full_pixels + full_pixels / 10,
            "animation tier must cut pyramid work ~4x: {low_pixels} vs {full_pixels}"
        );
    }

    #[test]
    fn texture_pyramid_tracks_size_and_pass_count() {
        let sizes: Vec<_> = texture_sizes(Size::from((1920, 1080)), 4).collect();

        assert_eq!(
            sizes,
            vec![
                Size::from((1920, 1080)),
                Size::from((960, 540)),
                Size::from((480, 270)),
                Size::from((240, 135)),
                Size::from((120, 67)),
            ]
        );
    }

    #[test]
    fn texture_pyramid_clamps_passes_and_dimensions() {
        assert_eq!(texture_sizes(Size::from((1, 1)), 0).count(), 2);

        let sizes: Vec<_> = texture_sizes(Size::from((2, 3)), u8::MAX).collect();
        assert_eq!(sizes.len(), 32);
        assert_eq!(sizes.last(), Some(&Size::from((1, 1))));
    }

    #[test]
    fn capacity_bucket_reuses_small_resize_steps() {
        let one = capacity_size_for(Size::from((1, 1)));
        assert_eq!(one, Size::from((64, 64)));
        assert_eq!(capacity_size_for(Size::from((2, 8))), one);
        assert_eq!(capacity_size_for(Size::from((64, 64))), one);
        assert_eq!(
            capacity_size_for(Size::from((65, 64))),
            Size::from((128, 64))
        );
    }

    #[test]
    fn resize_sequences_only_cross_bucket_boundaries() {
        let one_px: Vec<_> = (1..=512)
            .map(|width| capacity_size_for(Size::from((width, 1080))))
            .collect();
        let one_px_changes = one_px
            .windows(2)
            .filter(|sizes| sizes[0] != sizes[1])
            .count();
        assert_eq!(one_px_changes, 7);

        let two_px: Vec<_> = (1..=512)
            .step_by(2)
            .map(|width| capacity_size_for(Size::from((width, 1080))))
            .collect();
        let eight_px: Vec<_> = (1..=512)
            .step_by(8)
            .map(|width| capacity_size_for(Size::from((width, 1080))))
            .collect();
        assert_eq!(
            two_px
                .windows(2)
                .filter(|sizes| sizes[0] != sizes[1])
                .count(),
            7
        );
        assert_eq!(
            eight_px
                .windows(2)
                .filter(|sizes| sizes[0] != sizes[1])
                .count(),
            7
        );
    }

    #[test]
    fn capacity_plan_preserves_active_sizes_and_budget() {
        let active: Vec<_> = texture_sizes(Size::from((1920, 1080)), 3).collect();
        let capacity = pyramid_capacity_sizes(Size::from((1920, 1080)), 3);

        assert_eq!(capacity.len(), active.len());
        assert!(capacity
            .iter()
            .zip(&active)
            .all(|(capacity, active)| capacity.w >= active.w && capacity.h >= active.h));
        assert!(capacity
            .windows(2)
            .all(|sizes| { sizes[0].w >= sizes[1].w && sizes[0].h >= sizes[1].h }));
        assert!(pyramid_bytes(&capacity) <= MAX_RETAINED_PYRAMID_BYTES);

        let oversized = pyramid_capacity_sizes(Size::from((8192, 8192)), 3);
        assert_eq!(oversized, active_sizes(Size::from((8192, 8192)), 3));
    }

    #[test]
    fn active_rect_excludes_unused_capacity() {
        let rect = active_texture_rect(Size::from((8, 4)), Size::from((64, 32)));
        assert_eq!(rect.loc, (0., 0.).into());
        assert_eq!(rect.size, (8., 4.).into());

        let visible = smithay::utils::Rectangle::new(
            smithay::utils::Point::from((2., 1.)),
            Size::from((4., 2.)),
        );
        assert_eq!(visible.intersection(rect), Some(visible));
        let outside = smithay::utils::Rectangle::new(
            smithay::utils::Point::from((8., 0.)),
            Size::from((1., 1.)),
        );
        assert!(outside.intersection(rect).is_none());
    }

    #[test]
    fn shrink_hysteresis_requires_sustained_pressure() {
        assert!(!shrink_ready(0));
        assert!(!shrink_ready(7));
        assert!(shrink_ready(8));
        assert!(shrink_ready(u8::MAX));
    }

    #[test]
    fn global_budget_rejects_an_over_budget_reservation_without_mutation() {
        assert!(reserve_blur_bytes(MAX_GLOBAL_BLUR_TEXTURE_BYTES + 1).is_err());
        assert!(reserve_blur_bytes(usize::MAX).is_err());
    }

    #[test]
    fn budget_reservation_releases_its_charge() {
        let _lock = super::BLUR_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = super::RETAINED_BLUR_TEXTURE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        let reservation = reserve_blur_bytes(4096).expect("small reservation must fit");
        assert_eq!(
            super::RETAINED_BLUR_TEXTURE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            before + 4096
        );
        drop(reservation);
        assert_eq!(
            super::RETAINED_BLUR_TEXTURE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    fn active_sizes(
        size: Size<i32, smithay::utils::Buffer>,
        passes: u8,
    ) -> Vec<Size<i32, smithay::utils::Buffer>> {
        texture_sizes(size, passes).collect()
    }
}
