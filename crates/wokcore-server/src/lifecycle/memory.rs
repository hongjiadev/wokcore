use std::{sync::Arc, time::Duration};

use tokio::{
    sync::watch,
    task::{JoinError, JoinHandle},
    time::sleep,
};

use super::ServiceLifecycle;

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
const SYSTEM_IDLE_DELAY: Duration = Duration::from_secs(1);
#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
const SYSTEM_FOLLOW_UP_IDLE_DELAY: Duration = Duration::from_secs(9);

pub struct PreparedIdleMemoryReclaimer {
    lifecycle: ServiceLifecycle,
    idle_delay: Duration,
    follow_up_idle_delay: Duration,
    backend: Arc<dyn MemoryReclaimBackend>,
}

impl PreparedIdleMemoryReclaimer {
    pub fn for_system(lifecycle: ServiceLifecycle) -> Option<Self> {
        #[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
        {
            Some(Self {
                lifecycle,
                idle_delay: SYSTEM_IDLE_DELAY,
                follow_up_idle_delay: SYSTEM_FOLLOW_UP_IDLE_DELAY,
                backend: Arc::new(SystemMemoryReclaimBackend),
            })
        }
        #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
        {
            let _ = lifecycle;
            None
        }
    }

    #[cfg(test)]
    fn with_backend(
        lifecycle: ServiceLifecycle,
        idle_delay: Duration,
        backend: Arc<dyn MemoryReclaimBackend>,
    ) -> Self {
        Self {
            lifecycle,
            idle_delay,
            follow_up_idle_delay: idle_delay,
            backend,
        }
    }

    pub fn start(self) -> RunningIdleMemoryReclaimer {
        let (shutdown, shutdown_requested) = watch::channel(false);
        let initial_activity_revision = self.lifecycle.activity_revision();
        let join = tokio::spawn(run_reclaimer(
            self,
            shutdown_requested,
            initial_activity_revision,
        ));
        RunningIdleMemoryReclaimer {
            shutdown,
            join: Some(join),
        }
    }
}

pub struct RunningIdleMemoryReclaimer {
    shutdown: watch::Sender<bool>,
    join: Option<JoinHandle<()>>,
}

impl RunningIdleMemoryReclaimer {
    pub async fn shutdown(&mut self) -> Result<(), JoinError> {
        self.shutdown.send_replace(true);
        if let Some(join) = self.join.take() {
            join.await?;
        }
        Ok(())
    }
}

impl Drop for RunningIdleMemoryReclaimer {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

async fn run_reclaimer(
    prepared: PreparedIdleMemoryReclaimer,
    mut shutdown_requested: watch::Receiver<bool>,
    mut reclaimed_activity_revision: u64,
) {
    loop {
        tokio::select! {
            biased;
            changed = shutdown_requested.changed() => {
                if changed.is_err() || *shutdown_requested.borrow() {
                    return;
                }
            }
            () = prepared.lifecycle.wait_for_idle_zero_transition() => {}
        }

        tokio::select! {
            biased;
            changed = shutdown_requested.changed() => {
                if changed.is_err() || *shutdown_requested.borrow() {
                    return;
                }
            }
            () = sleep(prepared.idle_delay) => {}
        }

        let activity_revision = prepared.lifecycle.activity_revision();
        if prepared.lifecycle.snapshot().active_requests != 0
            || activity_revision == reclaimed_activity_revision
        {
            continue;
        }
        let backend = Arc::clone(&prepared.backend);
        if tokio::task::spawn_blocking(move || backend.reclaim())
            .await
            .is_err()
        {
            return;
        }
        reclaimed_activity_revision = activity_revision;

        tokio::select! {
            biased;
            changed = shutdown_requested.changed() => {
                if changed.is_err() || *shutdown_requested.borrow() {
                    return;
                }
            }
            () = sleep(prepared.follow_up_idle_delay) => {}
        }

        if prepared.lifecycle.snapshot().active_requests != 0
            || prepared.lifecycle.activity_revision() != reclaimed_activity_revision
        {
            continue;
        }
        let backend = Arc::clone(&prepared.backend);
        if tokio::task::spawn_blocking(move || backend.reclaim())
            .await
            .is_err()
        {
            return;
        }
    }
}

trait MemoryReclaimBackend: Send + Sync + 'static {
    fn reclaim(&self);
}

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
struct SystemMemoryReclaimBackend;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
impl MemoryReclaimBackend for SystemMemoryReclaimBackend {
    fn reclaim(&self) {
        // SAFETY: glibc documents malloc_trim as process-global and thread-safe.
        let _ = unsafe { libc::malloc_trim(0) };
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
}

#[cfg(target_os = "macos")]
impl MemoryReclaimBackend for SystemMemoryReclaimBackend {
    fn reclaim(&self) {
        // SAFETY: Apple documents a null zone as examining all malloc zones and
        // a zero goal as requesting maximal pressure relief.
        let _ = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::time::{sleep, timeout};

    use super::{MemoryReclaimBackend, PreparedIdleMemoryReclaimer};
    use crate::lifecycle::ServiceLifecycle;

    const IDLE_DELAY: Duration = Duration::from_millis(25);
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct CountingBackend {
        calls: AtomicUsize,
    }

    impl CountingBackend {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl MemoryReclaimBackend for CountingBackend {
        fn reclaim(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn wait_for_calls(backend: &CountingBackend, expected: usize) {
        timeout(TEST_TIMEOUT, async {
            while backend.calls() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_coalesces_a_completed_burst() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();
        let guards = (0..32)
            .map(|_| admission.try_enter().unwrap())
            .collect::<Vec<_>>();

        drop(guards);

        wait_for_calls(&backend, 2).await;
        sleep(IDLE_DELAY * 3).await;
        assert_eq!(backend.calls(), 2);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_reclaims_delayed_frees_once_in_the_same_idle_epoch() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();

        drop(admission.try_enter().unwrap());

        wait_for_calls(&backend, 2).await;
        sleep(IDLE_DELAY * 3).await;
        assert_eq!(backend.calls(), 2);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_waits_until_resumed_activity_finishes() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();
        let first = admission.try_enter().unwrap();
        drop(first);
        sleep(IDLE_DELAY / 2).await;
        let resumed = admission.try_enter().unwrap();

        sleep(IDLE_DELAY * 2).await;
        assert_eq!(backend.calls(), 0);
        drop(resumed);
        wait_for_calls(&backend, 2).await;
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_handles_distinct_idle_transitions() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();

        drop(admission.try_enter().unwrap());
        wait_for_calls(&backend, 2).await;
        drop(admission.try_enter().unwrap());
        wait_for_calls(&backend, 4).await;

        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_coalesces_transitions_during_the_idle_delay() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let admission = lifecycle.admission_controller();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();

        drop(admission.try_enter().unwrap());
        sleep(IDLE_DELAY / 2).await;
        drop(admission.try_enter().unwrap());

        wait_for_calls(&backend, 2).await;
        sleep(IDLE_DELAY * 3).await;
        assert_eq!(backend.calls(), 2);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_memory_reclaimer_shutdown_is_prompt_without_activity() {
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let backend = Arc::new(CountingBackend::default());
        let mut running =
            PreparedIdleMemoryReclaimer::with_backend(lifecycle, IDLE_DELAY, backend.clone())
                .start();

        timeout(TEST_TIMEOUT, running.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backend.calls(), 0);
    }
}
