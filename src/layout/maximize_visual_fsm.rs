//! Maximize visual transition FSM (scrolling exclusivity + settling).
//!
//! Ownership:
//! - This module owns the **visual** maximize transition phase only: live-tile exclusivity
//!   while entering maximized, timeout fallback that temporarily releases exclusivity, and
//!   settle/cancel completion.
//! - Column/Mapped still own pending and committed maximized (protocol sizing). This FSM does
//!   not invent a second sizing model.
//!
//! Phases intentionally exclude the old bool pair `committed && timed_out` (ambiguous under the
//! previous `MaximizeTransition { committed, timed_out }` record). Cancelled and Finished clear
//! the `Option` holder on [`ScrollingSpace`](super::scrolling::ScrollingSpace) rather than
//! lingering as internal phases that still look "active".

use std::fmt::Debug;
use std::time::Duration;

/// Do not let an unresponsive client hide the rest of the workspace indefinitely.
pub const MAXIMIZE_PENDING_TIMEOUT: Duration = Duration::from_secs(1);

/// Explicit visual phase of one maximize transition.
///
/// Distinct from window pending/committed sizing: those live on Column/Mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeVisualPhase {
    /// Maximize requested; waiting for the **target** window to commit maximized.
    PendingConfigure,
    /// Target committed maximized; waiting for tile/view movement animations to settle.
    CommittedSettling,
    /// Client did not commit within [`MAXIMIZE_PENDING_TIMEOUT`]; exclusivity released so other
    /// tiles are visible, but the record is kept so a late *valid* maximize commit can resume
    /// into [`CommittedSettling`](MaximizeVisualPhase::CommittedSettling).
    TimedOutVisibleFallback,
}

/// One scrolling-space maximize visual transition.
#[derive(Debug, Clone)]
pub struct MaximizeVisualFsm<Id> {
    window: Id,
    started_at: Duration,
    phase: MaximizeVisualPhase,
}

/// Why the holder should clear the transition (`Option` → `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeVisualClear {
    /// Unmaximize, fullscreen, target removed, new request replaced, or pending cancelled.
    Cancelled,
    /// Target committed maximized and tile/view animations settled.
    Finished,
}

/// Test/diag observation of the production FSM (no parallel test state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeTransitionObservation {
    /// No active record (never started, [`Cancelled`](MaximizeVisualClear::Cancelled), or
    /// [`Finished`](MaximizeVisualClear::Finished)).
    Idle,
    PendingConfigure,
    CommittedSettling,
    TimedOutVisibleFallback,
}

impl From<MaximizeVisualPhase> for MaximizeTransitionObservation {
    fn from(phase: MaximizeVisualPhase) -> Self {
        match phase {
            MaximizeVisualPhase::PendingConfigure => Self::PendingConfigure,
            MaximizeVisualPhase::CommittedSettling => Self::CommittedSettling,
            MaximizeVisualPhase::TimedOutVisibleFallback => Self::TimedOutVisibleFallback,
        }
    }
}

impl<Id> MaximizeVisualFsm<Id> {
    /// Start a transition for a maximize request.
    ///
    /// If the target tile is already committed maximized (e.g. re-maximize without a client
    /// round-trip), enter [`CommittedSettling`](MaximizeVisualPhase::CommittedSettling) directly.
    pub fn begin(window: Id, started_at: Duration, already_committed_maximized: bool) -> Self {
        Self {
            window,
            started_at,
            phase: if already_committed_maximized {
                MaximizeVisualPhase::CommittedSettling
            } else {
                MaximizeVisualPhase::PendingConfigure
            },
        }
    }

    pub fn window(&self) -> &Id {
        &self.window
    }

    pub fn phase(&self) -> MaximizeVisualPhase {
        self.phase
    }

    pub fn started_at(&self) -> Duration {
        self.started_at
    }

    pub fn observation(&self) -> MaximizeTransitionObservation {
        self.phase.into()
    }

    /// Live-tile exclusivity (and floating live suppression) while entering maximize.
    ///
    /// False only in [`TimedOutVisibleFallback`](MaximizeVisualPhase::TimedOutVisibleFallback);
    /// late valid target commit re-enables exclusivity via
    /// [`on_target_maximized_commit`](Self::on_target_maximized_commit).
    pub fn exclusivity_active(&self) -> bool {
        matches!(
            self.phase,
            MaximizeVisualPhase::PendingConfigure | MaximizeVisualPhase::CommittedSettling
        )
    }

