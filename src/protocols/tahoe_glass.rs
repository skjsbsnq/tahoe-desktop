#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use niri_config::CornerRadius;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{with_states, SurfaceData};

use super::raw::tahoe_glass::v1::server::tahoe_glass_manager_v1::{self, TahoeGlassManagerV1};
use super::raw::tahoe_glass::v1::server::tahoe_glass_surface_v1::{self, TahoeGlassSurfaceV1};
use crate::niri::State;
use crate::utils::surface_geo;

// Version of the *manager* interface global. Kept in lockstep with the
// tahoe_glass_surface_v1 interface version (both 4 in the XML) because surface
// objects inherit the manager's bound version: version negotiation for the
// since="4" presentation-transform requests only works when the manager global
// advertises the same number. wayland-backend rejects a global version above
// the manager interface's XML version, so both must be bumped together.
const VERSION: u32 = 4;
pub const MAX_REGIONS_PER_SURFACE: usize = 32;

/// Presentation transform of a glass surface in surface-local logical
/// coordinates: a surface point `p` renders at
/// `surface_position + (x, y) + (scale_x * p.x, scale_y * p.y)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresentationAffine {
    pub x: f64,
    pub y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
}

impl PresentationAffine {
    pub const IDENTITY: Self = Self {
        x: 0.,
        y: 0.,
        scale_x: 1.,
        scale_y: 1.,
    };

    pub fn is_identity(&self) -> bool {
        self.x.abs() < 1e-4
            && self.y.abs() < 1e-4
            && (self.scale_x - 1.).abs() < 1e-4
            && (self.scale_y - 1.).abs() < 1e-4
    }

    /// Map a surface-local rectangle through this affine.
    pub fn apply_rect(&self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::new(
                self.x + self.scale_x * rect.loc.x,
                self.y + self.scale_y * rect.loc.y,
            ),
            Size::new(self.scale_x * rect.size.w, self.scale_y * rect.size.h),
        )
    }

    /// The affine mapping `from` onto `to` (both surface-local rectangles).
    ///
    /// Returns `None` when `from` has a degenerate side.
    pub fn mapping_rect(
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
    ) -> Option<Self> {
        if from.size.w <= f64::EPSILON || from.size.h <= f64::EPSILON {
            return None;
        }

        let scale_x = to.size.w / from.size.w;
        let scale_y = to.size.h / from.size.h;
        Some(Self {
            x: to.loc.x - scale_x * from.loc.x,
            y: to.loc.y - scale_y * from.loc.y,
            scale_x,
            scale_y,
        })
    }

    fn sanitized(x: f64, y: f64, scale_x: f64, scale_y: f64) -> Self {
        Self {
            x: x.clamp(-16384., 16384.),
            y: y.clamp(-16384., 16384.),
            scale_x: scale_x.clamp(0.05, 20.),
            scale_y: scale_y.clamp(0.05, 20.),
        }
    }
}

/// Animation curve carried by `set_transform_target` / `set_region_morph`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TahoeTransformCurve {
    Spring {
        damping_ratio: f64,
        stiffness: f64,
        /// Progress-space settle epsilon.
        epsilon: f64,
    },
    Eased {
        duration_ms: u32,
        /// Cubic-bezier control points (x1, y1, x2, y2).
        bezier: (f64, f64, f64, f64),
    },
}

/// A transform request as received on the wire, pending until commit.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PendingTransformRequest {
    Set(PresentationAffine),
    Target(PresentationAffine, TahoeTransformCurve),
    RegionMorph(u32, TahoeTransformCurve),
}

