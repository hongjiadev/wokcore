use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::time::{sleep, timeout};
use wokcore_server::observability::{
    DEFAULT_SCANNER_WORKERS, ENUMERATION_SLICE_ENTRIES, ENUMERATION_SLICE_TIME, IndexPhase,
    MAX_ENUMERATION_SLICE_ENTRIES, MAX_ENUMERATION_SLICE_TIME, MAX_SCANNER_WORKERS,
    PreparedScheduler, ProductionSessionScanBackend, SESSION_BATCH_QUEUE_CAPACITY,
    SESSION_BATCH_ROWS, SESSION_BATCH_UTF8_BYTES, SESSION_PARTIAL_FLUSH_INTERVAL,
    SESSION_PRODUCER_SLICE, ScanFileObservation, ScanRootObservation, ScanSliceBudget,
    ScanSliceReport, ScanTimestampSource, SchedulerConfig, SessionKind, SessionRootPaths,
    SessionScanBackend,
};
use wokcore_storage::{
    MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, STATE_STORE_WRITER_QUEUE_CAPACITY,
    SessionSourceErrorCode, SessionSourceStatus, WAL_CHECKPOINT_THRESHOLD_BYTES,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const FILESYSTEM_EVENT_TIMEOUT: Duration = Duration::from_secs(15);

struct FixedTimestamp;

impl ScanTimestampSource for FixedTimestamp {
    fn now(&self) -> Option<String> {
        Some("2026-07-27T00:00:00Z".to_owned())
    }
}

struct GatedBackend {
    calls: AtomicUsize,
    gate: Mutex<bool>,
    released: Condvar,
}

impl GatedBackend {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            gate: Mutex::new(false),
            released: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.released.notify_all();
    }
}

impl SessionScanBackend for GatedBackend {
    fn scan_slice(&self, kind: SessionKind, budget: ScanSliceBudget) -> ScanSliceReport {
        assert_eq!(budget.maximum_entries, ENUMERATION_SLICE_ENTRIES);
        assert_eq!(budget.maximum_duration, ENUMERATION_SLICE_TIME);
        self.calls.fetch_add(1, Ordering::AcqRel);
        let mut released = self.gate.lock().unwrap();
        while !*released {
            released = self.released.wait(released).unwrap();
        }
        ScanSliceReport {
            complete: true,
            transition_at: "2026-07-27T00:00:00Z".to_owned(),
            root: ScanRootObservation::Readable,
            files: vec![ScanFileObservation {
                source_key: format!("{kind:?}"),
                current_generation_visible: true,
                status: SessionSourceStatus::Available,
                error_code: None,
            }],
        }
    }
}

#[test]
fn scheduler_resource_defaults_and_hard_maxima_are_exact() {
    assert_eq!(DEFAULT_SCANNER_WORKERS, 2);
    assert_eq!(MAX_SCANNER_WORKERS, 4);
    assert_eq!(ENUMERATION_SLICE_ENTRIES, 256);
    assert_eq!(MAX_ENUMERATION_SLICE_ENTRIES, 1_024);
    assert_eq!(ENUMERATION_SLICE_TIME, Duration::from_millis(25));
    assert_eq!(MAX_ENUMERATION_SLICE_TIME, Duration::from_millis(100));
    assert_eq!(SESSION_BATCH_ROWS, 512);
    assert_eq!(SESSION_BATCH_ROWS, MAX_SESSION_BATCH_ROWS);
    assert_eq!(SESSION_BATCH_UTF8_BYTES, 512 * 1024);
    assert_eq!(SESSION_BATCH_UTF8_BYTES, MAX_SESSION_BATCH_BYTES);
    assert_eq!(SESSION_PRODUCER_SLICE, Duration::from_millis(25));
    assert_eq!(SESSION_PARTIAL_FLUSH_INTERVAL, Duration::from_millis(250));
    assert_eq!(SESSION_BATCH_QUEUE_CAPACITY, 4);
    assert_eq!(
        SESSION_BATCH_QUEUE_CAPACITY,
        STATE_STORE_WRITER_QUEUE_CAPACITY
    );
    assert_eq!(WAL_CHECKPOINT_THRESHOLD_BYTES, 16 * 1024 * 1024);

    let backend = Arc::new(GatedBackend::new());
    let invalid = SchedulerConfig {
        workers: 5,
        ..SchedulerConfig::default()
    };
    assert!(PreparedScheduler::new(backend, invalid).is_err());
}

