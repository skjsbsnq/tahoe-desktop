//! Shared minimize/restore lifecycle controller.
//!
//! Floating and scrolling each hold one runtime instance. The **policy** lives only here:
//! per-window active animation, duplicate/no-op, reverse reusing the same snapshot/texture,
//! advance + completion cleanup, restore-period live-tile visibility lease, and overlay
//! enumeration metadata.
//!
//! Space adapters supply tile lookup/position, snapshot capture context, layout-specific
//! focus/activation, and where overlays are placed in the render stack. This module never
//! inspects Column or FloatingSpace layout structure.

use std::fmt::Debug;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale};
use tracing::warn;

use super::coords::{GenieEndpointResolve, OutputLocalRect};
use super::minimize_window_animation::{
    MinimizeWindowAnimation, MinimizeWindowAnimationRenderElement,
};
use super::tile::TileRenderSnapshot;
use crate::animation::{Animation, Clock};
use crate::render_helpers::RenderCtx;

/// Direction of an active minimize/restore Genie (or alpha fallback) animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAnimDirection {
    Minimize,
    Restore,
}

/// Controller-managed live-tile visibility lease events for the space adapter to apply.
///
/// While a restore overlay owns the pixels, the live tile must not draw. Every path that
/// ends the restore overlay (success, failure, cancel, reverse-to-minimize, remove) must
/// emit [`LeaseEvent::Reveal`] so the tile cannot stay suppressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseEvent<Id> {
    /// Suppress live-tile drawing for this window id.
    Suppress(Id),
    /// Allow live-tile drawing again.
    Reveal(Id),
}

#[derive(Debug)]
struct ActiveEntry<Id> {
    id: Id,
    direction: LifecycleAnimDirection,
    animation: MinimizeWindowAnimation,
    /// True while restore overlay owns visibility (live tile suppressed).
    holds_visibility_lease: bool,
}

/// One minimize/restore controller instance per space (floating or scrolling).
#[derive(Debug)]
pub struct MinimizeRestoreController<Id> {
    /// At most one active animation per window id. Reverse reuses the same entry/texture.
    entries: Vec<ActiveEntry<Id>>,
}

impl<Id> Default for MinimizeRestoreController<Id> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id> MinimizeRestoreController<Id> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.animation.are_animations_ongoing())
    }
}

impl<Id: Clone + PartialEq + Debug> MinimizeRestoreController<Id> {
    pub fn has_minimize(&self, id: &Id) -> bool {
        self.entries
            .iter()
            .any(|e| e.id == *id && e.direction == LifecycleAnimDirection::Minimize)
    }

    pub fn has_restore(&self, id: &Id) -> bool {
        self.entries
            .iter()
            .any(|e| e.id == *id && e.direction == LifecycleAnimDirection::Restore)
    }

    pub fn has_any(&self, id: &Id) -> bool {
        self.entries.iter().any(|e| e.id == *id)
    }

    /// Drop any animation for `id`. Returns [`LeaseEvent::Reveal`] if a restore lease was held.
    pub fn clear(&mut self, id: &Id) -> Option<LeaseEvent<Id>> {
        let idx = self.entries.iter().position(|e| e.id == *id)?;
        let entry = self.entries.remove(idx);
        entry
            .holds_visibility_lease
            .then(|| LeaseEvent::Reveal(entry.id))
    }

