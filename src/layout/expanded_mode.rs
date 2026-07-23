//! Workspace expanded-mode orchestration (fullscreen / maximize / return placement).
//!
//! Ownership:
//! - This module owns **typed return placement** and **pure orchestration decisions** for
//!   entering and leaving maximized/fullscreen, including floating ↔ scrolling migration
//!   when the window must return to floating.
//! - Column still owns layout size calculation (`is_pending_fullscreen` /
//!   `is_pending_maximized` + tile sizes).
//! - Mapped / window still owns protocol pending and committed sizing mode.
//! - Workspace is the sole production executor of these decisions (public
//!   `set_fullscreen` / `set_maximized` / top-snap / interactive-move entry points).
//! - R08 `MaximizeVisualFsm` remains the visual exclusivity owner; this module does not
//!   duplicate pending/committed visual phases.
//!
//! Desired mode (request), committed mode (protocol), and visual transition (R08 FSM) are
//! intentionally separate. Fullscreen and maximized remain non-exclusive at the column
//! level: unfullscreen may restore maximized before any float-return.

use std::fmt::Debug;

/// Where a window should return when leaving expanded mode (maximize and/or fullscreen).
///
/// Replaces the bare `restore_to_floating: bool`. `Floating` means the window entered
/// expanded mode from the floating space (or was inserted with floating return intent,
/// e.g. top-snap from a floating interactive move) and must float again once both
/// maximized and fullscreen are cleared. While still maximized after unfullscreen, the
/// window stays in scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReturnPlacement {
    /// Remain in / return to the scrolling layout.
    #[default]
    Scrolling,
    /// Return to floating when fully leaving expanded mode.
    Floating,
}

impl ReturnPlacement {
    /// Capture from a floating vs scrolling source (transport or current space).
    pub fn from_is_floating(is_floating: bool) -> Self {
        if is_floating {
            Self::Floating
        } else {
            Self::Scrolling
        }
    }

    pub fn is_floating(self) -> bool {
        matches!(self, Self::Floating)
    }
}

/// Kind of expanded-mode request Workspace orchestrates.
///
/// Distinct from window `SizingMode` (committed/pending protocol value) and from R08
/// visual FSM phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedModeKind {
    Fullscreen,
    Maximized,
}

/// Plan produced by pure decisions for Workspace to execute with its space adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedModePlan {
    /// Request is a no-op (e.g. unfullscreen while already floating/unfullscreen).
    NoOp,
    /// Leave expanded mode by moving floating in one step (combined unexpand + float so
    /// the window can request a floating size rather than a column size).
    ExitToFloating,
    /// Window is in (or will be moved into) scrolling; apply mode there and maybe capture
    /// return placement when leaving Normal.
    ApplyInScrolling {
        /// Move floating → scrolling before applying the mode.
        move_from_floating: bool,
        /// Value written into column pending fullscreen/maximized.
        enable: bool,
        /// If `Some`, write this return placement after a Normal → expanded transition.
        capture_return: Option<ReturnPlacement>,
    },
}

/// Pure orchestration decisions. No layout mutation.
pub struct ExpandedModeOrchestrator;

impl ExpandedModeOrchestrator {
    /// Plan a fullscreen request.
    ///
    /// `column_pending_fullscreen` / `column_pending_maximized` are required only when the
    /// window is in scrolling and `enable` is false (exit path).
    pub fn plan_fullscreen(
        currently_floating: bool,
        enable: bool,
        column_pending_fullscreen: bool,
        column_pending_maximized: bool,
        return_placement: ReturnPlacement,
        window_pending_is_normal: bool,
    ) -> ExpandedModePlan {
        if currently_floating {
            if enable {
                return ExpandedModePlan::ApplyInScrolling {
                    move_from_floating: true,
                    enable: true,
                    // Capture after apply if we left Normal (always when entering from floating
                    // normal; caller re-checks was_normal after apply for safety).
                    capture_return: Some(ReturnPlacement::Floating),
                };
            }
            // Floating windows are never fullscreen.
            return ExpandedModePlan::NoOp;
        }

        if !enable {
            // Going fullscreen → maximized keeps return placement for a later unmaximize.
            if should_exit_to_floating_on_unfullscreen(
                column_pending_fullscreen,
                column_pending_maximized,
                return_placement,
            ) {
                return ExpandedModePlan::ExitToFloating;
            }
        }

        let capture_return = if enable && window_pending_is_normal {
            // Entering from scrolling Normal → Scrolling return (unless caller already set
            // Floating via transport / prior expanded stint — only capture when was Normal,
            // which means overwrite with Scrolling when not moving from floating).
            Some(ReturnPlacement::Scrolling)
        } else {
            None
        };

        ExpandedModePlan::ApplyInScrolling {
            move_from_floating: false,
            enable,
            capture_return,
        }
    }

    /// Plan a maximize request.
    pub fn plan_maximized(
        currently_floating: bool,
        enable: bool,
        window_pending_is_maximized: bool,
        return_placement: ReturnPlacement,
        window_pending_is_normal: bool,
    ) -> ExpandedModePlan {
        if currently_floating {
            if enable {
                return ExpandedModePlan::ApplyInScrolling {
                    move_from_floating: true,
                    enable: true,
                    capture_return: Some(ReturnPlacement::Floating),
                };
            }
            // Floating windows are never maximized.
            return ExpandedModePlan::NoOp;
        }

        if !enable
            && should_exit_to_floating_on_unmaximize(window_pending_is_maximized, return_placement)
        {
            return ExpandedModePlan::ExitToFloating;
        }

        let capture_return = if enable && window_pending_is_normal {
            Some(ReturnPlacement::Scrolling)
        } else {
            None
        };

        ExpandedModePlan::ApplyInScrolling {
            move_from_floating: false,
            enable,
            capture_return,
        }
    }