#[test]
fn production_backend_uses_only_explicit_synthetic_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let roots = SessionRootPaths {
        codex: fixture.path().join("absent-codex"),
        claude: fixture.path().join("synthetic-claude"),
        gemini: fixture.path().join("synthetic-gemini"),
    };
    std::fs::create_dir(&roots.claude).unwrap();
    std::fs::create_dir(&roots.gemini).unwrap();
    let backend = ProductionSessionScanBackend::open(
        roots,
        fixture.path().join("state.sqlite3"),
        [0x5a; 32],
        Arc::new(FixedTimestamp),
    )
    .unwrap();
    let budget = ScanSliceBudget {
        maximum_entries: ENUMERATION_SLICE_ENTRIES,
        maximum_duration: ENUMERATION_SLICE_TIME,
    };

    let complete_empty = |kind| {
        for _ in 0..8 {
            let report = backend.scan_slice(kind, budget);
            if report.complete {
                return report;
            }
        }
        panic!("production scanner did not retain its bounded cursor for {kind:?}");
    };
    let codex = complete_empty(SessionKind::Codex);
    let claude = complete_empty(SessionKind::Claude);
    let gemini = complete_empty(SessionKind::Gemini);

    assert_eq!(codex.root, ScanRootObservation::Missing);
    assert_eq!(claude.root, ScanRootObservation::Readable);
    assert_eq!(gemini.root, ScanRootObservation::Readable);
    assert!(codex.files.is_empty());
    assert!(claude.files.is_empty());
    assert!(gemini.files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_scan_cannot_start_before_the_explicit_readiness_boundary() {
    let backend = Arc::new(GatedBackend::new());
    let (handle, prepared) =
        PreparedScheduler::new(backend.clone(), SchedulerConfig::default()).unwrap();

    sleep(Duration::from_millis(50)).await;
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
    assert_eq!(handle.status().phase, IndexPhase::Starting);

    let running = prepared.start_after_readiness().unwrap();
    timeout(TEST_TIMEOUT, async {
        while backend.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.status().phase, IndexPhase::Scanning);

    backend.release();
    timeout(TEST_TIMEOUT, async {
        while handle.status().phase != IndexPhase::Idle {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        handle
            .status()
            .sources
            .iter()
            .all(|source| source.status == SessionSourceStatus::Available)
    );
    running.shutdown().await.unwrap();
}

struct AggregateBackend {
    scans: AtomicUsize,
}

impl SessionScanBackend for AggregateBackend {
    fn scan_slice(&self, kind: SessionKind, _budget: ScanSliceBudget) -> ScanSliceReport {
        let scan = self.scans.fetch_add(1, Ordering::AcqRel);
        let transition_at = if scan < 3 {
            "2026-07-27T00:00:00Z"
        } else {
            "2026-07-27T00:01:00Z"
        };
        let files = match kind {
            SessionKind::Codex if scan < 3 => vec![
                ScanFileObservation {
                    source_key: "z".to_owned(),
                    current_generation_visible: true,
                    status: SessionSourceStatus::Stale,
                    error_code: Some(SessionSourceErrorCode::SourceIoFailed),
                },
                ScanFileObservation {
                    source_key: "a".to_owned(),
                    current_generation_visible: true,
                    status: SessionSourceStatus::ResourceLimited,
                    error_code: Some(SessionSourceErrorCode::SourceReplayLimit),
                },
            ],
            _ => vec![ScanFileObservation {
                source_key: "ok".to_owned(),
                current_generation_visible: true,
                status: SessionSourceStatus::Available,
                error_code: None,
            }],
        };
        ScanSliceReport {
            complete: true,
            transition_at: transition_at.to_owned(),
            root: ScanRootObservation::Readable,
            files,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregation_is_deterministic_recovers_and_notification_storm_is_nonblocking() {
    let backend = Arc::new(AggregateBackend {
        scans: AtomicUsize::new(0),
    });
    let config = SchedulerConfig {
        notification_debounce: Duration::from_millis(5),
        fallback_interval: Duration::from_secs(60),
        ..SchedulerConfig::default()
    };
    let (handle, prepared) = PreparedScheduler::new(backend, config).unwrap();
    let running = prepared.start_after_readiness().unwrap();
    timeout(TEST_TIMEOUT, async {
        loop {
            let status = handle.status();
            if status.phase == IndexPhase::Idle
                && status.sources[0].status != SessionSourceStatus::Undiscovered
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = handle.status().sources[0].clone();
    assert_eq!(first.status, SessionSourceStatus::ResourceLimited);
    assert_eq!(
        first.error_code,
        Some(SessionSourceErrorCode::SourceReplayLimit)
    );

    let started = Instant::now();
    for _ in 0..10_000 {
        let _ = handle.notify(SessionKind::Codex);
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    timeout(TEST_TIMEOUT, async {
        loop {
            let status = handle.status();
            if status.phase == IndexPhase::Idle
                && status.sources[0].status == SessionSourceStatus::Available
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let recovered = handle.status().sources[0].clone();
    assert_eq!(recovered.error_code, None);
    assert_eq!(
        recovered.last_transition_at.as_deref(),
        Some("2026-07-27T00:01:00Z")
    );
    running.shutdown().await.unwrap();
}

struct NotificationBackend {
    scans: [AtomicUsize; 3],
}

impl NotificationBackend {
    fn count(&self, kind: SessionKind) -> usize {
        let index = match kind {
            SessionKind::Codex => 0,
            SessionKind::Claude => 1,
            SessionKind::Gemini => 2,
        };
        self.scans[index].load(Ordering::Acquire)
    }
}

impl SessionScanBackend for NotificationBackend {
    fn scan_slice(&self, kind: SessionKind, _budget: ScanSliceBudget) -> ScanSliceReport {
        let index = match kind {
            SessionKind::Codex => 0,
            SessionKind::Claude => 1,
            SessionKind::Gemini => 2,
        };
        self.scans[index].fetch_add(1, Ordering::AcqRel);
        ScanSliceReport {
            complete: true,
            transition_at: "2026-07-27T00:00:00Z".to_owned(),
            root: ScanRootObservation::Readable,
            files: vec![ScanFileObservation {
                source_key: format!("{kind:?}"),
                current_generation_visible: true,
                status: SessionSourceStatus::Available,
                error_code: None,
            }],
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_filesystem_events_feed_the_debounced_scheduler() {
    let fixture = tempfile::tempdir().unwrap();
    let roots = SessionRootPaths {
        codex: fixture.path().join("codex"),
        claude: fixture.path().join("claude"),
        gemini: fixture.path().join("gemini"),
    };
    for root in [&roots.codex, &roots.claude, &roots.gemini] {
        std::fs::create_dir(root).unwrap();
    }
    let backend = Arc::new(NotificationBackend {
        scans: std::array::from_fn(|_| AtomicUsize::new(0)),
    });
    let config = SchedulerConfig {
        notification_debounce: Duration::from_millis(50),
        fallback_interval: Duration::from_secs(60),
        ..SchedulerConfig::default()
    };
    let (_, prepared) = PreparedScheduler::new(backend.clone(), config).unwrap();
    let running = prepared
        .with_filesystem_notifications(roots.clone())
        .start_after_readiness()
        .unwrap();
    timeout(TEST_TIMEOUT, async {
        while backend.count(SessionKind::Codex) == 0
            || backend.count(SessionKind::Claude) == 0
            || backend.count(SessionKind::Gemini) == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let baseline = backend.count(SessionKind::Codex);

    std::fs::write(roots.codex.join("notification.jsonl"), b"{}\n").unwrap();

    timeout(FILESYSTEM_EVENT_TIMEOUT, async {
        while backend.count(SessionKind::Codex) == baseline {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    running.shutdown().await.unwrap();
}