    /// Reverse an active restore animation into minimize, reusing the same snapshot/texture.
    ///
    /// Returns [`LeaseEvent::Reveal`] when a restore lease is released.
    pub fn reverse_to_minimize(
        &mut self,
        id: &Id,
        config: niri_config::Animation,
        target_rect: Option<OutputLocalRect>,
    ) -> Option<LeaseEvent<Id>> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.id == *id && e.direction == LifecycleAnimDirection::Restore)?;
        let mut entry = self.entries.remove(idx);
        entry
            .animation
            .reverse_to_minimize(config, target_rect.map(|r| r.to_f64()));
        entry.direction = LifecycleAnimDirection::Minimize;
        let event = if entry.holds_visibility_lease {
            entry.holds_visibility_lease = false;
            Some(LeaseEvent::Reveal(entry.id.clone()))
        } else {
            None
        };
        // Push to end so stacking matches the previous dual-list take+push behavior.
        self.entries.push(entry);
        event
    }

    /// Reverse an active minimize animation into restore, reusing the same snapshot/texture.
    ///
    /// Returns [`LeaseEvent::Suppress`] so the adapter can hide the live tile.
    pub fn reverse_to_restore(
        &mut self,
        id: &Id,
        config: niri_config::Animation,
        source_rect: Option<OutputLocalRect>,
    ) -> Option<LeaseEvent<Id>> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.id == *id && e.direction == LifecycleAnimDirection::Minimize)?;
        let mut entry = self.entries.remove(idx);
        entry
            .animation
            .reverse_to_restore(config, source_rect.map(|r| r.to_f64()));
        entry.direction = LifecycleAnimDirection::Restore;
        entry.holds_visibility_lease = true;
        let event = LeaseEvent::Suppress(entry.id.clone());
        self.entries.push(entry);
        Some(event)
    }

    /// Start a new minimize animation. Drops any prior entry for `id` (revealing if leased).
    pub fn start_minimize(
        &mut self,
        renderer: &mut GlesRenderer,
        id: Id,
        snapshot: TileRenderSnapshot,
        scale: Scale<f64>,
        clock: Clock,
        config: niri_config::Animation,
        tile_view_pos: Point<f64, Logical>,
        target_rect: Option<OutputLocalRect>,
    ) -> Option<LeaseEvent<Id>> {
        let prior = self.clear(&id);

        let anim = Animation::new(clock, 0., 1., 0., config);
        let resolve = GenieEndpointResolve::identity();
        let pos = resolve.window_from_view_pos(tile_view_pos);
        let target = target_rect.map(|r| resolve.anchor_from_output_local(r));

        match MinimizeWindowAnimation::new_with_target(renderer, snapshot, scale, pos, anim, target)
        {
            Ok(animation) => {
                self.entries.push(ActiveEntry {
                    id,
                    direction: LifecycleAnimDirection::Minimize,
                    animation,
                    holds_visibility_lease: false,
                });
                prior
            }
            Err(err) => {
                warn!("error creating a minimizing window animation: {err:?}");
                prior
            }
        }
    }

    /// Start a new restore animation. On success returns [`LeaseEvent::Suppress`].
    /// On creation failure returns any prior reveal plus a reveal for the failed attempt
    /// only if a prior suppress was already applied by the caller — adapters must not
    /// suppress before calling this; the Suppress event is the sole lease acquire path.
    pub fn start_restore(
        &mut self,
        renderer: &mut GlesRenderer,
        id: Id,
        snapshot: TileRenderSnapshot,
        scale: Scale<f64>,
        clock: Clock,
        config: niri_config::Animation,
        tile_view_pos: Point<f64, Logical>,
        source_rect: Option<OutputLocalRect>,
    ) -> Option<LeaseEvent<Id>> {
        let prior = self.clear(&id);

        let anim = Animation::new(clock, 0., 1., 0., config);
        let resolve = GenieEndpointResolve::identity();
        let pos = resolve.window_from_view_pos(tile_view_pos);
        let source = source_rect.map(|r| resolve.anchor_from_output_local(r));

        match MinimizeWindowAnimation::new_with_source(renderer, snapshot, scale, pos, anim, source)
        {
            Ok(animation) => {
                self.entries.push(ActiveEntry {
                    id: id.clone(),
                    direction: LifecycleAnimDirection::Restore,
                    animation,
                    holds_visibility_lease: true,
                });
                // Prefer Suppress for the new restore; prior reveal is subsumed (tile
                // must stay suppressed). If there was a prior lease on another entry of
                // the same id it was already cleared into `prior` — same id cannot hold
                // two leases, so Suppress is correct.
                let _ = prior;
                Some(LeaseEvent::Suppress(id))
            }
            Err(err) => {
                warn!("error creating a restoring window animation: {err:?}");
                // Creation failed: if prior held a lease, reveal; otherwise nothing.
                prior
            }
        }
    }

    /// Advance animations; drop finished ones. Returns Reveal events for finished restores
    /// that held a visibility lease.
    pub fn advance(&mut self) -> Vec<LeaseEvent<Id>> {
        let mut events = Vec::new();
        self.entries.retain_mut(|entry| {
            entry.animation.advance_animations();
            let ongoing = entry.animation.are_animations_ongoing();
            if !ongoing && entry.holds_visibility_lease {
                events.push(LeaseEvent::Reveal(entry.id.clone()));
            }
            ongoing
        });
        events
    }

    /// Iterate active overlays in stable stacking order (oldest first; render typically rev).
    pub fn for_each_overlay(
        &self,
        mut f: impl FnMut(&Id, LifecycleAnimDirection, &MinimizeWindowAnimation),
    ) {
        for entry in &self.entries {
            f(&entry.id, entry.direction, &entry.animation);
        }
    }

    /// Render all overlays into `view_rect` (output-local for Genie; caller chooses space).
    pub fn render_overlays(
        &self,
        mut ctx: RenderCtx<GlesRenderer>,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        mut push: impl FnMut(MinimizeWindowAnimationRenderElement),
    ) {
        for entry in self.entries.iter().rev() {
            push(entry.animation.render(ctx.r(), view_rect, scale));
        }
    }

    /// Number of minimize-direction entries (observation / capacity hints).
    pub fn minimize_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.direction == LifecycleAnimDirection::Minimize)
            .count()
    }

    /// Number of restore-direction entries.
    pub fn restore_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.direction == LifecycleAnimDirection::Restore)
            .count()
    }

    /// Whether any entry holds a visibility lease for `id`.
    pub fn holds_visibility_lease(&self, id: &Id) -> bool {
        self.entries
            .iter()
            .any(|e| e.id == *id && e.holds_visibility_lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_anim(ms: u32) -> niri_config::Animation {
        niri_config::Animation {
            off: false,
            kind: niri_config::animations::Kind::Easing(niri_config::animations::EasingParams {
                duration_ms: ms,
                curve: niri_config::animations::Curve::Linear,
            }),
        }
    }

    #[test]
    fn empty_controller_reports_no_activity() {
        let mut c: MinimizeRestoreController<u32> = MinimizeRestoreController::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert!(!c.are_animations_ongoing());
        assert!(!c.has_minimize(&1));
        assert!(!c.has_restore(&1));
        assert!(!c.has_any(&1));
        assert!(!c.holds_visibility_lease(&1));
        assert_eq!(c.minimize_count(), 0);
        assert_eq!(c.restore_count(), 0);
        assert!(c.clear(&1).is_none());
        assert!(c.advance().is_empty());
    }

    #[test]
    fn reverse_apis_return_none_without_matching_direction() {
        let mut c: MinimizeRestoreController<u32> = MinimizeRestoreController::new();
        let cfg = linear_anim(100);
        assert!(c.reverse_to_minimize(&1, cfg, None).is_none());
        assert!(c.reverse_to_restore(&1, cfg, None).is_none());
    }

    #[test]
    fn lease_event_equality() {
        assert_eq!(LeaseEvent::Suppress(1u32), LeaseEvent::Suppress(1));
        assert_eq!(LeaseEvent::Reveal(2u32), LeaseEvent::Reveal(2));
        assert_ne!(LeaseEvent::Suppress(1u32), LeaseEvent::Reveal(1));
    }
}
