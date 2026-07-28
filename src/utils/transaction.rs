use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use atomic::Ordering;
use calloop::ping::{make_ping, Ping};
use calloop::timer::{TimeoutAction, Timer};
use calloop::LoopHandle;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::compositor::{Blocker, BlockerState};

/// Default time limit, after which the transaction completes.
///
/// Serves to avoid hanging when a client fails to respond to a configure promptly.
///
/// A-8: this also bounds the worst-case stall before a close animation starts playing, since the
/// closing snapshot hangs motionless in `AnimationState::Waiting` until the transaction releases.
/// 300 ms was well past the point where that reads as frozen rather than deliberate.
///
/// What the budget actually has to cover is not just a configure round-trip: `xdg_shell` holds the
/// transaction until the client's dmabuf fence signals, so this is also a client GPU-time budget
/// for the first frame after a resize (buffer reallocation, shader compile, large surfaces). If it
/// fires early the transaction force-completes and siblings apply their new sizes while the slow
/// window still presents its old buffer — a transient, self-correcting intra-column size desync.
/// 150 ms trades a rarer instance of that against never stalling a close for a fifth of a second.
const TIME_LIMIT: Duration = Duration::from_millis(150);

/// Transaction between Wayland clients.
///
/// How to use it:
/// 1. Create a transaction with [`Transaction::new()`].
/// 2. Clone it as many times as you need.
/// 3. Before adding the transaction as a commit blocker, remember to call
///    [`Transaction::add_notification()`] to receive a notification when the transaction completes.
/// 4. Before adding the transaction as a commit blocker, remember to call
///    [`Transaction::register_deadline_timer()`] to make sure the transaction completes when
///    reaching the deadline.
/// 5. In your surface pre-commit handler, if the transaction corresponding to that commit isn't
///    ready, get a blocker with [`Transaction::blocker()`] and add it to the surface.
#[derive(Debug, Clone)]
pub struct Transaction {
    inner: Arc<Inner>,
    deadline: Rc<RefCell<Deadline>>,
}

/// Blocker for a [`Transaction`].
#[derive(Debug)]
pub struct TransactionBlocker(Weak<Inner>);

#[derive(Debug)]
enum Deadline {
    NotRegistered(Instant),
    Registered { remove: Ping },
}

#[derive(Debug)]
struct Inner {
    /// Whether the transaction is completed.
    completed: AtomicBool,
    /// Notifications to send out upon completing the transaction.
    notifications: Mutex<Option<(Sender<Client>, Vec<Client>)>>,
}

impl Transaction {
    /// Creates a new transaction.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
            deadline: Rc::new(RefCell::new(Deadline::NotRegistered(
                Instant::now() + TIME_LIMIT,
            ))),
        }
    }

    /// Gets a blocker for this transaction.
    pub fn blocker(&self) -> TransactionBlocker {
        trace!(transaction = ?Arc::as_ptr(&self.inner), "generating blocker");
        TransactionBlocker(Arc::downgrade(&self.inner))
    }

    /// Adds a notification for when this transaction completes.
    pub fn add_notification(&self, sender: Sender<Client>, client: Client) {
        if self.is_completed() {
            error!("tried to add notification to a completed transaction");
            return;
        }

        let mut guard = self.inner.notifications.lock().unwrap();
        guard.get_or_insert((sender, Vec::new())).1.push(client);
    }

    /// Registers this transaction's deadline timer on an event loop.
    pub fn register_deadline_timer<T: 'static>(&self, event_loop: &LoopHandle<'static, T>) {
        let mut cell = self.deadline.borrow_mut();
        if let Deadline::NotRegistered(deadline) = *cell {
            let timer = Timer::from_deadline(deadline);
            let inner = Arc::downgrade(&self.inner);
            let token = event_loop
                .insert_source(timer, move |_, _, _| {
                    let _span = trace_span!("deadline timer", transaction = ?Weak::as_ptr(&inner))
                        .entered();

                    // FIXME: come up with some way to control the deadline timer from tests.
                    #[cfg(not(test))]
                    if let Some(inner) = inner.upgrade() {
                        trace!("deadline reached, completing transaction");
                        inner.complete();
                    } else {
                        // We should remove the timer automatically. But this callback can still
                        // just happen to run while the ping callback is scheduled, leading to this
                        // branch being legitimately taken.
                        trace!("transaction completed without removing the timer");
                    }

                    TimeoutAction::Drop
                })
                .unwrap();

            // Add a ping source that will be used to remove the timer automatically.
            let (ping, source) = make_ping().unwrap();
            let loop_handle = event_loop.clone();
            event_loop
                .insert_source(source, move |_, _, _| {
                    loop_handle.remove(token);
                })
                .unwrap();

            *cell = Deadline::Registered { remove: ping };
        }
    }

    /// Returns whether this transaction has already completed.
    pub fn is_completed(&self) -> bool {
        self.inner.is_completed()
    }

    /// Returns whether this is the last instance of this transaction.
    pub fn is_last(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _span = trace_span!("drop", transaction = ?Arc::as_ptr(&self.inner)).entered();

        if self.is_last() {
            // If this was the last transaction, complete it.
            trace!("last transaction dropped, completing");
            self.inner.complete();

            // Also remove the timer.
            if let Deadline::Registered { remove } = &*self.deadline.borrow() {
                remove.ping();
            };
        }
    }
}

impl TransactionBlocker {
    pub fn completed() -> Self {
        Self(Weak::new())
    }
}

impl Blocker for TransactionBlocker {
    fn state(&self) -> BlockerState {
        if self.0.upgrade().is_none_or(|x| x.is_completed()) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

impl Inner {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            notifications: Mutex::new(None),
        }
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Relaxed)
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Relaxed);

        let mut guard = self.notifications.lock().unwrap();
        if let Some((sender, clients)) = guard.take() {
            for client in clients {
                if let Err(err) = sender.send(client) {
                    warn!("error sending blocker notification: {err:?}");
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_is_short_enough_for_a_close_animation() {
        // A-8. This is a change detector on the constant, not a behavioural test: the deadline
        // timer body is `#[cfg(not(test))]`, so a transaction *firing* on its deadline cannot be
        // exercised from here at all. What it guards is the budget either side.
        //
        // Upper bound: a closing window's snapshot hangs motionless until its transaction
        // releases, so a regression back toward 300 ms is a visible stall, not a config tweak.
        assert!(
            TIME_LIMIT <= Duration::from_millis(150),
            "TIME_LIMIT is the worst-case delay before a close animation plays; {TIME_LIMIT:?} \
             exceeds the 150 ms budget"
        );

        // Lower bound: the transaction also waits on the client's dmabuf fence, so cutting this
        // much further starts force-completing legitimately-slow first frames after a resize.
        assert!(
            TIME_LIMIT >= Duration::from_millis(100),
            "TIME_LIMIT {TIME_LIMIT:?} leaves too little room for a slow client's first frame"
        );

        // And a fresh transaction's deadline really is derived from the constant.
        let transaction = Transaction::new();
        let Deadline::NotRegistered(deadline) = *transaction.deadline.borrow() else {
            panic!("a fresh transaction must have an unregistered deadline");
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining <= TIME_LIMIT && remaining + Duration::from_millis(50) >= TIME_LIMIT,
            "fresh transaction deadline is {remaining:?} away, expected ≈{TIME_LIMIT:?}"
        );
    }
}
