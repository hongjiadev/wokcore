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
            }),
        }
    }

    pub fn admission_controller(&self) -> AdmissionController {
        self.state.shared.admission_controller()
    }

    pub fn snapshot(&self) -> LifecycleSnapshot {
        loop {
            let before = self
                .state
                .shared
                .phase
                .load(std::sync::atomic::Ordering::SeqCst);
            let active_requests = self.state.shared.active_requests();
            let after = self
                .state
                .shared
                .phase
                .load(std::sync::atomic::Ordering::SeqCst);
            if before == after {
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
        self.publish_phase(&mut transition, LifecyclePhase::Running);
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
                    self.publish_phase(&mut transition, LifecyclePhase::Draining);
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
                self.publish_phase(&mut transition, LifecyclePhase::Running);
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
                self.publish_phase(&mut transition, LifecyclePhase::Draining);
            }
            LifecyclePhase::Draining | LifecyclePhase::AwaitingCancellation => {}
            LifecyclePhase::Stopping => return Ok(self.snapshot()),
        }

        let active_requests = self.state.shared.active_requests();
        if active_requests != 0 {
            return Err(LifecycleError::ActiveRequestsRemain { active_requests });
        }
        self.publish_phase(&mut transition, LifecyclePhase::Stopping);
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
                self.publish_phase(&mut transition, LifecyclePhase::AwaitingCancellation);
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

    fn publish_phase(&self, transition: &mut TransitionState, phase: LifecyclePhase) {
        transition.phase = phase;
        self.state.shared.set_phase(phase as u8);
        self.state.phase_watch.send_replace(WatchState {
            phase,
            drain_generation: transition.drain_generation,
        });
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