    /// Target window committed maximized under Mapped configure/ack/commit serial semantics.
    ///
    /// Applies to both pending and timed-out phases (late valid commit resumes exclusivity).
    pub fn on_target_maximized_commit(&mut self) {
        match self.phase {
            MaximizeVisualPhase::PendingConfigure
            | MaximizeVisualPhase::TimedOutVisibleFallback
            | MaximizeVisualPhase::CommittedSettling => {
                self.phase = MaximizeVisualPhase::CommittedSettling;
            }
        }
    }

    /// Advance wall-clock (unadjusted). May enter
    /// [`TimedOutVisibleFallback`](MaximizeVisualPhase::TimedOutVisibleFallback) from
    /// [`PendingConfigure`](MaximizeVisualPhase::PendingConfigure).
    pub fn on_clock_tick(&mut self, now: Duration) {
        if self.phase != MaximizeVisualPhase::PendingConfigure {
            return;
        }
        if now.saturating_sub(self.started_at) >= MAXIMIZE_PENDING_TIMEOUT {
            self.phase = MaximizeVisualPhase::TimedOutVisibleFallback;
        }
    }

    /// Whether pending has expired for `now` without mutating (for tests / diagnostics).
    pub fn would_timeout_at(&self, now: Duration) -> bool {
        self.phase == MaximizeVisualPhase::PendingConfigure
            && now.saturating_sub(self.started_at) >= MAXIMIZE_PENDING_TIMEOUT
    }
}

impl<Id: PartialEq> MaximizeVisualFsm<Id> {
    pub fn targets(&self, window: &Id) -> bool {
        &self.window == window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_pending_vs_already_committed() {
        let pending = MaximizeVisualFsm::begin(1usize, Duration::ZERO, false);
        assert_eq!(pending.phase(), MaximizeVisualPhase::PendingConfigure);
        assert!(pending.exclusivity_active());
        assert_eq!(
            pending.observation(),
            MaximizeTransitionObservation::PendingConfigure
        );

        let committed = MaximizeVisualFsm::begin(1usize, Duration::ZERO, true);
        assert_eq!(committed.phase(), MaximizeVisualPhase::CommittedSettling);
        assert!(committed.exclusivity_active());
    }

    #[test]
    fn timeout_boundary_1000_vs_1001_ms_unadjusted() {
        let mut fsm = MaximizeVisualFsm::begin(7usize, Duration::ZERO, false);

        fsm.on_clock_tick(Duration::from_millis(999));
        assert_eq!(fsm.phase(), MaximizeVisualPhase::PendingConfigure);
        assert!(fsm.exclusivity_active());

        fsm.on_clock_tick(Duration::from_millis(1000));
        assert_eq!(fsm.phase(), MaximizeVisualPhase::TimedOutVisibleFallback);
        assert!(!fsm.exclusivity_active());

        // Re-begin for exact 1001 path from pending.
        let mut fsm = MaximizeVisualFsm::begin(7usize, Duration::ZERO, false);
        fsm.on_clock_tick(Duration::from_millis(1001));
        assert_eq!(fsm.phase(), MaximizeVisualPhase::TimedOutVisibleFallback);
    }

    #[test]
    fn late_commit_after_timeout_resumes_committed_settling() {
        let mut fsm = MaximizeVisualFsm::begin(1usize, Duration::ZERO, false);
        fsm.on_clock_tick(Duration::from_millis(1001));
        assert_eq!(fsm.phase(), MaximizeVisualPhase::TimedOutVisibleFallback);
        assert!(!fsm.exclusivity_active());

        fsm.on_target_maximized_commit();
        assert_eq!(fsm.phase(), MaximizeVisualPhase::CommittedSettling);
        assert!(fsm.exclusivity_active());
        // No path yields committed && timed_out simultaneously.
        assert_ne!(
            fsm.observation(),
            MaximizeTransitionObservation::TimedOutVisibleFallback
        );
    }

    #[test]
    fn commit_while_pending_enters_settling() {
        let mut fsm = MaximizeVisualFsm::begin(1usize, Duration::ZERO, false);
        fsm.on_target_maximized_commit();
        assert_eq!(fsm.phase(), MaximizeVisualPhase::CommittedSettling);
        // Timeout must not fire once committed.
        fsm.on_clock_tick(Duration::from_secs(10));
        assert_eq!(fsm.phase(), MaximizeVisualPhase::CommittedSettling);
    }

    #[test]
    fn observation_never_idle_while_record_exists() {
        for phase in [
            MaximizeVisualPhase::PendingConfigure,
            MaximizeVisualPhase::CommittedSettling,
            MaximizeVisualPhase::TimedOutVisibleFallback,
        ] {
            let mut fsm = MaximizeVisualFsm::begin(1usize, Duration::ZERO, false);
            fsm.phase = phase;
            assert_ne!(
                fsm.observation(),
                MaximizeTransitionObservation::Idle,
                "phase {phase:?}"
            );
        }
    }
}
