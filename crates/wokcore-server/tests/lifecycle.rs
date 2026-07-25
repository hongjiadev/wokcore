use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{Barrier, oneshot},
    task::{JoinSet, yield_now},
    time::timeout,
};
use wokcore_server::lifecycle::{DrainOutcome, LifecycleError, LifecyclePhase, ServiceLifecycle};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn running_lifecycle() -> ServiceLifecycle {
    let lifecycle = ServiceLifecycle::new();
    assert_eq!(lifecycle.snapshot().phase, LifecyclePhase::Starting);
    lifecycle.mark_running().unwrap();
    lifecycle
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
async fn one_thousand_requests_are_admitted_without_a_concurrency_ceiling() {
    let lifecycle = running_lifecycle();
    let admission = lifecycle.admission_controller();
    let entered = Arc::new(Barrier::new(1_001));
    let release = Arc::new(Barrier::new(1_001));
    let mut requests = JoinSet::new();

    for _ in 0..1_000 {
        let admission = admission.clone();
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        requests.spawn(async move {
            let guard = admission.try_enter().unwrap();
            entered.wait().await;
            release.wait().await;
            drop(guard);
        });
    }

    entered.wait().await;
    let held = lifecycle.snapshot();
    assert_eq!(held.phase, LifecyclePhase::Running);
    assert_eq!(held.active_requests, 1_000);
    release.wait().await;
    while let Some(request) = requests.join_next().await {
        request.unwrap();
    }
    assert_eq!(lifecycle.snapshot().active_requests, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guards_decrement_exactly_once_on_drop_abort_and_panic_unwind() {
    let lifecycle = running_lifecycle();
    let admission = lifecycle.admission_controller();

    let guard = admission.try_enter().unwrap();
    assert_eq!(lifecycle.snapshot().active_requests, 1);
    drop(guard);
    assert_eq!(lifecycle.snapshot().active_requests, 0);

    let (abort_entered, abort_ready) = oneshot::channel();
    let admission_for_abort = admission.clone();
    let aborted = tokio::spawn(async move {
        let _guard = admission_for_abort.try_enter().unwrap();
        abort_entered.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    abort_ready.await.unwrap();
    assert_eq!(lifecycle.snapshot().active_requests, 1);
    aborted.abort();
    assert!(aborted.await.unwrap_err().is_cancelled());
    assert_eq!(lifecycle.snapshot().active_requests, 0);

    let admission_for_panic = admission.clone();
    let panicked = tokio::spawn(async move {
        let _guard = admission_for_panic.try_enter().unwrap();
        panic!("intentional lifecycle guard unwind");
    });
    assert!(panicked.await.unwrap_err().is_panic());
    assert_eq!(lifecycle.snapshot().active_requests, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_linearizes_against_admission_and_preserves_existing_guards() {
    let lifecycle = Arc::new(running_lifecycle());
    let admission = lifecycle.admission_controller();
    let existing = admission.try_enter().unwrap();
    let lifecycle_for_drain = Arc::clone(&lifecycle);
    let drain =
        tokio::spawn(async move { lifecycle_for_drain.begin_drain(TEST_TIMEOUT).await.unwrap() });

    wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;
    for _ in 0..1_000 {
        let rejection = admission.try_enter().unwrap_err();
        assert!(rejection.retry_after_seconds() > 0);
        assert!(rejection.retry_after_seconds() <= 3_600);
    }
    assert_eq!(lifecycle.snapshot().active_requests, 1);

    drop(existing);
    assert_eq!(drain.await.unwrap(), DrainOutcome::Completed);
    assert_eq!(
        lifecycle.snapshot(),
        wokcore_server::lifecycle::LifecycleSnapshot {
            phase: LifecyclePhase::Draining,
            active_requests: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_active_drain_completes_and_can_transition_to_stopping() {
    let lifecycle = running_lifecycle();

    assert_eq!(
        lifecycle.begin_drain(TEST_TIMEOUT).await.unwrap(),
        DrainOutcome::Completed
    );
    assert_eq!(lifecycle.snapshot().phase, LifecyclePhase::Draining);
    assert_eq!(
        lifecycle.request_stop().unwrap().phase,
        LifecyclePhase::Stopping
    );
    assert!(matches!(
        lifecycle.cancel_drain(),
        Err(LifecycleError::InvalidTransition {
            from: LifecyclePhase::Stopping,
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_timeout_awaits_explicit_cancellation_without_killing_guards() {
    let lifecycle = running_lifecycle();
    let admission = lifecycle.admission_controller();
    let guard = admission.try_enter().unwrap();

    assert_eq!(
        lifecycle
            .begin_drain(Duration::from_millis(20))
            .await
            .unwrap(),
        DrainOutcome::TimedOutAwaitingCancellation
    );
    assert_eq!(
        lifecycle.snapshot(),
        wokcore_server::lifecycle::LifecycleSnapshot {
            phase: LifecyclePhase::AwaitingCancellation,
            active_requests: 1,
        }
    );
    assert!(admission.try_enter().is_err());

    drop(guard);
    lifecycle.wait_for_zero_active().await;
    assert_eq!(
        lifecycle.snapshot().phase,
        LifecyclePhase::AwaitingCancellation
    );
    assert_eq!(
        lifecycle.request_stop().unwrap().phase,
        LifecyclePhase::Stopping
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_pre_stop_drain_restores_running_and_wakes_waiters() {
    let lifecycle = Arc::new(running_lifecycle());
    let admission = lifecycle.admission_controller();
    let existing = admission.try_enter().unwrap();
    let lifecycle_for_drain = Arc::clone(&lifecycle);
    let drain =
        tokio::spawn(async move { lifecycle_for_drain.begin_drain(TEST_TIMEOUT).await.unwrap() });

    wait_for_phase(&lifecycle, LifecyclePhase::Draining).await;
    assert_eq!(
        lifecycle.cancel_drain().unwrap().phase,
        LifecyclePhase::Running
    );
    assert_eq!(drain.await.unwrap(), DrainOutcome::Cancelled);
    let newly_admitted = admission.try_enter().unwrap();
    assert_eq!(lifecycle.snapshot().active_requests, 2);
    drop(newly_admitted);
    drop(existing);
    assert_eq!(lifecycle.snapshot().active_requests, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cancel_and_stop_cannot_roll_stopping_back_to_running() {
    let lifecycle = Arc::new(running_lifecycle());
    assert_eq!(
        lifecycle.begin_drain(TEST_TIMEOUT).await.unwrap(),
        DrainOutcome::Completed
    );
    let start = Arc::new(Barrier::new(3));

    let lifecycle_for_cancel = Arc::clone(&lifecycle);
    let cancel_start = Arc::clone(&start);
    let cancel = tokio::spawn(async move {
        cancel_start.wait().await;
        lifecycle_for_cancel.cancel_drain()
    });
    let lifecycle_for_stop = Arc::clone(&lifecycle);
    let stop_start = Arc::clone(&start);
    let stop = tokio::spawn(async move {
        stop_start.wait().await;
        lifecycle_for_stop.request_stop()
    });

    start.wait().await;
    let _ = cancel.await.unwrap();
    assert_eq!(stop.await.unwrap().unwrap().phase, LifecyclePhase::Stopping);
    assert_eq!(lifecycle.snapshot().phase, LifecyclePhase::Stopping);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_active_notification_is_lost_wakeup_safe() {
    let lifecycle = Arc::new(running_lifecycle());
    let admission = lifecycle.admission_controller();

    for _ in 0..1_000 {
        let guard = admission.try_enter().unwrap();
        let lifecycle_for_wait = Arc::clone(&lifecycle);
        let waiter = tokio::spawn(async move {
            lifecycle_for_wait.wait_for_zero_active().await;
        });
        yield_now().await;
        drop(guard);
        timeout(TEST_TIMEOUT, waiter).await.unwrap().unwrap();
        assert_eq!(lifecycle.snapshot().active_requests, 0);
    }
}

#[test]
fn snapshots_are_immutable_values_and_starting_rejects_admission() {
    let lifecycle = ServiceLifecycle::new();
    let admission = lifecycle.admission_controller();
    let starting = lifecycle.snapshot();

    let rejection = admission.try_enter().unwrap_err();
    assert!(rejection.retry_after_seconds() > 0);
    lifecycle.mark_running().unwrap();
    let guard = admission.try_enter().unwrap();

    assert_eq!(
        starting,
        wokcore_server::lifecycle::LifecycleSnapshot {
            phase: LifecyclePhase::Starting,
            active_requests: 0,
        }
    );
    assert_eq!(lifecycle.snapshot().active_requests, 1);
    drop(guard);
}