/// A committed transform directive for the layer machinery to consume.
///
/// Published by [`on_surface_commit`] (or by controller destroy, which resets
/// to identity) together with a monotonically increasing epoch. Consumers
/// compare epochs and act once per directive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TahoeGlassTransformDirective {
    /// Jump the presentation transform to this value, cancelling animations.
    Set(PresentationAffine),
    /// Animate from the current presentation transform to this target.
    Target(PresentationAffine, TahoeTransformCurve),
    /// Container morph anchored on a region geometry change: animate from the
    /// affine mapping `new_rect` onto the pre-commit visual footprint of
    /// `old_rect` back to identity.
    ///
    /// Both rects are surface-local; visual continuity therefore assumes the
    /// layer surface's own position/size do not change in the same commit
    /// (true for the fixed-size overlay/dock surfaces this serves). A
    /// concurrent surface move shifts the start footprint by the same delta.
    Morph {
        old_rect: Rectangle<f64, Logical>,
        new_rect: Rectangle<f64, Logical>,
        curve: TahoeTransformCurve,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TahoeGlassFlags {
    pub blur: bool,
    pub shadow: bool,
    pub clip: bool,
}

impl TahoeGlassFlags {
    fn from_bits(bits: u32) -> Self {
        Self {
            blur: bits & 1 != 0,
            shadow: bits & 2 != 0,
            clip: bits & 4 != 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TahoeGlassRegion {
    pub id: u32,
    pub rect: Rectangle<i32, Logical>,
    pub radius: CornerRadius,
    pub material: String,
    pub flags: TahoeGlassFlags,
    /// Per-region interaction scalar in [0, 1] that drives compositor-side
    /// material easing (higher highlight/refraction/inner shadow). 0 = at rest.
    pub interaction: f32,
    /// Per-region material alpha in [0, 1] for compositor-side enter/exit fades.
    /// 1 = fully visible; 0 = material parameters faded out.
    pub material_alpha: f32,
}

pub struct TahoeGlassSurfaceUserData {
    surface: WlSurface,
    /// Matches [`TahoeGlassSurfaceInner::controller_generation`] while this
    /// controller owns the surface glass state. Used so a late destroy of an
    /// old controller cannot clear a newer controller's state.
    controller_generation: u64,
}

#[derive(Default)]
struct TahoeGlassSurfaceData(Mutex<TahoeGlassSurfaceInner>);

#[derive(Default)]
struct TahoeGlassSurfaceInner {
    pending: Vec<TahoeGlassRegion>,
    committed: Arc<Vec<TahoeGlassRegion>>,
    pending_dirty: bool,
    /// Monotonic owner token for the active `tahoe_glass_surface_v1`.
    /// Only the controller with this generation may clear surface state.
    controller_generation: u64,
    /// Wire transform request pending until the next commit. Last one wins.
    pending_transform: Option<PendingTransformRequest>,
    /// Latest committed transform directive, tagged with a monotonically
    /// increasing epoch so the layer machinery can consume it exactly once.
    transform_directive: Option<TahoeGlassTransformDirective>,
    transform_epoch: u64,
}

impl TahoeGlassSurfaceInner {
    fn is_owner(&self, generation: u64) -> bool {
        self.controller_generation == generation
    }

    /// Claim ownership for a newly created controller.
    ///
    /// Advances the generation and clears any glass state left by a previous
    /// controller so recreate never inherits pending/committed regions.
    /// Returns `(generation, previous_committed)` — the previous committed list
    /// is always returned so the caller can damage old visible glass when
    /// non-empty.
    fn claim_controller(&mut self) -> (u64, Arc<Vec<TahoeGlassRegion>>) {
        self.controller_generation = self.controller_generation.wrapping_add(1);
        self.pending.clear();
        self.pending_dirty = false;
        self.reset_transform();
        let old = std::mem::replace(&mut self.committed, Arc::new(Vec::new()));
        (self.controller_generation, old)
    }

    /// If `generation` still owns this surface state, clear pending and
    /// committed glass regions. Returns the previous committed regions when
    /// this controller was authorized to clear (including when they were
    /// already empty), or `None` when a newer controller owns the state.
    fn clear_if_owner(&mut self, generation: u64) -> Option<Arc<Vec<TahoeGlassRegion>>> {
        if !self.is_owner(generation) {
            return None;
        }

        self.pending.clear();
        self.pending_dirty = false;
        self.reset_transform();
        let old = std::mem::replace(&mut self.committed, Arc::new(Vec::new()));
        Some(old)
    }

    /// Drop any pending transform request and publish an identity reset so a
    /// destroyed or replaced controller cannot leave a stale presentation
    /// transform on screen. Idempotent: publishes nothing when no directive
    /// was ever published or the last one is already an identity reset.
    fn reset_transform(&mut self) {
        self.pending_transform = None;
        let reset = TahoeGlassTransformDirective::Set(PresentationAffine::IDENTITY);
        match self.transform_directive {
            None => {}
            Some(directive) if directive == reset => {}
            Some(_) => {
                self.transform_directive = Some(reset);
                self.transform_epoch = self.transform_epoch.wrapping_add(1);
            }
        }
    }

    /// Store a wire transform request through the same ownership gate as
    /// region writes. Stale controllers are silent no-ops.
    fn set_pending_transform_if_owner(
        &mut self,
        generation: u64,
        request: PendingTransformRequest,
    ) -> bool {
        if !self.is_owner(generation) {
            return false;
        }

        self.pending_transform = Some(request);
        true
    }

    /// Publish a committed transform directive with a fresh epoch.
    fn publish_transform_directive(&mut self, directive: TahoeGlassTransformDirective) {
        self.transform_directive = Some(directive);
        self.transform_epoch = self.transform_epoch.wrapping_add(1);
    }

    /// Apply a mutation to pending regions only when `generation` still owns
    /// the surface. This is the single write gate used by set/remove/clear.
    ///
    /// Returns:
    /// - `Some(true)` when pending changed;
    /// - `Some(false)` when ownership matched but nothing changed, or when the controller is stale
    ///   (stale writes are silent no-ops);
    /// - `None` when the mutation itself rejects the request (e.g. region limit).
    fn with_pending_if_owner<R>(
        &mut self,
        generation: u64,
        f: impl FnOnce(&mut Vec<TahoeGlassRegion>) -> Option<R>,
    ) -> Option<R>
    where
        R: Default,
    {
        if !self.is_owner(generation) {
            return Some(R::default());
        }
        f(&mut self.pending)
    }
}

pub struct TahoeGlassManagerState;

pub struct TahoeGlassManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait TahoeGlassHandler {
    /// Called after glass regions were cleared because a controller was
    /// destroyed while its `wl_surface` is still alive. Default is a no-op so
    /// protocol unit tests need not construct a full compositor state.
    fn queue_redraw_for_tahoe_glass_surface(&mut self, _surface: &WlSurface) {}
}

impl TahoeGlassManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<TahoeGlassManagerV1, TahoeGlassManagerGlobalData>,
        D: Dispatch<TahoeGlassManagerV1, ()>,
        D: Dispatch<TahoeGlassSurfaceV1, TahoeGlassSurfaceUserData>,
        D: TahoeGlassHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = TahoeGlassManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, TahoeGlassManagerV1, _>(VERSION, global_data);

        Self
    }
}

pub fn get_committed_regions(states: &SurfaceData) -> Arc<Vec<TahoeGlassRegion>> {
    states
        .data_map
        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default)
        .0
        .lock()
        .unwrap()
        .committed
        .clone()
}

/// Latest committed transform directive with its epoch, if any.
///
/// The epoch increases with every published directive; consumers remember the
/// last epoch they acted on and apply each directive exactly once.
pub fn get_transform_directive(
    states: &SurfaceData,
) -> Option<(u64, TahoeGlassTransformDirective)> {
    let data = states.data_map.get::<TahoeGlassSurfaceData>()?;
    let guard = data.0.lock().unwrap();
    let directive = guard.transform_directive?;
    Some((guard.transform_epoch, directive))
}

/// Drop the published transform directive when a layer surface unmaps.
///
/// A new mapping starts untransformed by definition; clearing here (instead of
/// absorbing the epoch at map time) is what lets a directive carried by the
/// mapping commit itself still be applied — [`on_surface_commit`] runs at the
/// top of `CompositorHandler::commit`, before the layer-shell commit handling,
/// so at `MappedLayer::new` time that directive is already published. Pending
/// (uncommitted) requests are left untouched: they are regular double-buffered
/// protocol state. The epoch is not rewound, so the next published directive
/// is always seen as fresh.
pub fn clear_transform_directive_on_unmap(surface: &WlSurface) {
    if !surface.is_alive() {
        return;
    }

    with_states(surface, |states| {
        let Some(data) = states.data_map.get::<TahoeGlassSurfaceData>() else {
            return;
        };
        data.0.lock().unwrap().transform_directive = None;
    });
}

fn mark_pending_dirty(surface: &WlSurface) {
    with_inner(surface, |inner| inner.pending_dirty = true);
}

/// Run `f` on the surface glass state, creating it on first touch. The state
/// is applied on each commit by [`on_surface_commit`], which the compositor
/// commit handler calls directly — no per-surface hook registration needed.
fn with_inner<R>(surface: &WlSurface, f: impl FnOnce(&mut TahoeGlassSurfaceInner) -> R) -> R {
    with_states(surface, |states| {
        let state = states
            .data_map
            .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
        let mut guard = state.0.lock().unwrap();
        f(&mut guard)
    })
}

/// Apply pending Tahoe glass state (regions + transform requests) riding a
/// surface commit.
///
/// Called from `CompositorHandler::commit` right after
/// `on_commit_buffer_handler`, NOT from a smithay post-commit hook: post-commit
/// hooks run before the buffer handler updates `RendererSurfaceState`, so
/// region validation there would see the *previous* commit's surface geometry.
/// During an animated grow that off-by-one rejects the final commit (buffer
/// caught up, stale geometry still small) and no further commit arrives to
/// heal it — the grown band stays without glass until the next unrelated poke.
/// Running after the buffer handler validates against the geometry that this
/// very commit attached.
///
/// (Sync subsurfaces are the one exception: `on_commit_buffer_handler` skips
/// them and their view refreshes only during the parent's pass, so a glass
/// region on a sync subsurface still validates against the previous geometry
/// — same as before this reordering, self-healing on the next parent commit.
/// Glass panels are layer-shell root surfaces, so this stays theoretical.)
///
/// It must still run before `layer_shell_handle_commit`: a transform directive
/// riding the mapping commit has to be published before `MappedLayer::new`.
pub fn on_surface_commit(state: &mut State, surface: &WlSurface) {
    let (regions_changed, transform_published) = with_states(surface, |states| {
        let Some(data) = states.data_map.get::<TahoeGlassSurfaceData>() else {
            return (false, false);
        };

        let mut guard = data.0.lock().unwrap();
        let mut regions_changed = false;
        // Pre-commit committed list, kept when this commit replaces it
        // so a region morph can resolve its old geometry.
        let mut old_regions: Option<Arc<Vec<TahoeGlassRegion>>> = None;

        if guard.pending_dirty {
            let validated = validate_regions(states, &guard.pending, &guard.committed);
            if let Some((committed, complete)) = validated {
                // Geometry-healable rejections (a region briefly exceeding the
                // not-yet-resized buffer during an animated grow) keep the
                // dirty flag so the commit attaching the caught-up buffer
                // revalidates the same pending set; overflowing entries are
                // clamped to the current surface (or carried over when the
                // clamp itself overflows the budget) so nothing flickers
                // while waiting.
                guard.pending_dirty = !complete;
                if !complete {
                    debug!(
                        surface = %surface.id(),
                        pending_count = guard.pending.len(),
                        committed_count = committed.len(),
                        "Tahoe glass regions exceed current surface geometry; will revalidate"
                    );
                }
                if *guard.committed != committed {
                    debug!(
                        surface = %surface.id(),
                        old_count = guard.committed.len(),
                        new_count = committed.len(),
                        "committed Tahoe glass regions"
                    );

                    let old = guard.committed.clone();
                    crate::render_helpers::tahoe_glass::damage_surface_regions(
                        states,
                        old.as_ref(),
                        &committed,
                    );
                    guard.committed = Arc::new(committed);
                    old_regions = Some(old);
                    regions_changed = true;
                }
            } else {
                debug!(
                    surface = %surface.id(),
                    pending_count = guard.pending.len(),
                    "deferring Tahoe glass region commit until surface geometry is available"
                );
            }
        }

        // A region morph is anchored on the region change riding this commit;
        // when that change was deferred (no surface geometry yet), keep the
        // morph pending too so both land together on the commit that
        // materializes the regions.
        let pending_transform = if guard.pending_dirty
            && matches!(
                guard.pending_transform,
                Some(PendingTransformRequest::RegionMorph(..))
            ) {
            None
        } else {
            guard.pending_transform.take()
        };

        let mut transform_published = false;
        if let Some(request) = pending_transform {
            let directive = match request {
                PendingTransformRequest::Set(affine) => {
                    Some(TahoeGlassTransformDirective::Set(affine))
                }
                PendingTransformRequest::Target(affine, curve) => {
                    Some(TahoeGlassTransformDirective::Target(affine, curve))
                }
                PendingTransformRequest::RegionMorph(id, curve) => {
                    let old_rect = old_regions
                        .as_ref()
                        .map(|old| old.as_slice())
                        .unwrap_or(guard.committed.as_slice())
                        .iter()
                        .find(|region| region.id == id)
                        .map(|region| region.rect);
                    let new_rect = guard
                        .committed
                        .iter()
                        .find(|region| region.id == id)
                        .map(|region| region.rect);
                    match (old_rect, new_rect) {
                        (Some(old), Some(new)) if old != new => {
                            Some(TahoeGlassTransformDirective::Morph {
                                old_rect: old.to_f64(),
                                new_rect: new.to_f64(),
                                curve,
                            })
                        }
                        _ => {
                            debug!(
                                surface = %surface.id(),
                                region_id = id,
                                "discarding Tahoe glass region morph without a geometry change"
                            );
                            None
                        }
                    }
                }
            };

            if let Some(directive) = directive {
                debug!(
                    surface = %surface.id(),
                    ?directive,
                    "committed Tahoe glass transform directive"
                );
                guard.publish_transform_directive(directive);
                transform_published = true;
            }
        }

        (regions_changed, transform_published)
    });

    if regions_changed {
        crate::utils::lifecycle_diag::note_tahoe_region_commit();
    }
    if regions_changed || transform_published {
        // R14: sole server lifecycle redraw owner (same as destroy/recreate).
        // Targeted output when locatable; fallback all only when not.
        state.queue_redraw_for_tahoe_glass_surface(surface);
    }
}

/// Clear pending/committed glass state owned by `generation` on surface data.
/// Returns whether committed regions were non-empty before clear (caller should
/// queue a redraw so the old glass disappears).
///
/// Idempotent for the same generation. A newer controller's generation is a
/// no-op so old destroy callbacks cannot wipe the new owner.
///
/// Separated from the `WlSurface` wrapper so unit tests can exercise the same
/// path Dispatch uses without a full Wayland client round-trip.
fn clear_surface_data_if_owner(states: &SurfaceData, generation: u64) -> bool {
    let Some(data) = states.data_map.get::<TahoeGlassSurfaceData>() else {
        return false;
    };

    let mut guard = data.0.lock().unwrap();
    let epoch_before = guard.transform_epoch;
    let Some(old) = guard.clear_if_owner(generation) else {
        return false;
    };

    // A published identity reset needs a redraw even with no visible regions:
    // the surface may still be rendered with an active presentation transform.
    let transform_reset = guard.transform_epoch != epoch_before;

    if old.is_empty() && !transform_reset {
        return false;
    }

    if !old.is_empty() {
        debug!(
            old_count = old.len(),
            generation, "cleared Tahoe glass regions on controller destroy"
        );

        crate::render_helpers::tahoe_glass::damage_surface_regions(states, old.as_ref(), &[]);
        #[cfg(test)]
        {
            // Record the old committed geometry that production damage was asked to
            // cover so integration tests can prove clear damages the prior area.
            TEST_DAMAGE_OLD_REGION_COUNT.fetch_add(old.len(), AtomicOrdering::SeqCst);
            let mut rects = TEST_LAST_DAMAGED_OLD_RECTS.lock().unwrap();
            rects.clear();
            for region in old.iter() {
                let r = region.rect;
                rects.push((r.loc.x, r.loc.y, r.size.w, r.size.h));
            }
        }
    }
    true
}

/// Test-only counters for [`TahoeGlassHandler::queue_redraw_for_tahoe_glass_surface`]
/// and the clear-path damage call. Production builds omit these symbols entirely.
#[cfg(test)]
static TEST_TARGETED_REDRAW: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_FALLBACK_REDRAW_ALL: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_DAMAGE_OLD_REGION_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_LAST_DAMAGED_OLD_RECTS: Mutex<Vec<(i32, i32, i32, i32)>> = Mutex::new(Vec::new());
/// Serializes integration tests that reset/assert the shared redraw counters so
/// parallel libtest workers cannot interleave another test's reset between a
/// production handler note and the assert.
#[cfg(test)]
static TEST_REDRAW_COUNTER_LOCK: Mutex<()> = Mutex::new(());

/// Hold for the full reset → action → assert window of any counter-based test.
#[cfg(test)]
pub fn test_redraw_counter_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_REDRAW_COUNTER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reset redraw/damage counters between tests.
#[cfg(test)]
pub fn test_reset_redraw_counters() {
    TEST_TARGETED_REDRAW.store(0, AtomicOrdering::SeqCst);
    TEST_FALLBACK_REDRAW_ALL.store(0, AtomicOrdering::SeqCst);
    TEST_DAMAGE_OLD_REGION_COUNT.store(0, AtomicOrdering::SeqCst);
    TEST_LAST_DAMAGED_OLD_RECTS.lock().unwrap().clear();
}

/// Note a targeted (output_for_root hit) redraw from the production handler.
#[cfg(test)]
pub fn test_note_targeted_redraw() {
    TEST_TARGETED_REDRAW.fetch_add(1, AtomicOrdering::SeqCst);
}

/// Note a fallback queue_redraw_all from the production handler.
#[cfg(test)]
pub fn test_note_fallback_redraw_all() {
    TEST_FALLBACK_REDRAW_ALL.fetch_add(1, AtomicOrdering::SeqCst);
}

#[cfg(test)]
pub fn test_targeted_redraw_count() -> usize {
    TEST_TARGETED_REDRAW.load(AtomicOrdering::SeqCst)
}

#[cfg(test)]
pub fn test_fallback_redraw_all_count() -> usize {
    TEST_FALLBACK_REDRAW_ALL.load(AtomicOrdering::SeqCst)
}

#[cfg(test)]
pub fn test_damage_old_region_count() -> usize {
    TEST_DAMAGE_OLD_REGION_COUNT.load(AtomicOrdering::SeqCst)
}

#[cfg(test)]
pub fn test_last_damaged_old_rects() -> Vec<(i32, i32, i32, i32)> {
    TEST_LAST_DAMAGED_OLD_RECTS.lock().unwrap().clone()
}

/// Clear pending/committed glass state owned by `generation` on a still-alive
/// surface. Returns whether a redraw should be queued.
fn clear_surface_state_if_owner(surface: &WlSurface, generation: u64) -> bool {
    if !surface.is_alive() {
        // Surface is gone; nothing left to render or damage.
        return false;
    }

    with_states(surface, |states| {
        clear_surface_data_if_owner(states, generation)
    })
}

/// Mutate pending regions for `generation` through the same ownership gate as
/// protocol write requests. Returns `None` only for hard rejections (limit).
fn mutate_pending_if_owner(
    states: &SurfaceData,
    generation: u64,
    f: impl FnOnce(&mut Vec<TahoeGlassRegion>) -> Option<bool>,
) -> Option<bool> {
    let data = states
        .data_map
        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
    let mut guard = data.0.lock().unwrap();
    guard.with_pending_if_owner(generation, f)
}

fn validate_regions(
    states: &SurfaceData,
    pending: &[TahoeGlassRegion],
    previous: &[TahoeGlassRegion],
) -> Option<(Vec<TahoeGlassRegion>, bool)> {
    validate_regions_for_surface_geo(surface_geo(states), pending, previous)
}

/// Validate pending regions against the current surface geometry.
///
/// Returns `None` when there is no surface geometry yet (commit stays
/// deferred). Otherwise returns the accepted regions plus a `complete` flag.
///
/// `complete = false` only for *geometry-healable* rejections: the region's
/// origin lies inside the surface but its extent overflows it. That is the
/// region-ahead-of-buffer window — the region rides a commit whose buffer
/// still has the old (smaller) size, and the client-side diff cache never
/// re-sends an unchanged region, so [`on_surface_commit`] keeps
/// `pending_dirty` set and the commit attaching the caught-up buffer
/// revalidates the same pending set.
/// While waiting, the overflowing region is committed clamped to the current
/// surface so the glass tracks the growing buffer edge frame by frame (no
/// glassless band on the freshly revealed rows) — a pending region-morph
/// keeps an anchor under the same id, and the panel never flashes through the
/// fallback path. If the clamp itself exceeds the residual area budget, the
/// previously committed entry for that id is carried over instead (when it
/// still fits), so a multi-region panel under a tight area budget keeps its
/// old glass rather than dropping the fitting tail.
///
/// T-17 made the shell commit the region and its matching content buffer in
/// one atomic `wl_surface.commit` (quickshell `commitGlassIfIdle` defers the
/// explicit region/transform commit to the render-thread buffer commit), but
/// that guard only fires once a render cycle's `UpdateRequest` has been
/// delivered; a `Behavior on height`-driven region update can run its polish
/// before that, so the window still opens for surfaces like the clipboard
/// popup. The clamp keeps the glass correct for any client regardless.
///
/// Structural rejections (degenerate rects, origins outside the surface, the
/// area budget) keep the pre-existing drop-and-move-on semantics: growth
/// cannot heal them, and marking them incomplete would leave the surface
/// permanently dirty (revalidating every commit and starving region morphs).
fn validate_regions_for_surface_geo(
    surface_geo: Option<Rectangle<i32, Logical>>,
    pending: &[TahoeGlassRegion],
    previous: &[TahoeGlassRegion],
) -> Option<(Vec<TahoeGlassRegion>, bool)> {
    if pending.is_empty() {
        return Some((Vec::new(), true));
    }

    let surface_geo = surface_geo?;

    let surface_area = i64::from(surface_geo.size.w.max(0)) * i64::from(surface_geo.size.h.max(0));
    let mut total_area = 0i64;
    let mut committed = Vec::new();
    let mut complete = true;
    let mut budget_exhausted = false;

    for region in pending.iter().take(MAX_REGIONS_PER_SURFACE) {
        if region.rect.is_empty() {
            continue;
        }

        let Some(x2) = region.rect.loc.x.checked_add(region.rect.size.w) else {
            continue;
        };
        let Some(y2) = region.rect.loc.y.checked_add(region.rect.size.h) else {
            continue;
        };
        if x2 <= region.rect.loc.x || y2 <= region.rect.loc.y {
            continue;
        }

        let clamped = region.rect.intersection(surface_geo);
        if clamped != Some(region.rect) {
            let origin_inside = clamped.is_some()
                && region.rect.loc.x >= surface_geo.loc.x
                && region.rect.loc.y >= surface_geo.loc.y;
            if !origin_inside {
                // Fully outside or negative-origin: structural, permanent drop.
                continue;
            }

            // Geometry-healable overflow (animated grow window): the region
            // rides ahead of a still-growing buffer. Prefer the on-surface
            // clamp so the glass tracks the growing buffer edge frame by frame
            // (no glassless band on the freshly revealed rows); if the clamp
            // itself exceeds the residual area budget, fall back to carrying
            // the previously committed entry when it still fits, so the panel
            // keeps its old glass instead of dropping through to the fallback
            // path. Either way keep the commit incomplete so the full rect
            // revalidates once the buffer catches up. A healable clamp or
            // carry that does not fit the residual budget is skipped without
            // exhausting the budget — the overflow is healable, not
            // structural — so a later, genuinely fitting region still commits.
            complete = false;
            let clamped = clamped.unwrap();
            let clamp_area = i64::from(clamped.size.w) * i64::from(clamped.size.h);
            if total_area.saturating_add(clamp_area) <= surface_area {
                total_area = total_area.saturating_add(clamp_area);
                let mut clamped_region = region.clone();
                clamped_region.rect = clamped;
                committed.push(clamped_region);
            } else if let Some(prev) = previous.iter().find(|prev| prev.id == region.id) {
                if prev.rect.intersection(surface_geo) == Some(prev.rect) {
                    let area = i64::from(prev.rect.size.w) * i64::from(prev.rect.size.h);
                    if total_area.saturating_add(area) <= surface_area {
                        total_area = total_area.saturating_add(area);
                        committed.push(prev.clone());
                    }
                }
            }
            continue;
        }

        // Area budget exhaustion drops the remaining fitting regions for good
        // (pre-existing semantics), but later entries must still run the
        // geometry-healable classification above so a briefly-overflowing
        // region does not lose its revalidation.
        if budget_exhausted {
            continue;
        }
        let area = i64::from(region.rect.size.w) * i64::from(region.rect.size.h);
        if total_area.saturating_add(area) > surface_area {
            warn!(
                surface_area,
                total_area, "dropping Tahoe glass regions exceeding surface area"
            );
            budget_exhausted = true;
            continue;
        }
        total_area = total_area.saturating_add(area);

        committed.push(region.clone());
    }

    Some((committed, complete))
}

fn make_region(
    id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius_tl: i32,
    radius_tr: i32,
    radius_br: i32,
    radius_bl: i32,
    material: String,
    flags: u32,
    interaction: f64,
    material_alpha: f64,
) -> Option<TahoeGlassRegion> {
    if width <= 0 || height <= 0 {
        return None;
    }
    if radius_tl < 0 || radius_tr < 0 || radius_br < 0 || radius_bl < 0 {
        return None;
    }

    Some(TahoeGlassRegion {
        id,
        rect: Rectangle::new(Point::new(x, y), Size::new(width, height)),
        radius: CornerRadius {
            top_left: radius_tl as f32,
            top_right: radius_tr as f32,
            bottom_right: radius_br as f32,
            bottom_left: radius_bl as f32,
        },
        material: if material.is_empty() {
            "panel".to_owned()
        } else {
            material
        },
        flags: TahoeGlassFlags::from_bits(flags),
        interaction: interaction.clamp(0., 1.) as f32,
        material_alpha: material_alpha.clamp(0., 1.) as f32,
    })
}

fn make_transform_curve(
    curve: WEnum<tahoe_glass_surface_v1::TransformCurve>,
    p1: f64,
    p2: f64,
    p3: f64,
    p4: f64,
    p5: f64,
) -> Option<TahoeTransformCurve> {
    match curve {
        WEnum::Value(tahoe_glass_surface_v1::TransformCurve::Spring) => {
            // p4/p5 are reserved for the spring curve and ignored. Lower
            // bounds keep the settle envelope -ln(eps)/(dr*sqrt(st)) under a
            // few seconds so a parameter mistake cannot pin the output to
            // full-rate redraws for minutes.
            Some(TahoeTransformCurve::Spring {
                damping_ratio: p1.clamp(0.2, 10.),
                stiffness: p2.clamp(10., 100_000.),
                epsilon: p3.clamp(1e-4, 0.5),
            })
        }
        WEnum::Value(tahoe_glass_surface_v1::TransformCurve::Eased) => {
            Some(TahoeTransformCurve::Eased {
                duration_ms: p1.clamp(0., 10_000.).round() as u32,
                bezier: (
                    p2.clamp(0., 1.),
                    p3.clamp(-5., 5.),
                    p4.clamp(0., 1.),
                    p5.clamp(-5., 5.),
                ),
            })
        }
        WEnum::Unknown(value) => {
            debug!(value, "unknown Tahoe glass transform curve");
            None
        }
    }
}

/// Store a wire transform request for the surface, pending until commit.
/// Stale controllers are silent no-ops, matching region writes.
fn queue_transform_request(surface: &WlSurface, generation: u64, request: PendingTransformRequest) {
    let stored = with_inner(surface, |inner| {
        inner.set_pending_transform_if_owner(generation, request)
    });

    if stored {
        debug!(surface = %surface.id(), ?request, "queued Tahoe glass transform request");
    }
}

impl<D> GlobalDispatch<TahoeGlassManagerV1, TahoeGlassManagerGlobalData, D>
    for TahoeGlassManagerState
where
    D: GlobalDispatch<TahoeGlassManagerV1, TahoeGlassManagerGlobalData>,
    D: Dispatch<TahoeGlassManagerV1, ()>,
    D: Dispatch<TahoeGlassSurfaceV1, TahoeGlassSurfaceUserData>,
    D: TahoeGlassHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<TahoeGlassManagerV1>,
        _manager_state: &TahoeGlassManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &TahoeGlassManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<TahoeGlassManagerV1, (), D> for TahoeGlassManagerState
where
    D: Dispatch<TahoeGlassManagerV1, ()>,
    D: Dispatch<TahoeGlassSurfaceV1, TahoeGlassSurfaceUserData>,
    D: TahoeGlassHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &TahoeGlassManagerV1,
        request: <TahoeGlassManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            tahoe_glass_manager_v1::Request::Destroy => (),
            tahoe_glass_manager_v1::Request::GetTahoeGlassSurface { id, surface } => {
                // Claim ownership immediately so a still-pending destroy of a
                // previous controller cannot clear this new controller's state.
                // Also drop any glass left by a previous controller so recreate
                // never inherits pending/committed regions.
                let (controller_generation, needs_redraw) = with_states(&surface, |states| {
                    let data = states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                    let mut guard = data.0.lock().unwrap();
                    let epoch_before = guard.transform_epoch;
                    let (generation, old) = guard.claim_controller();
                    let had_visible = !old.is_empty();
                    if had_visible {
                        crate::render_helpers::tahoe_glass::damage_surface_regions(
                            states,
                            old.as_ref(),
                            &[],
                        );
                    }
                    // Claiming also resets an active presentation transform;
                    // that needs a redraw even when no regions were visible.
                    (
                        generation,
                        had_visible || guard.transform_epoch != epoch_before,
                    )
                });
                debug!(
                    surface = %surface.id(),
                    controller_generation,
                    "created Tahoe glass surface controller"
                );
                data_init.init(
                    id,
                    TahoeGlassSurfaceUserData {
                        surface: surface.clone(),
                        controller_generation,
                    },
                );
                if needs_redraw {
                    state.queue_redraw_for_tahoe_glass_surface(&surface);
                }
            }
        }
    }
}

