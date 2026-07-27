use std::{
    fmt,
    num::NonZeroU64,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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

        #[cfg(test)]
        self.shared
            .checkpoint(AdmissionTestPoint::IncrementBeforePhaseRecheck);

        if self.shared.phase.load(Ordering::SeqCst) == PHASE_RUNNING {
            #[cfg(test)]
            self.shared
                .checkpoint(AdmissionTestPoint::RunningPhaseRecheck);
            self.shared.observe_activity();
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
    transition_revision: AtomicU64,
    activity_revision: AtomicU64,
    last_activity_tick_millis: AtomicU64,
    zero_active: Notify,
    idle_zero_active: Notify,
    #[cfg(test)]
    test_hooks: AdmissionTestHooks,
}

impl SharedLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_STARTING),
            active: AtomicUsize::new(0),
            transition_revision: AtomicU64::new(0),
            activity_revision: AtomicU64::new(0),
            last_activity_tick_millis: AtomicU64::new(monotonic_tick_millis()),
            zero_active: Notify::new(),
            idle_zero_active: Notify::new(),
            #[cfg(test)]
            test_hooks: AdmissionTestHooks::default(),
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

    pub(crate) fn advance_transition_revision(&self) -> bool {
        self.transition_revision
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
                revision.checked_add(1)
            })
            .is_ok()
    }

    pub(crate) fn transition_revision(&self) -> u64 {
        self.transition_revision.load(Ordering::SeqCst)
    }

    pub(crate) fn activity_revision(&self) -> u64 {
        self.activity_revision.load(Ordering::SeqCst)
    }

    pub(crate) fn has_been_idle_for(&self, duration: Duration) -> bool {
        let elapsed = monotonic_tick_millis()
            .saturating_sub(self.last_activity_tick_millis.load(Ordering::SeqCst));
        elapsed >= duration.as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn observe_activity(&self) {
        self.last_activity_tick_millis
            .store(monotonic_tick_millis(), Ordering::SeqCst);
        let _ =
            self.activity_revision
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
                    revision.checked_add(1)
                });
    }

    pub(crate) fn active_requests(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_for_zero_active(&self) {
        loop {
            // `notify_waiters` advances a generation captured when this future is created.
            let notified = self.zero_active.notified();
            if self.active_requests() == 0 {
                return;
            }
            #[cfg(test)]
            self.checkpoint(AdmissionTestPoint::NonzeroCheckBeforeAwait);
            notified.await;
        }
    }

    pub(crate) async fn wait_for_idle_zero_transition(&self) {
        self.idle_zero_active.notified().await;
    }

    fn decrement_active(&self) {
        #[cfg(test)]
        self.test_hooks
            .decrement_attempts
            .fetch_add(1, Ordering::SeqCst);
        let outcome = Self::checked_decrement(&self.active);
        if outcome != DecrementOutcome::AlreadyZero {
            self.observe_activity();
        }
        if outcome == DecrementOutcome::BecameZero {
            self.zero_active.notify_waiters();
            // The idle-memory observer is the sole consumer. A stored single permit
            // coalesces bursts without making request completion perform reclamation.
            self.idle_zero_active.notify_one();
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

    #[cfg(test)]
    fn install_test_gate(&self, point: AdmissionTestPoint, gate: Arc<TestGate>) {
        let slot = match point {
            AdmissionTestPoint::IncrementBeforePhaseRecheck => {
                &self.test_hooks.after_increment_before_phase_recheck
            }
            AdmissionTestPoint::RunningPhaseRecheck => &self.test_hooks.after_running_phase_recheck,
            AdmissionTestPoint::NonzeroCheckBeforeAwait => {
                &self.test_hooks.after_nonzero_check_before_await
            }
        };
        *slot.lock().unwrap() = Some(gate);
    }

    #[cfg(test)]
    fn checkpoint(&self, point: AdmissionTestPoint) {
        let slot = match point {
            AdmissionTestPoint::IncrementBeforePhaseRecheck => {
                &self.test_hooks.after_increment_before_phase_recheck
            }
            AdmissionTestPoint::RunningPhaseRecheck => &self.test_hooks.after_running_phase_recheck,
            AdmissionTestPoint::NonzeroCheckBeforeAwait => {
                &self.test_hooks.after_nonzero_check_before_await
            }
        };
        let gate = slot.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.reached.wait();
            gate.release.wait();
        }
    }

    #[cfg(test)]
    fn decrement_attempts(&self) -> usize {
        self.test_hooks.decrement_attempts.load(Ordering::SeqCst)
    }
}

fn monotonic_tick_millis() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecrementOutcome {
    BecameZero,
    Remaining,
    AlreadyZero,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum AdmissionTestPoint {
    IncrementBeforePhaseRecheck,
    RunningPhaseRecheck,
    NonzeroCheckBeforeAwait,
}

#[cfg(test)]
#[derive(Default)]
struct AdmissionTestHooks {
    after_increment_before_phase_recheck: std::sync::Mutex<Option<Arc<TestGate>>>,
    after_running_phase_recheck: std::sync::Mutex<Option<Arc<TestGate>>>,
    after_nonzero_check_before_await: std::sync::Mutex<Option<Arc<TestGate>>>,
    decrement_attempts: AtomicUsize,
}

#[cfg(test)]
struct TestGate {
    reached: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::{sync::oneshot, task::yield_now, time::timeout};

    use super::{
        AdmissionController, AdmissionTestPoint, DecrementOutcome, SharedLifecycle, TestGate,
    };
    use crate::lifecycle::{DrainOutcome, LifecyclePhase, ServiceLifecycle};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    impl TestGate {
        fn new() -> Self {
            Self {
                reached: Barrier::new(2),
                release: Barrier::new(2),
            }
        }
    }

    async fn wait_for_phase(lifecycle: &ServiceLifecycle, expected: LifecyclePhase) {
        timeout(TEST_TIMEOUT, async {
            loop {
                if lifecycle.snapshot().phase == expected {
                    return;
                }
                yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_paused_after_increment_rolls_back_when_drain_publishes() {
        let lifecycle = Arc::new(ServiceLifecycle::new());
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let gate = Arc::new(TestGate::new());
        admission.shared.install_test_gate(
            AdmissionTestPoint::IncrementBeforePhaseRecheck,
            Arc::clone(&gate),
        );
        let admission_thread = std::thread::spawn(move || admission.try_enter());

        gate.reached.wait();
        assert_eq!(lifecycle.snapshot().active_requests, 1);
        let lifecycle_for_drain = Arc::clone(&lifecycle);
        let drain =
            tokio::spawn(
                async move { lifecycle_for_drain.begin_drain(TEST_TIMEOUT).await.unwrap() },
            );
        wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;
        gate.release.wait();

        assert!(admission_thread.join().unwrap().is_err());
        assert_eq!(drain.await.unwrap(), DrainOutcome::Completed);
        assert_eq!(lifecycle.snapshot().active_requests, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_that_rechecks_running_before_drain_is_counted_as_existing() {
        let lifecycle = Arc::new(ServiceLifecycle::new());
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let gate = Arc::new(TestGate::new());
        admission
            .shared
            .install_test_gate(AdmissionTestPoint::RunningPhaseRecheck, Arc::clone(&gate));
        let admission_thread = std::thread::spawn(move || admission.try_enter());

        gate.reached.wait();
        let lifecycle_for_drain = Arc::clone(&lifecycle);
        let drain =
            tokio::spawn(
                async move { lifecycle_for_drain.begin_drain(TEST_TIMEOUT).await.unwrap() },
            );
        wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;
        gate.release.wait();

        let guard = admission_thread.join().unwrap().unwrap();
        assert_eq!(lifecycle.snapshot().active_requests, 1);
        drop(guard);
        assert_eq!(drain.await.unwrap(), DrainOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_wait_consumes_notification_delivered_after_a_nonzero_check() {
        let lifecycle = Arc::new(ServiceLifecycle::new());
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let guard = admission.try_enter().unwrap();
        let gate = Arc::new(TestGate::new());
        admission.shared.install_test_gate(
            AdmissionTestPoint::NonzeroCheckBeforeAwait,
            Arc::clone(&gate),
        );
        let lifecycle_for_wait = Arc::clone(&lifecycle);
        let waiter = tokio::spawn(async move {
            lifecycle_for_wait.wait_for_zero_active().await;
        });

        gate.reached.wait();
        drop(guard);
        gate.release.wait();

        timeout(TEST_TIMEOUT, waiter).await.unwrap().unwrap();
        assert_eq!(lifecycle.snapshot().active_requests, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_guard_drop_path_attempts_exactly_one_decrement() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let survivor = admission.try_enter().unwrap();
        let normal = admission.try_enter().unwrap();
        drop(normal);
        assert_eq!(admission.shared.decrement_attempts(), 1);
        assert_eq!(lifecycle.snapshot().active_requests, 1);
        drop(survivor);
        assert_eq!(admission.shared.decrement_attempts(), 2);

        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let survivor = admission.try_enter().unwrap();
        let (entered, ready) = oneshot::channel();
        let admission_for_abort = admission.clone();
        let aborted = tokio::spawn(async move {
            let _target = admission_for_abort.try_enter().unwrap();
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        aborted.abort();
        assert!(aborted.await.unwrap_err().is_cancelled());
        assert_eq!(admission.shared.decrement_attempts(), 1);
        assert_eq!(lifecycle.snapshot().active_requests, 1);
        drop(survivor);
        assert_eq!(admission.shared.decrement_attempts(), 2);

        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let survivor = admission.try_enter().unwrap();
        let admission_for_panic = admission.clone();
        let panicked = tokio::spawn(async move {
            let _target = admission_for_panic.try_enter().unwrap();
            panic!("intentional decrement observer unwind");
        });
        assert!(panicked.await.unwrap_err().is_panic());
        assert_eq!(admission.shared.decrement_attempts(), 1);
        assert_eq!(lifecycle.snapshot().active_requests, 1);
        drop(survivor);
        assert_eq!(admission.shared.decrement_attempts(), 2);
    }

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
