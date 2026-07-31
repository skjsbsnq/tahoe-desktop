//! Unified redraw attribution for lifecycle / foreign-toplevel / glass clusters.
//!
//! Protocol adapters must not invent output sets or call [`crate::niri::Niri::queue_redraw_all`]
//! directly for locatable events. Command/controller owners return a
//! [`RedrawAttribution`]; [`crate::niri::Niri::apply_redraw_attribution`] is the sole
//! consumer that schedules frames.
//!
//! Fallback to all-outputs is only allowed for reviewed reasons
//! ([`RedrawFallbackReason`]); each use increments a reason counter for observation.

use smithay::output::Output;

use crate::utils::lifecycle_diag;

/// Targeted redraw cause returned by a cluster owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedrawReason {
    /// Minimize / restore model or animation change.
    Lifecycle,
    /// Window activation / focus hand-off.
    Activate,
    /// Maximize / unmaximize (expanded-mode) layout change.
    Maximize,
    /// Tahoe glass surface content or lifecycle change.
    Glass,
    /// Generic layout/workspace action cluster (`Input::do_action` tail).
    Action,
}

/// Reviewed reasons that may schedule a redraw on every output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedrawFallbackReason {
    /// Window or surface root could not be mapped to an output.
    Unlocatable,
    /// Output is being removed / torn down and residual damage is global.
    OutputTeardown,
    /// Global configuration change that affects all outputs.
    GlobalConfig,
    /// Global UI overlay / debug state that spans all outputs (overview, debug
    /// tint/damage, screenshot UI, hotkey overlay, screen transition).
    GlobalUi,
}

/// Result of attributing a cluster event to affected outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedrawAttribution {
    /// No frame needed (no-op command, pure cache write, or empty affect set).
    None,
    /// Explicit set of outputs that must submit a frame.
    Outputs {
        outputs: Vec<Output>,
        reason: RedrawReason,
    },
    /// All outputs; only for [`RedrawFallbackReason`] cases.
    All { reason: RedrawFallbackReason },
}

impl RedrawAttribution {
    pub fn none() -> Self {
        Self::None
    }

    pub fn outputs(outputs: impl IntoIterator<Item = Output>, reason: RedrawReason) -> Self {
        let mut list: Vec<Output> = Vec::new();
        for output in outputs {
            if !list.iter().any(|o| o == &output) {
                list.push(output);
            }
        }
        if list.is_empty() {
            Self::None
        } else {
            Self::Outputs {
                outputs: list,
                reason,
            }
        }
    }

    /// Single locatable output, or fallback when the window has no output.
    pub fn window_output_or_unlocatable(output: Option<&Output>, reason: RedrawReason) -> Self {
        match output {
            Some(output) => Self::outputs([output.clone()], reason),
            None => Self::All {
                reason: RedrawFallbackReason::Unlocatable,
            },
        }
    }

    /// Home output plus any additional outputs that also changed (e.g. previous focus).
    pub fn window_and_related(
        window_output: Option<&Output>,
        related: impl IntoIterator<Item = Output>,
        reason: RedrawReason,
    ) -> Self {
        let mut list: Vec<Output> = Vec::new();
        if let Some(output) = window_output {
            list.push(output.clone());
        }
        for output in related {
            if !list.iter().any(|o| o == &output) {
                list.push(output);
            }
        }
        if list.is_empty() {
            Self::All {
                reason: RedrawFallbackReason::Unlocatable,
            }
        } else {
            Self::Outputs {
                outputs: list,
                reason,
            }
        }
    }

    pub fn all(reason: RedrawFallbackReason) -> Self {
        Self::All { reason }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Self::All { .. })
    }

    pub fn outputs_slice(&self) -> &[Output] {
        match self {
            Self::Outputs { outputs, .. } => outputs.as_slice(),
            _ => &[],
        }
    }

    pub fn targeted_reason(&self) -> Option<RedrawReason> {
        match self {
            Self::Outputs { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    pub fn fallback_reason(&self) -> Option<RedrawFallbackReason> {
        match self {
            Self::All { reason } => Some(*reason),
            _ => None,
        }
    }
}

/// Record a reviewed all-outputs fallback for diagnostics.
pub fn note_fallback_reason(reason: RedrawFallbackReason) {
    match reason {
        RedrawFallbackReason::Unlocatable => {
            lifecycle_diag::note_redraw_fallback_unlocatable();
        }
        RedrawFallbackReason::OutputTeardown => {
            lifecycle_diag::note_redraw_fallback_output_teardown();
        }
        RedrawFallbackReason::GlobalConfig => {
            lifecycle_diag::note_redraw_fallback_global_config();
        }
        RedrawFallbackReason::GlobalUi => {
            lifecycle_diag::note_redraw_fallback_global_ui();
        }
    }
}

/// Record a targeted cluster redraw for diagnostics.
pub fn note_targeted_reason(reason: RedrawReason) {
    match reason {
        RedrawReason::Lifecycle => lifecycle_diag::note_redraw_targeted_lifecycle(),
        RedrawReason::Activate => lifecycle_diag::note_redraw_targeted_activate(),
        RedrawReason::Maximize => lifecycle_diag::note_redraw_targeted_maximize(),
        RedrawReason::Glass => lifecycle_diag::note_redraw_targeted_glass(),
        RedrawReason::Action => lifecycle_diag::note_redraw_targeted_action(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_outputs_collapses_to_none() {
        assert!(RedrawAttribution::outputs(std::iter::empty(), RedrawReason::Lifecycle).is_none());
    }

    #[test]
    fn window_output_missing_is_unlocatable_fallback() {
        let attr = RedrawAttribution::window_output_or_unlocatable(None, RedrawReason::Maximize);
        assert_eq!(
            attr.fallback_reason(),
            Some(RedrawFallbackReason::Unlocatable)
        );
    }
}