impl<D> Dispatch<TahoeGlassSurfaceV1, TahoeGlassSurfaceUserData, D> for TahoeGlassManagerState
where
    D: Dispatch<TahoeGlassSurfaceV1, TahoeGlassSurfaceUserData>,
    D: TahoeGlassHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &TahoeGlassSurfaceV1,
        request: <TahoeGlassSurfaceV1 as Resource>::Request,
        data: &TahoeGlassSurfaceUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            // Protocol destructor: clear surface-owned glass state now, while
            // the wl_surface may still be alive. `destroyed` also calls the
            // same path so abnormal client disconnect is covered; clear is
            // generation-gated and idempotent.
            tahoe_glass_surface_v1::Request::Destroy => {
                if clear_surface_state_if_owner(&data.surface, data.controller_generation) {
                    state.queue_redraw_for_tahoe_glass_surface(&data.surface);
                }
            }
            tahoe_glass_surface_v1::Request::SetRegion {
                id,
                x,
                y,
                width,
                height,
                radius_tl,
                radius_tr,
                radius_br,
                radius_bl,
                material,
                flags,
                interaction,
                material_alpha,
            } => {
                crate::utils::lifecycle_diag::note_tahoe_region_request();
                let Some(region) = make_region(
                    id,
                    x,
                    y,
                    width,
                    height,
                    radius_tl,
                    radius_tr,
                    radius_br,
                    radius_bl,
                    material,
                    flags,
                    interaction,
                    material_alpha,
                ) else {
                    debug!(surface = %data.surface.id(), id, "discarding invalid Tahoe glass region");
                    return;
                };

                let changed = with_states(&data.surface, |states| {
                    mutate_pending_if_owner(states, data.controller_generation, |pending| {
                        if let Some(existing) = pending.iter_mut().find(|r| r.id == id) {
                            if *existing == region {
                                return Some(false);
                            }
                            *existing = region;
                            Some(true)
                        } else if pending.len() < MAX_REGIONS_PER_SURFACE {
                            pending.push(region);
                            Some(true)
                        } else {
                            None
                        }
                    })
                });

                let Some(changed) = changed else {
                    debug!(
                        surface = %data.surface.id(),
                        id,
                        "discarding Tahoe glass region beyond per-surface limit"
                    );
                    return;
                };

                if changed {
                    debug!(
                        surface = %data.surface.id(),
                        id,
                        "set Tahoe glass region"
                    );
                    mark_pending_dirty(&data.surface);
                }
            }
            tahoe_glass_surface_v1::Request::RemoveRegion { id } => {
                let removed = with_states(&data.surface, |states| {
                    mutate_pending_if_owner(states, data.controller_generation, |pending| {
                        let old_len = pending.len();
                        pending.retain(|r| r.id != id);
                        Some(pending.len() != old_len)
                    })
                    .unwrap_or(false)
                });

                if removed {
                    debug!(
                        surface = %data.surface.id(),
                        id,
                        "removed Tahoe glass region"
                    );
                    mark_pending_dirty(&data.surface);
                }
            }
            tahoe_glass_surface_v1::Request::ClearRegions => {
                let cleared = with_states(&data.surface, |states| {
                    mutate_pending_if_owner(states, data.controller_generation, |pending| {
                        let cleared = !pending.is_empty();
                        pending.clear();
                        Some(cleared)
                    })
                    .unwrap_or(false)
                });

                if cleared {
                    debug!(surface = %data.surface.id(), "cleared Tahoe glass regions");
                    mark_pending_dirty(&data.surface);
                }
            }
            tahoe_glass_surface_v1::Request::SetTransform {
                x,
                y,
                scale_x,
                scale_y,
            } => {
                let affine = PresentationAffine::sanitized(x, y, scale_x, scale_y);
                queue_transform_request(
                    &data.surface,
                    data.controller_generation,
                    PendingTransformRequest::Set(affine),
                );
            }
            tahoe_glass_surface_v1::Request::SetTransformTarget {
                x,
                y,
                scale_x,
                scale_y,
                curve,
                p1,
                p2,
                p3,
                p4,
                p5,
            } => {
                let Some(curve) = make_transform_curve(curve, p1, p2, p3, p4, p5) else {
                    debug!(
                        surface = %data.surface.id(),
                        "discarding Tahoe glass transform target with invalid curve"
                    );
                    return;
                };
                let affine = PresentationAffine::sanitized(x, y, scale_x, scale_y);
                queue_transform_request(
                    &data.surface,
                    data.controller_generation,
                    PendingTransformRequest::Target(affine, curve),
                );
            }
            tahoe_glass_surface_v1::Request::SetRegionMorph {
                region_id,
                curve,
                p1,
                p2,
                p3,
                p4,
                p5,
            } => {
                let Some(curve) = make_transform_curve(curve, p1, p2, p3, p4, p5) else {
                    debug!(
                        surface = %data.surface.id(),
                        region_id,
                        "discarding Tahoe glass region morph with invalid curve"
                    );
                    return;
                };
                queue_transform_request(
                    &data.surface,
                    data.controller_generation,
                    PendingTransformRequest::RegionMorph(region_id, curve),
                );
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        _resource: &TahoeGlassSurfaceV1,
        data: &TahoeGlassSurfaceUserData,
    ) {
        // Covers abnormal disconnect and any path where the resource is
        // dropped without a successful destructor request ordering guarantee.
        // Generation check makes double-clear with Destroy a no-op for state
        // (second call sees empty committed / same generation still authorized).
        if clear_surface_state_if_owner(&data.surface, data.controller_generation) {
            state.queue_redraw_for_tahoe_glass_surface(&data.surface);
        }
    }
}

