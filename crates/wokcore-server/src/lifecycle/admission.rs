use std::{
    fmt,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
};

use tokio::sync::Notify;

pub(crate) const PHASE_STARTING: u8 = 0;
pub(crate) const PHASE_RUNNING: u8 = 1;
pub(crate) const PHASE_DRAINING: u8 = 2;
pub(crate) const PHASE_AWAITING_CANCELLATION: u8 = 3;
pub(crate) const PHASE_STOPPING: u8 = 4;

const MAINTENANCE_RETRY_AFTER_SECONDS: NonZeroU64 = NonZeroU64::MIN;

#[derive(Clone)]
pub struct AdmissionController {
    pub(crate) shared: Arc<SharedLifecycle>,
}

impl AdmissionController {
    pub fn try_enter(&self) -> Result<ActiveRequestGuard, MaintenanceAdmission> {
        if self.shared.phase.load(Ordering::SeqCst) != PHASE_RUNNING {
            return Err(MaintenanceAdmission::new());
        }

        if self
            .shared
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                active.checked_add(1)
            })
            .is_err()
        {
            return Err(MaintenanceAdmission::new());
        }

        if self.shared.phase.load(Ordering::SeqCst) == PHASE_RUNNING {
            return Ok(ActiveRequestGuard {
                shared: Arc::clone(&self.shared),
            });
        }

        self.shared.decrement_active();
        Err(MaintenanceAdmission::new())
    }
}

impl fmt::Debug for AdmissionController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmissionController")
            .field(
                "active_requests",
                &self.shared.active.load(Ordering::SeqCst),
            )
            .finish_non_exhaustive()
    }
}

pub struct ActiveRequestGuard {
    shared: Arc<SharedLifecycle>,
}

impl fmt::Debug for ActiveRequestGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ActiveRequestGuard")
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.shared.decrement_active();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenanceAdmission {
    retry_after_seconds: NonZeroU64,
}

impl MaintenanceAdmission {
    fn new() -> Self {
        Self {
            retry_after_seconds: MAINTENANCE_RETRY_AFTER_SECONDS,
        }
    }

    pub const fn retry_after_seconds(self) -> u64 {
        self.retry_after_seconds.get()
    }
}

pub(crate) struct SharedLifecycle {
    pub(crate) phase: AtomicU8,
    pub(crate) active: AtomicUsize,
    zero_active: Notify,
}

impl SharedLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_STARTING),
            active: AtomicUsize::new(0),
            zero_active: Notify::new(),
        }
    }

    pub(crate) fn admission_controller(self: &Arc<Self>) -> AdmissionController {
        AdmissionController {
            shared: Arc::clone(self),
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_running(&self) {
        self.phase.store(PHASE_RUNNING, Ordering::SeqCst);
    }

    pub(crate) fn set_phase(&self, phase: u8) {
        self.phase.store(phase, Ordering::SeqCst);
    }

    pub(crate) fn active_requests(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_for_zero_active(&self) {
        loop {
            let notified = self.zero_active.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active_requests() == 0 {
                return;
            }
            notified.await;
        }
    }

    fn decrement_active(&self) {
        if Self::checked_decrement(&self.active) == DecrementOutcome::BecameZero {
            self.zero_active.notify_waiters();
        }
    }

    fn checked_decrement(active: &AtomicUsize) -> DecrementOutcome {
        match active.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
            active.checked_sub(1)
        }) {
            Ok(1) => DecrementOutcome::BecameZero,
            Ok(_) => DecrementOutcome::Remaining,
            Err(0) => DecrementOutcome::AlreadyZero,
            Err(_) => unreachable!("checked subtraction fails only at zero"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecrementOutcome {
    BecameZero,
    Remaining,
    AlreadyZero,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::{AdmissionController, DecrementOutcome, SharedLifecycle};

    #[test]
    fn active_counter_overflow_is_rejected_without_wrapping() {
        let shared = Arc::new(SharedLifecycle::new());
        shared.mark_running();
        shared.active.store(usize::MAX, Ordering::Release);
        let admission = AdmissionController { shared };

        assert!(admission.try_enter().is_err());
        assert_eq!(admission.shared.active.load(Ordering::Acquire), usize::MAX);
    }

    #[test]
    fn guard_drop_cannot_underflow_the_active_counter() {
        let active = AtomicUsize::new(0);

        assert_eq!(
            SharedLifecycle::checked_decrement(&active),
            DecrementOutcome::AlreadyZero
        );
        assert_eq!(active.load(Ordering::Acquire), 0);
    }
}
