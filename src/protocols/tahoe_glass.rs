#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use niri_config::CornerRadius;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{add_post_commit_hook, with_states, SurfaceData};

use super::raw::tahoe_glass::v1::server::tahoe_glass_manager_v1::{self, TahoeGlassManagerV1};
use super::raw::tahoe_glass::v1::server::tahoe_glass_surface_v1::{self, TahoeGlassSurfaceV1};
use crate::niri::State;
use crate::utils::surface_geo;

// Version of the *manager* interface global. The manager interface itself is
// still v1 (only `get_tahoe_glass_surface`); the surface interface is v3, which
// carries the `interaction` and `material_alpha` args on `set_region`. Bumping this to the surface
// version makes wayland-backend reject the global ("implemented version higher
// than interface version") and panic, so it must stay at the manager's version.
const VERSION: u32 = 1;
pub const MAX_REGIONS_PER_SURFACE: usize = 32;

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
    hook_registered: bool,
    /// Monotonic owner token for the active `tahoe_glass_surface_v1`.
    /// Only the controller with this generation may clear surface state.
    controller_generation: u64,
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
        let old = std::mem::replace(&mut self.committed, Arc::new(Vec::new()));
        Some(old)
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

fn mark_pending_dirty(surface: &WlSurface) {
    let register_hook = with_states(surface, |states| {
        let state = states
            .data_map
            .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
        let mut guard = state.0.lock().unwrap();
        guard.pending_dirty = true;

        if guard.hook_registered {
            false
        } else {
            guard.hook_registered = true;
            true
        }
    });

    if register_hook {
        add_post_commit_hook::<State, _>(surface, |state, _dh, surface| {
            let changed = with_states(surface, |states| {
                let Some(data) = states.data_map.get::<TahoeGlassSurfaceData>() else {
                    return false;
                };

                let mut guard = data.0.lock().unwrap();
                if !guard.pending_dirty {
                    return false;
                }

                let Some(committed) = validate_regions(states, &guard.pending) else {
                    debug!(
                        surface = %surface.id(),
                        pending_count = guard.pending.len(),
                        "deferring Tahoe glass region commit until surface geometry is available"
                    );
                    return false;
                };

                guard.pending_dirty = false;
                if *guard.committed == committed {
                    return false;
                }

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
                true
            });

            if changed {
                if let Some(output) = state.niri.output_for_root(surface).cloned() {
                    state.niri.queue_redraw(&output);
                } else {
                    state.niri.queue_redraw_all();
                }
            }
        });
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
    let Some(old) = guard.clear_if_owner(generation) else {
        return false;
    };

    if old.is_empty() {
        return false;
    }

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
) -> Option<Vec<TahoeGlassRegion>> {
    validate_regions_for_surface_geo(surface_geo(states), pending)
}

fn validate_regions_for_surface_geo(
    surface_geo: Option<Rectangle<i32, Logical>>,
    pending: &[TahoeGlassRegion],
) -> Option<Vec<TahoeGlassRegion>> {
    if pending.is_empty() {
        return Some(Vec::new());
    }

    let surface_geo = surface_geo?;

    let surface_area = i64::from(surface_geo.size.w.max(0)) * i64::from(surface_geo.size.h.max(0));
    let mut total_area = 0i64;
    let mut committed = Vec::new();

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

        let Some(clamped) = region.rect.intersection(surface_geo) else {
            continue;
        };
        if clamped != region.rect {
            continue;
        }

        let area = i64::from(region.rect.size.w) * i64::from(region.rect.size.h);
        total_area = total_area.saturating_add(area);
        if total_area > surface_area {
            warn!(
                surface_area,
                total_area, "dropping Tahoe glass regions exceeding surface area"
            );
            break;
        }

        committed.push(region.clone());
    }

    Some(committed)
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
                let (controller_generation, had_visible_glass) = with_states(&surface, |states| {
                    let data = states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                    let mut guard = data.0.lock().unwrap();
                    let (generation, old) = guard.claim_controller();
                    let had_visible = !old.is_empty();
                    if had_visible {
                        crate::render_helpers::tahoe_glass::damage_surface_regions(
                            states,
                            old.as_ref(),
                            &[],
                        );
                    }
                    (generation, had_visible)
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
                if had_visible_glass {
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
            hook_registered: true,
            controller_generation: 0,
        }
    }

    #[test]
    fn validation_defers_non_empty_regions_until_surface_geometry_exists() {
        assert_eq!(
            validate_regions_for_surface_geo(None, &[region(1, 8, 4, 128, 32)]),
            None
        );
    }

    #[test]
    fn validation_allows_empty_regions_without_surface_geometry() {
        assert_eq!(
            validate_regions_for_surface_geo(None, &[]),
            Some(Vec::new())
        );
    }

    #[test]
    fn validation_keeps_only_regions_inside_surface_geometry() {
        let surface_geo = Rectangle::new(Point::new(0, 0), Size::new(100, 40));
        let inside = region(1, 8, 4, 84, 32);
        let outside = region(2, 90, 4, 20, 32);

        assert_eq!(
            validate_regions_for_surface_geo(Some(surface_geo), &[inside.clone(), outside]),
            Some(vec![inside])
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
}