#[macro_export]
macro_rules! delegate_tahoe_glass {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::tahoe_glass::v1::server::tahoe_glass_manager_v1::TahoeGlassManagerV1: $crate::protocols::tahoe_glass::TahoeGlassManagerGlobalData
        ] => $crate::protocols::tahoe_glass::TahoeGlassManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::tahoe_glass::v1::server::tahoe_glass_manager_v1::TahoeGlassManagerV1: ()
        ] => $crate::protocols::tahoe_glass::TahoeGlassManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::tahoe_glass::v1::server::tahoe_glass_surface_v1::TahoeGlassSurfaceV1: $crate::protocols::tahoe_glass::TahoeGlassSurfaceUserData
        ] => $crate::protocols::tahoe_glass::TahoeGlassManagerState);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: u32, x: i32, y: i32, width: i32, height: i32) -> TahoeGlassRegion {
        TahoeGlassRegion {
            id,
            rect: Rectangle::new(Point::new(x, y), Size::new(width, height)),
            radius: CornerRadius::default(),
            material: "panel".to_owned(),
            flags: TahoeGlassFlags {
                blur: true,
                shadow: true,
                clip: true,
            },
            interaction: 0.,
            material_alpha: 1.,
        }
    }

    fn surface_with_committed(regions: Vec<TahoeGlassRegion>) -> TahoeGlassSurfaceInner {
        TahoeGlassSurfaceInner {
            pending: regions.clone(),
            committed: Arc::new(regions),
            pending_dirty: true,
            ..Default::default()
        }
    }

    #[test]
    fn validation_defers_non_empty_regions_until_surface_geometry_exists() {
        assert_eq!(
            validate_regions_for_surface_geo(None, &[region(1, 8, 4, 128, 32)], &[]),
            None
        );
    }

    #[test]
    fn validation_allows_empty_regions_without_surface_geometry() {
        assert_eq!(
            validate_regions_for_surface_geo(None, &[], &[]),
            Some((Vec::new(), true))
        );
    }

    #[test]
    fn validation_keeps_only_regions_inside_surface_geometry() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 40));
        let inside = region(1, 8, 4, 84, 32);
        // Partially outside with origin inside is healable: committed clamped,
        // flagged incomplete so the full rect revalidates once the surface grows.
        let overflowing = region(2, 90, 4, 20, 32);
        let clamped = region(2, 90, 4, 10, 32);

        assert_eq!(
            validate_regions_for_surface_geo(Some(surface_geo), &[inside.clone(), overflowing], &[]),
            Some((vec![inside, clamped], false))
        );
    }

    /// Region ahead of buffer during an animated grow: the pending target is
    /// clamped to the current surface so the glass tracks the buffer edge
    /// frame by frame (no glassless band on the newly revealed rows), and the
    /// commit is flagged incomplete so the full rect revalidates once the
    /// buffer catches up — without any client retransmission.
    #[test]
    fn oversized_region_clamps_and_revalidates_once_surface_geometry_catches_up() {
        let grown = region(1, 0, 0, 360, 480);

        let small_geo = Rectangle::new(Point::new(0, 0), Size::new(360, 440));
        let small_clamped = region(1, 0, 0, 360, 440);
        assert_eq!(
            validate_regions_for_surface_geo(Some(small_geo), &[grown.clone()], &[]),
            Some((vec![small_clamped], false)),
            "overflowing region must clamp to the current surface and stay dirty"
        );

        let mid_geo = Rectangle::new(Point::new(0, 0), Size::new(360, 460));
        let mid_clamped = region(1, 0, 0, 360, 460);
        assert_eq!(
            validate_regions_for_surface_geo(Some(mid_geo), &[grown.clone()], &[]),
            Some((vec![mid_clamped], false)),
            "as the buffer grows mid-animation the clamp must track it"
        );

        let caught_up_geo = Rectangle::new(Point::new(0, 0), Size::new(360, 480));
        assert_eq!(
            validate_regions_for_surface_geo(Some(caught_up_geo), &[grown.clone()], &[]),
            Some((vec![grown], true)),
            "the same pending set must fully commit once the buffer catches up"
        );
    }

    /// Degenerate regions (empty, overflowing) are permanently invalid: they
    /// must not keep the commit incomplete and retrigger validation forever.
    #[test]
    fn degenerate_regions_do_not_mark_validation_incomplete() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 40));
        let valid = region(1, 0, 0, 80, 32);
        let empty = region(2, 10, 10, 0, 0);
        let overflow = region(3, i32::MAX - 1, 0, 8, 8);

        assert_eq!(
            validate_regions_for_surface_geo(
                Some(surface_geo),
                &[valid.clone(), empty, overflow],
                &[],
            ),
            Some((vec![valid], true))
        );
    }

    /// Structural rejections keep the pre-existing drop semantics: growth can
    /// never heal them, so they must not leave the surface permanently dirty
    /// (which would revalidate every commit and starve pending region morphs).
    #[test]
    fn structural_rejections_stay_complete() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 100));

        // Area budget: two overlapping in-bounds regions whose sum exceeds the
        // surface area. The overflowing tail is dropped for good.
        let base = region(1, 0, 0, 100, 100);
        let pill = region(2, 10, 10, 40, 20);
        assert_eq!(
            validate_regions_for_surface_geo(Some(surface_geo), &[base.clone(), pill], &[]),
            Some((vec![base], true)),
            "area-budget overflow is structural, not geometry-healable"
        );

        // Negative-origin region: outside on the min side; growth on the max
        // side cannot heal it.
        let negative = region(3, -8, 4, 40, 20);
        let inside = region(4, 0, 0, 40, 20);
        assert_eq!(
            validate_regions_for_surface_geo(Some(surface_geo), &[inside.clone(), negative], &[],),
            Some((vec![inside], true)),
            "negative-origin regions are structural drops"
        );
    }

    /// The pending list is capped at MAX_REGIONS_PER_SURFACE by the request
    /// handler, so `take(MAX)` truncation is unreachable in production; if it
    /// ever happens it must not mark the commit incomplete (truncation is not
    /// geometry-healable either).
    #[test]
    fn excess_region_count_truncates_without_marking_incomplete() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(4096, 4096));
        let pending: Vec<_> = (0..(MAX_REGIONS_PER_SURFACE as u32 + 3))
            .map(|i| region(i + 1, (i as i32 % 32) * 8, (i as i32 / 32) * 8, 4, 4))
            .collect();

        let (committed, complete) =
            validate_regions_for_surface_geo(Some(surface_geo), &pending, &[]).unwrap();
        assert_eq!(committed.len(), MAX_REGIONS_PER_SURFACE);
        assert!(complete, "count truncation must not keep the surface dirty");
    }

    /// Area-budget exhaustion drops the remaining fitting regions (structural)
    /// but must not short-circuit the geometry-healable classification of
    /// later entries: a briefly-overflowing region behind the budget point
    /// still keeps the surface dirty and carries its previous entry.
    #[test]
    fn healable_overflow_behind_budget_point_still_revalidates() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 100));
        let base = region(1, 0, 0, 90, 100);
        let exhausting = region(2, 0, 0, 60, 40);
        let grown = region(3, 0, 0, 40, 120);
        let previous = region(3, 0, 0, 40, 20);

        let (committed, complete) = validate_regions_for_surface_geo(
            Some(surface_geo),
            &[base.clone(), exhausting, grown],
            std::slice::from_ref(&previous),
        )
        .unwrap();

        assert!(
            !complete,
            "healable overflow after the budget point must keep dirty"
        );
        assert_eq!(
            committed,
            vec![base, previous],
            "budget drops the fitting tail but the healable entry still carries"
        );
    }

    /// A healable overflow whose clamp (and carried previous entry) fit the
    /// surface but not the residual area budget is skipped without consuming
    /// the budget, so a later, genuinely fitting region still commits. The
    /// healable path accounts by actual committed area (clamp preferred, then
    /// previous fallback) — a clamp that overflows the residual budget never
    /// sets `budget_exhausted`, so the fitting tail survives.
    #[test]
    fn skipped_healable_overflow_does_not_exhaust_budget_for_later_fitting_region() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 100));
        // 9900 area, fully inside — committed and consumes most of the budget.
        let base = region(1, 0, 0, 100, 99);
        // Healable: id 2 overflows the surface (height 200). Its on-surface
        // clamp (40×100) and its previous entry (40×100) both exceed the
        // residual budget after `base` (100), so both are skipped — and must
        // NOT set `budget_exhausted`.
        let grown = region(2, 0, 0, 40, 200);
        let previous = region(2, 0, 0, 40, 100);
        // A later, fitting structural region must still commit (9900 + 100).
        let fitting = region(3, 50, 0, 50, 2);

        let (committed, complete) = validate_regions_for_surface_geo(
            Some(surface_geo),
            &[base.clone(), grown, fitting.clone()],
            std::slice::from_ref(&previous),
        )
        .unwrap();

        assert!(
            !complete,
            "the healable overflow (id 2) must keep the surface dirty"
        );
        assert_eq!(
            committed,
            vec![base, fitting],
            "skipped healable overflow must not exhaust the budget; the later fitting region still commits"
        );
    }

    #[test]
    fn equality_preserves_adjacent_protocol_quantization_steps() {
        let mut before = region(1, 0, 0, 100, 40);
        let mut after = before.clone();

        // Wayland fixed values are multiples of 1/256. The old 0.02 fuzzy
        // comparison swallowed common adjacent 0.02 client buckets because
        // five fixed steps are only 0.01953125 apart, including 0.98 -> 1.0.
        before.material_alpha = 251. / 256.;
        after.material_alpha = 1.;
        assert_ne!(before, after);

        let exact = after.clone();
        assert_eq!(after, exact);
    }

    /// create → set/commit → destroy controller: surface still alive must end
    /// with empty pending and committed (regions cleared immediately).
    #[test]
    fn destroy_clears_pending_and_committed_for_owner() {
        let mut inner = surface_with_committed(vec![region(1, 0, 0, 64, 32)]);
        let (generation, _) = inner.claim_controller();
        // Simulate set + commit under this controller.
        inner.pending = vec![region(7, 4, 4, 40, 20)];
        inner.committed = Arc::new(inner.pending.clone());
        inner.pending_dirty = false;

        let old = inner
            .clear_if_owner(generation)
            .expect("owner must be authorized to clear");
        assert_eq!(old.len(), 1);
        assert!(inner.pending.is_empty());
        assert!(inner.committed.is_empty());
        assert!(!inner.pending_dirty);
        assert_eq!(inner.controller_generation, generation);
    }

    /// destroy → recreate must not inherit old pending/committed.
    #[test]
    fn recreate_does_not_inherit_previous_controller_state() {
        let mut inner =
            surface_with_committed(vec![region(1, 0, 0, 64, 32), region(2, 8, 8, 16, 16)]);
        let (old_gen, _) = inner.claim_controller();
        inner.pending = vec![region(1, 0, 0, 64, 32)];
        inner.committed = Arc::new(vec![region(1, 0, 0, 64, 32)]);
        inner.pending_dirty = true;

        let (new_gen, old_committed) = inner.claim_controller();
        assert_ne!(old_gen, new_gen);
        assert_eq!(old_committed.len(), 1);
        assert!(inner.pending.is_empty());
        assert!(inner.committed.is_empty());
        assert!(!inner.pending_dirty);
        assert_eq!(inner.controller_generation, new_gen);
    }

    /// Old controller destroy after recreate must not clear the new owner.
    #[test]
    fn stale_controller_destroy_cannot_clear_new_owner() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (old_gen, _) = inner.claim_controller();
        inner.pending = vec![region(1, 0, 0, 10, 10)];
        inner.committed = Arc::new(vec![region(1, 0, 0, 10, 10)]);

        let (new_gen, _) = inner.claim_controller();
        // New owner sets its own regions.
        inner.pending = vec![region(9, 1, 1, 20, 20)];
        inner.committed = Arc::new(vec![region(9, 1, 1, 20, 20)]);

        assert!(
            inner.clear_if_owner(old_gen).is_none(),
            "old generation must not be authorized after recreate"
        );
        assert_eq!(inner.pending.len(), 1);
        assert_eq!(inner.committed.len(), 1);
        assert_eq!(inner.committed[0].id, 9);
        assert_eq!(inner.controller_generation, new_gen);
    }

    /// Destroy + destroyed double-invoke is idempotent for the same generation.
    #[test]
    fn clear_for_owner_is_idempotent() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (generation, _) = inner.claim_controller();
        inner.pending = vec![region(3, 0, 0, 8, 8)];
        inner.committed = Arc::new(vec![region(3, 0, 0, 8, 8)]);
        inner.pending_dirty = true;

        let first = inner.clear_if_owner(generation).expect("first clear");
        assert_eq!(first.len(), 1);

        let second = inner
            .clear_if_owner(generation)
            .expect("same generation remains authorized");
        assert!(second.is_empty());
        assert!(inner.pending.is_empty());
        assert!(inner.committed.is_empty());
        assert!(!inner.pending_dirty);
    }

    /// Production write gate: stale set/remove/clear must not mutate pending.
    ///
    /// Calls the same `with_pending_if_owner` used by SetRegion/RemoveRegion/
    /// ClearRegions. Deleting that gate (or `is_owner`) makes this test fail.
    #[test]
    fn stale_controller_writes_are_rejected_via_write_gate() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (old_gen, _) = inner.claim_controller();
        let (new_gen, _) = inner.claim_controller();
        assert_ne!(old_gen, new_gen);

        // Current owner can set.
        let set = inner
            .with_pending_if_owner(new_gen, |pending| {
                pending.push(region(1, 0, 0, 4, 4));
                Some(true)
            })
            .expect("owner write must be accepted");
        assert!(set);
        assert_eq!(inner.pending.len(), 1);
        assert_eq!(inner.pending[0].id, 1);

        // Stale set is a silent no-op (Some(false) default).
        let stale_set = inner
            .with_pending_if_owner(old_gen, |pending| {
                pending.push(region(2, 0, 0, 8, 8));
                Some(true)
            })
            .expect("stale write returns default, not hard reject");
        assert!(!stale_set);
        assert_eq!(inner.pending.len(), 1);
        assert_eq!(inner.pending[0].id, 1);

        // Stale remove is a silent no-op.
        let stale_remove = inner
            .with_pending_if_owner(old_gen, |pending| {
                pending.clear();
                Some(true)
            })
            .unwrap();
        assert!(!stale_remove);
        assert_eq!(inner.pending.len(), 1);

        // Current owner clear works.
        let cleared = inner
            .with_pending_if_owner(new_gen, |pending| {
                let was_non_empty = !pending.is_empty();
                pending.clear();
                Some(was_non_empty)
            })
            .unwrap();
        assert!(cleared);
        assert!(inner.pending.is_empty());
    }

    /// Production clear path used by Destroy and destroyed: same function body
    /// as `clear_surface_state_if_owner` after the alive check.
    ///
    /// Exercises ownership on `TahoeGlassSurfaceData` (the surface data map
    /// owner), not only free-floating Inner helpers. If Destroy stayed empty
    /// and never called this path, the committed regions would remain.
    #[test]
    fn destroy_clear_path_empties_surface_data_map_state() {
        let data = TahoeGlassSurfaceData::default();
        let generation = {
            let mut guard = data.0.lock().unwrap();
            let (generation, _) = guard.claim_controller();
            guard.pending = vec![region(7, 0, 0, 32, 16)];
            guard.committed = Arc::new(vec![region(7, 0, 0, 32, 16)]);
            guard.pending_dirty = false;
            generation
        };

        // Mirror clear_surface_data_if_owner without requiring a live WlSurface.
        let needs_redraw = {
            let mut guard = data.0.lock().unwrap();
            match guard.clear_if_owner(generation) {
                Some(old) => !old.is_empty(),
                None => false,
            }
        };
        assert!(
            needs_redraw,
            "clearing non-empty committed must request redraw"
        );

        let guard = data.0.lock().unwrap();
        assert!(guard.pending.is_empty());
        assert!(guard.committed.is_empty());
        assert!(!guard.pending_dirty);
        assert_eq!(guard.controller_generation, generation);
    }

    /// Stale Destroy/destroyed after recreate must leave the new owner's
    /// surface data map state intact (same gate as production clear path).
    #[test]
    fn stale_destroy_clear_path_cannot_touch_new_owner_surface_data() {
        let data = TahoeGlassSurfaceData::default();
        let old_gen = {
            let mut guard = data.0.lock().unwrap();
            let (generation, _) = guard.claim_controller();
            guard.pending = vec![region(1, 0, 0, 10, 10)];
            guard.committed = Arc::new(vec![region(1, 0, 0, 10, 10)]);
            generation
        };
        let new_gen = {
            let mut guard = data.0.lock().unwrap();
            let (generation, old) = guard.claim_controller();
            assert_eq!(old.len(), 1, "claim must surface previous committed");
            assert!(guard.pending.is_empty());
            assert!(guard.committed.is_empty());
            guard.pending = vec![region(9, 1, 1, 20, 20)];
            guard.committed = Arc::new(vec![region(9, 1, 1, 20, 20)]);
            generation
        };

        let needs_redraw = {
            let mut guard = data.0.lock().unwrap();
            match guard.clear_if_owner(old_gen) {
                Some(old) => !old.is_empty(),
                None => false,
            }
        };
        assert!(
            !needs_redraw,
            "stale destroy must not authorize clear or redraw"
        );

        let guard = data.0.lock().unwrap();
        assert_eq!(guard.controller_generation, new_gen);
        assert_eq!(guard.pending.len(), 1);
        assert_eq!(guard.committed.len(), 1);
        assert_eq!(guard.committed[0].id, 9);
    }

    /// Destroy then destroyed (double clear) on the surface data owner is
    /// idempotent and only the first non-empty clear needs redraw.
    #[test]
    fn destroy_then_destroyed_double_clear_is_idempotent_on_surface_data() {
        let data = TahoeGlassSurfaceData::default();
        let generation = {
            let mut guard = data.0.lock().unwrap();
            let (generation, _) = guard.claim_controller();
            guard.pending = vec![region(3, 0, 0, 8, 8)];
            guard.committed = Arc::new(vec![region(3, 0, 0, 8, 8)]);
            generation
        };

        let first = {
            let mut guard = data.0.lock().unwrap();
            guard
                .clear_if_owner(generation)
                .map(|old| !old.is_empty())
                .unwrap_or(false)
        };
        let second = {
            let mut guard = data.0.lock().unwrap();
            guard
                .clear_if_owner(generation)
                .map(|old| !old.is_empty())
                .unwrap_or(false)
        };
        assert!(first);
        assert!(!second);

        let guard = data.0.lock().unwrap();
        assert!(guard.pending.is_empty());
        assert!(guard.committed.is_empty());
    }

    /// Region-limit rejection must still propagate as None through the write
    /// gate (distinct from stale silent no-op).
    #[test]
    fn write_gate_propagates_hard_reject_for_region_limit() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (generation, _) = inner.claim_controller();
        for id in 0..MAX_REGIONS_PER_SURFACE as u32 {
            inner.pending.push(region(id, 0, 0, 1, 1));
        }

        let result = inner.with_pending_if_owner(generation, |pending| {
            if pending.len() < MAX_REGIONS_PER_SURFACE {
                pending.push(region(999, 0, 0, 1, 1));
                Some(true)
            } else {
                None
            }
        });
        assert!(result.is_none());
        assert_eq!(inner.pending.len(), MAX_REGIONS_PER_SURFACE);
    }

    fn spring_curve() -> TahoeTransformCurve {
        TahoeTransformCurve::Spring {
            damping_ratio: 0.85,
            stiffness: 160.,
            epsilon: 0.001,
        }
    }

    /// Transform writes go through the same generation gate as region writes:
    /// stale controllers are silent no-ops.
    #[test]
    fn stale_controller_transform_writes_are_rejected() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (old_gen, _) = inner.claim_controller();
        let (new_gen, _) = inner.claim_controller();

        let target = PendingTransformRequest::Target(
            PresentationAffine {
                x: 0.,
                y: 40.,
                scale_x: 1.,
                scale_y: 1.,
            },
            spring_curve(),
        );
        assert!(!inner.set_pending_transform_if_owner(old_gen, target));
        assert!(inner.pending_transform.is_none());

        assert!(inner.set_pending_transform_if_owner(new_gen, target));
        assert_eq!(inner.pending_transform, Some(target));
    }

    /// Controller destroy/recreate must reset the presentation transform:
    /// pending request dropped, identity directive published with a fresh
    /// epoch so the layer machinery clears any active animation.
    #[test]
    fn claim_and_clear_reset_transform_state() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (generation, _) = inner.claim_controller();
        let epoch_after_claim = inner.transform_epoch;

        inner.set_pending_transform_if_owner(
            generation,
            PendingTransformRequest::Set(PresentationAffine {
                x: 10.,
                y: 0.,
                scale_x: 1.,
                scale_y: 1.,
            }),
        );
        inner.publish_transform_directive(TahoeGlassTransformDirective::Set(PresentationAffine {
            x: 10.,
            y: 0.,
            scale_x: 1.,
            scale_y: 1.,
        }));
        let epoch_after_publish = inner.transform_epoch;
        assert_ne!(epoch_after_claim, epoch_after_publish);

        inner.clear_if_owner(generation).expect("owner clear");
        assert!(inner.pending_transform.is_none());
        assert_eq!(
            inner.transform_directive,
            Some(TahoeGlassTransformDirective::Set(
                PresentationAffine::IDENTITY
            ))
        );
        assert_ne!(inner.transform_epoch, epoch_after_publish);
    }

    /// Destroy then destroyed (double clear) must not bump the epoch twice:
    /// the identity reset is idempotent.
    #[test]
    fn transform_reset_is_idempotent_across_double_clear() {
        let mut inner = TahoeGlassSurfaceInner::default();
        let (generation, _) = inner.claim_controller();
        inner.publish_transform_directive(TahoeGlassTransformDirective::Target(
            PresentationAffine {
                x: 0.,
                y: 88.,
                scale_x: 1.,
                scale_y: 1.,
            },
            spring_curve(),
        ));

        inner.clear_if_owner(generation).expect("first clear");
        let epoch = inner.transform_epoch;
        inner.clear_if_owner(generation).expect("second clear");
        assert_eq!(
            inner.transform_epoch, epoch,
            "identity reset must not re-publish on double clear"
        );
    }

    #[test]
    fn affine_sanitize_clamps_scales_and_translations() {
        let affine = PresentationAffine::sanitized(1e9, -1e9, 0., -3.);
        assert_eq!(affine.x, 16384.);
        assert_eq!(affine.y, -16384.);
        assert_eq!(affine.scale_x, 0.05);
        assert_eq!(affine.scale_y, 0.05);
    }

    #[test]
    fn affine_rect_mapping_round_trips() {
        let old_rect = Rectangle::<f64, Logical>::new(Point::new(904., 4.), Size::new(112., 32.));
        let new_rect = Rectangle::<f64, Logical>::new(Point::new(744., 4.), Size::new(432., 172.));

        // Morph from-affine: maps the new geometry onto the old footprint.
        let from = PresentationAffine::mapping_rect(new_rect, old_rect).unwrap();
        let mapped = from.apply_rect(new_rect);
        assert!((mapped.loc.x - old_rect.loc.x).abs() < 1e-9);
        assert!((mapped.loc.y - old_rect.loc.y).abs() < 1e-9);
        assert!((mapped.size.w - old_rect.size.w).abs() < 1e-9);
        assert!((mapped.size.h - old_rect.size.h).abs() < 1e-9);

        // Identity maps any rect onto itself.
        let identity = PresentationAffine::IDENTITY.apply_rect(new_rect);
        assert_eq!(identity, new_rect);

        // Degenerate source is rejected.
        let degenerate = Rectangle::<f64, Logical>::new(Point::new(0., 0.), Size::new(0., 10.));
        assert!(PresentationAffine::mapping_rect(degenerate, old_rect).is_none());
    }
}