    /// After applying mode in scrolling: whether to write `capture_return` onto the tile.
    ///
    /// Only when the window left Normal (same gate as the historical was_normal check).
    pub fn should_write_return_placement(was_normal: bool, now_normal: bool) -> bool {
        was_normal && !now_normal
    }

    /// Interactive move treats maximized windows with floating return as floating drags.
    pub fn interactive_move_treat_as_floating(
        is_currently_floating: bool,
        return_placement: ReturnPlacement,
        window_pending_is_maximized: bool,
    ) -> bool {
        is_currently_floating || (return_placement.is_floating() && window_pending_is_maximized)
    }
}

/// Unfullscreen should immediately float only when exit target is Normal (not Maximized).
pub fn should_exit_to_floating_on_unfullscreen(
    column_pending_fullscreen: bool,
    column_pending_maximized: bool,
    return_placement: ReturnPlacement,
) -> bool {
    column_pending_fullscreen && !column_pending_maximized && return_placement.is_floating()
}

/// Unmaximize should immediately float when the window's pending mode is maximized
/// (not fullscreen — pending sizing mode is fullscreen while both flags are set).
pub fn should_exit_to_floating_on_unmaximize(
    window_pending_is_maximized: bool,
    return_placement: ReturnPlacement,
) -> bool {
    window_pending_is_maximized && return_placement.is_floating()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfullscreen_to_maximized_does_not_float() {
        assert!(!should_exit_to_floating_on_unfullscreen(
            true,
            true,
            ReturnPlacement::Floating,
        ));
    }

    #[test]
    fn unfullscreen_to_normal_with_floating_return_floats() {
        assert!(should_exit_to_floating_on_unfullscreen(
            true,
            false,
            ReturnPlacement::Floating,
        ));
    }

    #[test]
    fn unfullscreen_with_scrolling_return_stays() {
        assert!(!should_exit_to_floating_on_unfullscreen(
            true,
            false,
            ReturnPlacement::Scrolling,
        ));
    }

    #[test]
    fn unmaximize_during_fullscreen_pending_is_not_maximized() {
        // Window pending sizing is fullscreen while column has max+fs; unmaximize must not float.
        assert!(!should_exit_to_floating_on_unmaximize(
            false,
            ReturnPlacement::Floating,
        ));
    }

    #[test]
    fn unmaximize_with_floating_return_floats() {
        assert!(should_exit_to_floating_on_unmaximize(
            true,
            ReturnPlacement::Floating,
        ));
    }

    #[test]
    fn plan_fullscreen_from_floating_moves_and_captures() {
        let plan = ExpandedModeOrchestrator::plan_fullscreen(
            true,
            true,
            false,
            false,
            ReturnPlacement::Scrolling,
            true,
        );
        assert_eq!(
            plan,
            ExpandedModePlan::ApplyInScrolling {
                move_from_floating: true,
                enable: true,
                capture_return: Some(ReturnPlacement::Floating),
            }
        );
    }

    #[test]
    fn plan_unfullscreen_exit_to_floating() {
        let plan = ExpandedModeOrchestrator::plan_fullscreen(
            false,
            false,
            true,
            false,
            ReturnPlacement::Floating,
            false,
        );
        assert_eq!(plan, ExpandedModePlan::ExitToFloating);
    }

    #[test]
    fn plan_unfullscreen_while_maximized_stays_scrolling() {
        let plan = ExpandedModeOrchestrator::plan_fullscreen(
            false,
            false,
            true,
            true,
            ReturnPlacement::Floating,
            false,
        );
        assert_eq!(
            plan,
            ExpandedModePlan::ApplyInScrolling {
                move_from_floating: false,
                enable: false,
                capture_return: None,
            }
        );
    }

    #[test]
    fn plan_unmaximize_exit_to_floating() {
        let plan = ExpandedModeOrchestrator::plan_maximized(
            false,
            false,
            true,
            ReturnPlacement::Floating,
            false,
        );
        assert_eq!(plan, ExpandedModePlan::ExitToFloating);
    }

    #[test]
    fn plan_unmaximize_on_floating_is_noop() {
        let plan = ExpandedModeOrchestrator::plan_maximized(
            true,
            false,
            false,
            ReturnPlacement::Scrolling,
            true,
        );
        assert_eq!(plan, ExpandedModePlan::NoOp);
    }

    #[test]
    fn interactive_move_treats_maximized_floating_return_as_floating() {
        assert!(
            ExpandedModeOrchestrator::interactive_move_treat_as_floating(
                false,
                ReturnPlacement::Floating,
                true,
            )
        );
        assert!(
            !ExpandedModeOrchestrator::interactive_move_treat_as_floating(
                false,
                ReturnPlacement::Scrolling,
                true,
            )
        );
        assert!(
            !ExpandedModeOrchestrator::interactive_move_treat_as_floating(
                false,
                ReturnPlacement::Floating,
                false,
            )
        );
    }

    #[test]
    fn write_return_only_when_leaving_normal() {
        assert!(ExpandedModeOrchestrator::should_write_return_placement(
            true, false
        ));
        assert!(!ExpandedModeOrchestrator::should_write_return_placement(
            false, false
        ));
        assert!(!ExpandedModeOrchestrator::should_write_return_placement(
            true, true
        ));
    }
}
