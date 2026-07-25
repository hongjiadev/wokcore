use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::{sync::watch, time};

use super::admission::{
    AdmissionController, PHASE_AWAITING_CANCELLATION, PHASE_DRAINING, PHASE_RUNNING,
    PHASE_STARTING, PHASE_STOPPING, SharedLifecycle,
};

#[derive(Clone)]
pub struct ServiceLifecycle {
    state: Arc<ServiceLifecycleState>,
}

struct ServiceLifecycleState {
    shared: Arc<SharedLifecycle>,
    transition: Mutex<TransitionState>,
    phase_watch: watch::Sender<WatchState>,
    #[cfg(test)]
    snapshot_test_hooks: SnapshotTestHooks,
}

#[derive(Clone, Copy)]
struct TransitionState {
    phase: LifecyclePhase,
    drain_generation: u64,
}

#[derive(Clone, Copy)]
struct WatchState {
    phase: LifecyclePhase,
    drain_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LifecyclePhase {
    Starting = PHASE_STARTING,
    Running = PHASE_RUNNING,
    Draining = PHASE_DRAINING,
    AwaitingCancellation = PHASE_AWAITING_CANCELLATION,
    Stopping = PHASE_STOPPING,
}

impl LifecyclePhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            PHASE_STARTING => Self::Starting,
            PHASE_RUNNING => Self::Running,
            PHASE_DRAINING => Self::Draining,
            PHASE_AWAITING_CANCELLATION => Self::AwaitingCancellation,
            PHASE_STOPPING => Self::Stopping,
            _ => unreachable!("lifecycle phase is only written by ServiceLifecycle"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleSnapshot {
    pub phase: LifecyclePhase,
    pub active_requests: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainOutcome {
    Completed,
    TimedOutAwaitingCancellation,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LifecycleError {
    #[error("service lifecycle cannot transition from {from:?}")]
    InvalidTransition { from: LifecyclePhase },
    #[error("service lifecycle still has {active_requests} active requests")]
    ActiveRequestsRemain { active_requests: usize },
    #[error("service lifecycle transition state is unavailable")]
    TransitionStateUnavailable,
    #[error("service lifecycle drain generation is exhausted")]
    DrainGenerationExhausted,
    #[error("service lifecycle transition revision is exhausted")]
    TransitionRevisionExhausted,
}

impl ServiceLifecycle {
    pub fn new() -> Self {
        let shared = Arc::new(SharedLifecycle::new());
        let initial = WatchState {
            phase: LifecyclePhase::Starting,
            drain_generation: 0,
        };
        let (phase_watch, _) = watch::channel(initial);
        Self {
            state: Arc::new(ServiceLifecycleState {
                shared,
                transition: Mutex::new(TransitionState {
                    phase: LifecyclePhase::Starting,
                    drain_generation: 0,
                }),
                phase_watch,
                #[cfg(test)]
                snapshot_test_hooks: SnapshotTestHooks::default(),
            }),
        }
    }

    pub fn admission_controller(&self) -> AdmissionController {
        self.state.shared.admission_controller()
    }

    pub fn snapshot(&self) -> LifecycleSnapshot {
        loop {
            let revision_before = self.state.shared.transition_revision();
            let before = self
                .state
                .shared
                .phase
                .load(std::sync::atomic::Ordering::SeqCst);
            #[cfg(test)]
            self.snapshot_checkpoint(SnapshotTestPoint::AfterInitialStateRead);
            let active_requests = self.state.shared.active_requests();
            #[cfg(test)]
            self.snapshot_checkpoint(SnapshotTestPoint::AfterActiveRead);
            let after = self
                .state
                .shared
                .phase
                .load(std::sync::atomic::Ordering::SeqCst);
            let revision_after = self.state.shared.transition_revision();
            if revision_before == revision_after && before == after {
                return LifecycleSnapshot {
                    phase: LifecyclePhase::from_raw(after),
                    active_requests,
                };
            }
        }
    }

    pub fn mark_running(&self) -> Result<LifecycleSnapshot, LifecycleError> {
        let mut transition = self.lock_transition()?;
        if transition.phase != LifecyclePhase::Starting {
            return Err(LifecycleError::InvalidTransition {
                from: transition.phase,
            });
        }
        self.publish_phase(&mut transition, LifecyclePhase::Running)?;
        Ok(self.snapshot())
    }

    pub async fn begin_drain(&self, timeout: Duration) -> Result<DrainOutcome, LifecycleError> {
        let drain_generation = {
            let mut transition = self.lock_transition()?;
            match transition.phase {
                LifecyclePhase::Starting => {
                    return Err(LifecycleError::InvalidTransition {
                        from: transition.phase,
                    });
                }
                LifecyclePhase::Running => {
                    transition.drain_generation = transition
                        .drain_generation
                        .checked_add(1)
                        .ok_or(LifecycleError::DrainGenerationExhausted)?;
                    self.publish_phase(&mut transition, LifecyclePhase::Draining)?;
                    transition.drain_generation
                }
                LifecyclePhase::Draining => transition.drain_generation,
                LifecyclePhase::AwaitingCancellation => {
                    return Ok(if self.state.shared.active_requests() == 0 {
                        DrainOutcome::Completed
                    } else {
                        DrainOutcome::TimedOutAwaitingCancellation
                    });
                }
                LifecyclePhase::Stopping => return Ok(DrainOutcome::Completed),
            }
        };

        let wait = self.wait_for_drain(drain_generation);
        let event = match time::timeout(timeout, wait).await {
            Ok(event) => event,
            Err(_) => DrainWait::TimedOut,
        };
        self.finish_drain_wait(drain_generation, event)
    }

    pub fn cancel_drain(&self) -> Result<LifecycleSnapshot, LifecycleError> {
        let mut transition = self.lock_transition()?;
        match transition.phase {
            LifecyclePhase::Draining | LifecyclePhase::AwaitingCancellation => {
                self.publish_phase(&mut transition, LifecyclePhase::Running)?;
                Ok(self.snapshot())
            }
            phase => Err(LifecycleError::InvalidTransition { from: phase }),
        }
    }

    pub fn request_stop(&self) -> Result<LifecycleSnapshot, LifecycleError> {
        let mut transition = self.lock_transition()?;
        match transition.phase {
            LifecyclePhase::Starting => {
                return Err(LifecycleError::InvalidTransition {
                    from: transition.phase,
                });
            }
            LifecyclePhase::Running => {
                transition.drain_generation = transition
                    .drain_generation
                    .checked_add(1)
                    .ok_or(LifecycleError::DrainGenerationExhausted)?;
                self.publish_phase(&mut transition, LifecyclePhase::Draining)?;
            }
            LifecyclePhase::Draining | LifecyclePhase::AwaitingCancellation => {}
            LifecyclePhase::Stopping => return Ok(self.snapshot()),
        }

        let active_requests = self.state.shared.active_requests();
        if active_requests != 0 {
            return Err(LifecycleError::ActiveRequestsRemain { active_requests });
        }
        self.publish_phase(&mut transition, LifecyclePhase::Stopping)?;
        Ok(self.snapshot())
    }

    pub async fn wait_for_zero_active(&self) {
        self.state.shared.wait_for_zero_active().await;
    }

    async fn wait_for_drain(&self, drain_generation: u64) -> DrainWait {
        let mut phase_watch = self.state.phase_watch.subscribe();
        loop {
            let observed = *phase_watch.borrow_and_update();
            if observed.drain_generation != drain_generation
                || observed.phase == LifecyclePhase::Running
            {
                return DrainWait::Cancelled;
            }
            match observed.phase {
                LifecyclePhase::AwaitingCancellation => return DrainWait::TimedOut,
                LifecyclePhase::Stopping => return DrainWait::ZeroActive,
                LifecyclePhase::Starting | LifecyclePhase::Running | LifecyclePhase::Draining => {}
            }
            if self.state.shared.active_requests() == 0 {
                return DrainWait::ZeroActive;
            }

            tokio::select! {
                () = self.state.shared.wait_for_zero_active() => {
                    return DrainWait::ZeroActive;
                }
                changed = phase_watch.changed() => {
                    if changed.is_err() {
                        return DrainWait::Cancelled;
                    }
                }
            }
        }
    }

    fn finish_drain_wait(
        &self,
        drain_generation: u64,
        event: DrainWait,
    ) -> Result<DrainOutcome, LifecycleError> {
        let mut transition = self.lock_transition()?;
        if transition.phase == LifecyclePhase::Stopping {
            return Ok(DrainOutcome::Completed);
        }
        if transition.drain_generation != drain_generation
            || transition.phase == LifecyclePhase::Running
            || event == DrainWait::Cancelled
        {
            return Ok(DrainOutcome::Cancelled);
        }
        if self.state.shared.active_requests() == 0 || event == DrainWait::ZeroActive {
            return Ok(DrainOutcome::Completed);
        }

        match transition.phase {
            LifecyclePhase::Draining => {
                self.publish_phase(&mut transition, LifecyclePhase::AwaitingCancellation)?;
                Ok(DrainOutcome::TimedOutAwaitingCancellation)
            }
            LifecyclePhase::AwaitingCancellation => Ok(DrainOutcome::TimedOutAwaitingCancellation),
            LifecyclePhase::Starting => Err(LifecycleError::InvalidTransition {
                from: transition.phase,
            }),
            LifecyclePhase::Running => Ok(DrainOutcome::Cancelled),
            LifecyclePhase::Stopping => Ok(DrainOutcome::Completed),
        }
    }

    fn lock_transition(&self) -> Result<MutexGuard<'_, TransitionState>, LifecycleError> {
        self.state
            .transition
            .lock()
            .map_err(|_| LifecycleError::TransitionStateUnavailable)
    }

    fn publish_phase(
        &self,
        transition: &mut TransitionState,
        phase: LifecyclePhase,
    ) -> Result<(), LifecycleError> {
        if !self.state.shared.advance_transition_revision() {
            return Err(LifecycleError::TransitionRevisionExhausted);
        }
        transition.phase = phase;
        self.state.shared.set_phase(phase as u8);
        self.state.phase_watch.send_replace(WatchState {
            phase,
            drain_generation: transition.drain_generation,
        });
        Ok(())
    }

    #[cfg(test)]
    fn install_snapshot_test_gate(
        &self,
        point: SnapshotTestPoint,
        target: std::thread::ThreadId,
        gate: Arc<SnapshotTestGate>,
    ) {
        let slot = match point {
            SnapshotTestPoint::AfterInitialStateRead => {
                &self.state.snapshot_test_hooks.after_initial_state_read
            }
            SnapshotTestPoint::AfterActiveRead => &self.state.snapshot_test_hooks.after_active_read,
        };
        *slot.lock().unwrap() = Some(SnapshotTestHook { target, gate });
    }

    #[cfg(test)]
    fn snapshot_checkpoint(&self, point: SnapshotTestPoint) {
        let slot = match point {
            SnapshotTestPoint::AfterInitialStateRead => {
                &self.state.snapshot_test_hooks.after_initial_state_read
            }
            SnapshotTestPoint::AfterActiveRead => &self.state.snapshot_test_hooks.after_active_read,
        };
        let current = std::thread::current().id();
        let gate = {
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|hook| hook.target == current) {
                slot.take().map(|hook| hook.gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.reached.wait();
            gate.release.wait();
        }
    }
}

impl Default for ServiceLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ServiceLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceLifecycle")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainWait {
    ZeroActive,
    TimedOut,
    Cancelled,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum SnapshotTestPoint {
    AfterInitialStateRead,
    AfterActiveRead,
}

#[cfg(test)]
#[derive(Default)]
struct SnapshotTestHooks {
    after_initial_state_read: Mutex<Option<SnapshotTestHook>>,
    after_active_read: Mutex<Option<SnapshotTestHook>>,
}

#[cfg(test)]
struct SnapshotTestHook {
    target: std::thread::ThreadId,
    gate: Arc<SnapshotTestGate>,
}

#[cfg(test)]
struct SnapshotTestGate {
    reached: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, mpsc},
        time::Duration,
    };

    use tokio::{task::yield_now, time::timeout};

    use super::{
        DrainOutcome, LifecyclePhase, ServiceLifecycle, SnapshotTestGate, SnapshotTestPoint,
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    impl SnapshotTestGate {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_retries_when_phase_returns_through_an_aba_transition() {
        let lifecycle = Arc::new(ServiceLifecycle::new());
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let existing = admission.try_enter().unwrap();
        let lifecycle_for_first_drain = Arc::clone(&lifecycle);
        let first_drain = tokio::spawn(async move {
            lifecycle_for_first_drain
                .begin_drain(TEST_TIMEOUT)
                .await
                .unwrap()
        });
        wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;

        let initial_read = Arc::new(SnapshotTestGate::new());
        let active_read = Arc::new(SnapshotTestGate::new());
        let snapshot_start = Arc::new(Barrier::new(2));
        let (thread_id_sender, thread_id_receiver) = mpsc::channel();
        let lifecycle_for_snapshot = Arc::clone(&lifecycle);
        let snapshot_start_for_thread = Arc::clone(&snapshot_start);
        let snapshot_thread = std::thread::spawn(move || {
            thread_id_sender.send(std::thread::current().id()).unwrap();
            snapshot_start_for_thread.wait();
            lifecycle_for_snapshot.snapshot()
        });
        let snapshot_thread_id = thread_id_receiver.recv().unwrap();
        lifecycle.install_snapshot_test_gate(
            SnapshotTestPoint::AfterInitialStateRead,
            snapshot_thread_id,
            Arc::clone(&initial_read),
        );
        lifecycle.install_snapshot_test_gate(
            SnapshotTestPoint::AfterActiveRead,
            snapshot_thread_id,
            Arc::clone(&active_read),
        );
        snapshot_start.wait();

        initial_read.reached.wait();
        lifecycle.cancel_drain().unwrap();
        let transient = admission.try_enter().unwrap();
        assert_eq!(lifecycle.state.shared.active_requests(), 2);
        initial_read.release.wait();
        active_read.reached.wait();
        drop(transient);
        let lifecycle_for_second_drain = Arc::clone(&lifecycle);
        let second_drain = tokio::spawn(async move {
            lifecycle_for_second_drain
                .begin_drain(TEST_TIMEOUT)
                .await
                .unwrap()
        });
        wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;
        active_read.release.wait();

        let snapshot = snapshot_thread.join().unwrap();
        assert_eq!(snapshot.phase, LifecyclePhase::Draining);
        assert_eq!(snapshot.active_requests, 1);
        assert_eq!(first_drain.await.unwrap(), DrainOutcome::Cancelled);
        drop(existing);
        assert_eq!(second_drain.await.unwrap(), DrainOutcome::Completed);
    }
}
