//! Single compositor-internal minimize/restore lifecycle command.
//!
//! Protocol adapters (foreign-toplevel, IPC, xdg-toplevel) only parse the request
//! into a [`LifecycleCommand`]. Cache selection, renderer availability, snapshot
//! animation, fallback model updates, and interactive-move policy live here.

use smithay::desktop::Window;

use crate::layout::MinimizeRect;
use crate::redraw_attribution::RedrawAttribution;

/// Direction of a window lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleDirection {
    Minimize,
    Restore,
}

/// How the lifecycle command obtains an animation anchor.
///
/// Callers must not read or invent anchors themselves when the strategy is
/// "use dock cache" — that is owned by the command.
#[derive(Debug, Clone)]
pub enum LifecycleAnchorInput {
    /// Explicit dock/taskbar rectangle already resolved by the caller.
    Explicit(MinimizeRect),
    /// Use the typed last-hint when it is `Resolved`, non-empty, and its output
    /// matches the window's current output; otherwise degrade to no-anchor
    /// (wrong-output, zero-area, Unresolved, or Cleared). Never reuses an older hint.
    CachedForCurrentOutput,
    /// No Genie target/source; reverse of in-flight animations still works.
    None,
}

/// Invocation metadata for trace/permissions only.
///
/// Must not change animation strategy, snapshot policy, or model transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleInvocationSource {
    ForeignToplevel,
    Ipc,
    XdgToplevel,
    #[cfg(test)]
    Test,
}

/// Compositor-internal lifecycle request.
#[derive(Debug, Clone)]
pub struct LifecycleCommand {
    pub window: Window,
    pub direction: LifecycleDirection,
    pub anchor: LifecycleAnchorInput,
    pub source: LifecycleInvocationSource,
}

impl LifecycleCommand {
    pub fn minimize(
        window: Window,
        anchor: LifecycleAnchorInput,
        source: LifecycleInvocationSource,
    ) -> Self {
        Self {
            window,
            direction: LifecycleDirection::Minimize,
            anchor,
            source,
        }
    }

    pub fn restore(
        window: Window,
        anchor: LifecycleAnchorInput,
        source: LifecycleInvocationSource,
    ) -> Self {
        Self {
            window,
            direction: LifecycleDirection::Restore,
            anchor,
            source,
        }
    }
}

/// Result of executing a lifecycle command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCommandOutcome {
    /// Model state and/or animation changed.
    Applied,
    /// Window missing, already in target state, restore rejected during interactive
    /// move, or other intentional no-op.
    NoOp,
}

/// Unified command result for adapters (protocol response + redraw attribution).
///
/// Adapters must apply [`Self::redraw`] via
/// [`crate::niri::Niri::apply_redraw_attribution`] and must not invent a parallel
/// `queue_redraw_all` path for lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleCommandResult {
    pub outcome: LifecycleCommandOutcome,
    pub redraw: RedrawAttribution,
}

impl LifecycleCommandResult {
    pub fn applied(redraw: RedrawAttribution) -> Self {
        Self {
            outcome: LifecycleCommandOutcome::Applied,
            redraw,
        }
    }

    pub fn no_op() -> Self {
        Self {
            outcome: LifecycleCommandOutcome::NoOp,
            redraw: RedrawAttribution::none(),
        }
    }

    /// Whether adapters should treat this as a successful state change.
    pub fn changed(&self) -> bool {
        matches!(self.outcome, LifecycleCommandOutcome::Applied)
    }
}
