use std::sync::{Arc, Mutex};

use niri_config::CornerRadius;
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

const VERSION: u32 = 2;
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
}

pub struct TahoeGlassSurfaceUserData {
    surface: WlSurface,
}

#[derive(Default)]
struct TahoeGlassSurfaceData(Mutex<TahoeGlassSurfaceInner>);

#[derive(Default)]
struct TahoeGlassSurfaceInner {
    pending: Vec<TahoeGlassRegion>,
    committed: Arc<Vec<TahoeGlassRegion>>,
    pending_dirty: bool,
    hook_registered: bool,
}

pub struct TahoeGlassManagerState;

pub struct TahoeGlassManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait TahoeGlassHandler {}

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

                guard.pending_dirty = false;

                let committed = validate_regions(states, &guard.pending);
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
                crate::render_helpers::tahoe_glass::damage_surface(states);
                true
            });

            if changed {
                state.niri.queue_redraw_all();
            }
        });
    }
}

fn validate_regions(states: &SurfaceData, pending: &[TahoeGlassRegion]) -> Vec<TahoeGlassRegion> {
    let Some(surface_geo) = surface_geo(states) else {
        return Vec::new();
    };

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

    committed
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
        _state: &mut D,
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
                with_states(&surface, |states| {
                    states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                });
                debug!(surface = %surface.id(), "created Tahoe glass surface");
                data_init.init(id, TahoeGlassSurfaceUserData { surface });
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
        _state: &mut D,
        _client: &Client,
        _resource: &TahoeGlassSurfaceV1,
        request: <TahoeGlassSurfaceV1 as Resource>::Request,
        data: &TahoeGlassSurfaceUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            tahoe_glass_surface_v1::Request::Destroy => (),
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
            } => {
                let Some(region) = make_region(
                    id, x, y, width, height, radius_tl, radius_tr, radius_br, radius_bl, material,
                    flags, interaction,
                ) else {
                    debug!(surface = %data.surface.id(), id, "discarding invalid Tahoe glass region");
                    return;
                };

                let inserted = with_states(&data.surface, |states| {
                    let state = states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                    let mut guard = state.0.lock().unwrap();
                    if let Some(existing) = guard.pending.iter_mut().find(|r| r.id == id) {
                        *existing = region;
                        true
                    } else if guard.pending.len() < MAX_REGIONS_PER_SURFACE {
                        guard.pending.push(region);
                        true
                    } else {
                        false
                    }
                });

                if !inserted {
                    debug!(
                        surface = %data.surface.id(),
                        id,
                        "discarding Tahoe glass region beyond per-surface limit"
                    );
                    return;
                }

                debug!(
                    surface = %data.surface.id(),
                    id,
                    "set Tahoe glass region"
                );
                mark_pending_dirty(&data.surface);
            }
            tahoe_glass_surface_v1::Request::RemoveRegion { id } => {
                with_states(&data.surface, |states| {
                    let state = states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                    state.0.lock().unwrap().pending.retain(|r| r.id != id);
                });

                debug!(
                    surface = %data.surface.id(),
                    id,
                    "removed Tahoe glass region"
                );
                mark_pending_dirty(&data.surface);
            }
            tahoe_glass_surface_v1::Request::ClearRegions => {
                with_states(&data.surface, |states| {
                    let state = states
                        .data_map
                        .get_or_insert_threadsafe(TahoeGlassSurfaceData::default);
                    state.0.lock().unwrap().pending.clear();
                });

                debug!(surface = %data.surface.id(), "cleared Tahoe glass regions");
                mark_pending_dirty(&data.surface);
            }
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
