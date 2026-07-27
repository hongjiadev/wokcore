use std::{
    sync::{Arc, Barrier, mpsc},
    time::Duration,
};

use wokcore_server::query::{
    DEFAULT_QUERY_WORKERS, MAX_QUERY_WORKERS, QUERY_QUEUE_CAPACITY, QueryService, QueryServiceError,
};

#[test]
fn query_service_has_exact_worker_and_queue_bounds() {
    assert_eq!(DEFAULT_QUERY_WORKERS, 2);
    assert_eq!(MAX_QUERY_WORKERS, 4);
    assert_eq!(QUERY_QUEUE_CAPACITY, 32);
    assert!(QueryService::start(0).is_err());
    assert!(QueryService::start(MAX_QUERY_WORKERS + 1).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_admission_is_non_blocking_and_two_workers_execute_concurrently() {
    let service = QueryService::start(DEFAULT_QUERY_WORKERS).unwrap();
    let handle = service.handle();
    let entered = Arc::new(Barrier::new(DEFAULT_QUERY_WORKERS + 1));
    let release = Arc::new(Barrier::new(DEFAULT_QUERY_WORKERS + 1));
    let mut running = Vec::new();

    for _ in 0..DEFAULT_QUERY_WORKERS {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        running.push(
            handle
                .try_submit(move |_| {
                    entered.wait();
                    release.wait();
                    Ok(Vec::new())
                })
                .unwrap(),
        );
    }
    tokio::task::spawn_blocking({
        let entered = Arc::clone(&entered);
        move || entered.wait()
    })
    .await
    .unwrap();

    let mut queued = Vec::new();
    for _ in 0..QUERY_QUEUE_CAPACITY {
        queued.push(handle.try_submit(|_| Ok(Vec::new())).unwrap());
    }
    assert!(matches!(
        handle.try_submit(|_| Ok(Vec::new())),
        Err(QueryServiceError::Busy)
    ));

    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    for pending in running {
        pending.wait().await.unwrap();
    }
    for pending in queued {
        pending.wait().await.unwrap();
    }
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_an_inflight_wait_cooperatively_cancels_the_worker() {
    let service = QueryService::start(DEFAULT_QUERY_WORKERS).unwrap();
    let entered = Arc::new(Barrier::new(2));
    let worker_entered = Arc::clone(&entered);
    let (cancelled, observed) = mpsc::channel();
    let pending = service
        .handle()
        .try_submit(move |cancellation| {
            worker_entered.wait();
            while !cancellation.is_cancelled() {
                std::thread::yield_now();
            }
            cancelled.send(()).unwrap();
            Err(QueryServiceError::Cancelled)
        })
        .unwrap();
    let waiter = tokio::spawn(pending.wait());
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();

    waiter.abort();
    let _ = waiter.await;
    tokio::task::spawn_blocking(move || observed.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    service.shutdown().await.unwrap();
}

#[test]
fn query_deadline_is_exactly_five_seconds() {
    assert_eq!(
        wokcore_server::query::QUERY_DEADLINE,
        Duration::from_secs(5)
    );
}
