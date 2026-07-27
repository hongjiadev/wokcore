use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

use wokcore_platform::sessions::SessionRootLease;
use wokcore_sessions::discovery::{
    DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES, DEFAULT_SESSION_DISCOVERY_SOFT_DEADLINE,
    MAX_SESSION_DISCOVERY_HARD_DEADLINE, MAX_SESSION_DISCOVERY_SLICE_ENTRIES,
    SessionDiscoveryClock, SessionDiscoveryCursor, SessionDiscoveryKind,
    SessionDiscoverySliceBudget, SessionDiscoverySliceOutcome, SessionDiscoverySourceFormat,
    discover_claude_sessions_slice_with_clock, discover_codex_sessions_slice_with_clock,
    discover_gemini_sessions_slice_with_clock,
};

const SOURCE_COUNT: usize = 1_025;

#[test]
fn discovery_slice_budget_defaults_and_hard_bounds_are_exact() {
    let budget = SessionDiscoverySliceBudget::default();

    assert_eq!(
        budget.maximum_entries(),
        DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES
    );
    assert_eq!(
        budget.soft_deadline(),
        DEFAULT_SESSION_DISCOVERY_SOFT_DEADLINE
    );
    assert_eq!(budget.hard_deadline(), MAX_SESSION_DISCOVERY_HARD_DEADLINE);
    assert_eq!(DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES, 256);
    assert_eq!(MAX_SESSION_DISCOVERY_SLICE_ENTRIES, 1_024);
    assert_eq!(
        DEFAULT_SESSION_DISCOVERY_SOFT_DEADLINE,
        Duration::from_millis(25)
    );
    assert_eq!(
        MAX_SESSION_DISCOVERY_HARD_DEADLINE,
        Duration::from_millis(100)
    );

    assert!(
        SessionDiscoverySliceBudget::new(
            MAX_SESSION_DISCOVERY_SLICE_ENTRIES + 1,
            Duration::from_millis(25),
            Duration::from_millis(100),
        )
        .is_err()
    );
    assert!(
        SessionDiscoverySliceBudget::new(1, Duration::from_millis(25), Duration::from_millis(101),)
            .is_err()
    );
    assert!(
        SessionDiscoverySliceBudget::new(1, Duration::from_millis(26), Duration::from_millis(25),)
            .is_err()
    );
}

#[test]
fn every_kind_resumes_more_than_1024_sources_without_gaps_or_duplicates() {
    for fixture in [
        SliceFixture::codex(),
        SliceFixture::claude(),
        SliceFixture::gemini(),
    ] {
        let root = SessionRootLease::open(fixture.root.path()).unwrap();
        let mut cursor = SessionDiscoveryCursor::new(fixture.kind);
        let mut names = Vec::new();
        let mut slices = 0;
        let clock = FrozenClock(Instant::now());

        loop {
            let slice = match fixture.kind {
                SessionDiscoveryKind::Codex => discover_codex_sessions_slice_with_clock(
                    &root,
                    &mut cursor,
                    SessionDiscoverySliceBudget::default(),
                    &clock,
                ),
                SessionDiscoveryKind::Claude => discover_claude_sessions_slice_with_clock(
                    &root,
                    &mut cursor,
                    SessionDiscoverySliceBudget::default(),
                    &clock,
                ),
                SessionDiscoveryKind::Gemini => discover_gemini_sessions_slice_with_clock(
                    &root,
                    &mut cursor,
                    SessionDiscoverySliceBudget::default(),
                    &clock,
                ),
            }
            .unwrap();
            slices += 1;
            assert!(slice.processed_entries <= DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES);
            assert!(slice.entries.len() <= DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES);
            names.extend(
                slice
                    .entries
                    .iter()
                    .map(|entry| entry.file_name().to_owned()),
            );
            if slice.outcome == SessionDiscoverySliceOutcome::Complete {
                break;
            }
            assert!(slices < 32, "cursor must make bounded forward progress");
        }

        assert!(slices >= 5);
        assert_eq!(names.len(), SOURCE_COUNT);
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), SOURCE_COUNT);
        assert_eq!(names, fixture.expected_names);
    }
}

#[test]
fn injected_monotonic_deadlines_yield_and_resume_without_reprocessing() {
    let fixture = SliceFixture::codex_count(12);
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let mut cursor = SessionDiscoveryCursor::new(SessionDiscoveryKind::Codex);
    let soft_clock = StepClock::new(Duration::from_millis(5));
    let mut names = Vec::new();
    let mut observed_soft_yield = false;

    loop {
        let slice = discover_codex_sessions_slice_with_clock(
            &root,
            &mut cursor,
            SessionDiscoverySliceBudget::default(),
            &soft_clock,
        )
        .unwrap();
        observed_soft_yield |= slice.outcome == SessionDiscoverySliceOutcome::SoftDeadlineReached;
        names.extend(
            slice
                .entries
                .iter()
                .map(|entry| entry.file_name().to_owned()),
        );
        if slice.outcome == SessionDiscoverySliceOutcome::Complete {
            break;
        }
    }

    assert!(observed_soft_yield);
    assert_eq!(names, fixture.expected_names);

    let hard_root = SessionRootLease::open(fixture.root.path()).unwrap();
    let mut hard_cursor = SessionDiscoveryCursor::new(SessionDiscoveryKind::Codex);
    let hard_clock = StepClock::new(Duration::from_millis(101));
    let hard_slice = discover_codex_sessions_slice_with_clock(
        &hard_root,
        &mut hard_cursor,
        SessionDiscoverySliceBudget::default(),
        &hard_clock,
    )
    .unwrap();

    assert_eq!(
        hard_slice.outcome,
        SessionDiscoverySliceOutcome::HardDeadlineReached
    );
    assert_eq!(hard_slice.processed_entries, 1);
}

#[test]
fn kind_specific_hierarchies_and_gemini_precedence_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    write_source(root.path(), "sessions/2026/07/27/live.jsonl");
    write_source(root.path(), "archived_sessions/archive.jsonl");
    write_source(root.path(), "projects/project/direct.jsonl");
    write_source(
        root.path(),
        "projects/project/session/subagents/nested/agent.jsonl",
    );
    write_source(root.path(), "tmp/project/chats/session-pair.json");
    write_source(root.path(), "tmp/project/chats/session-pair.jsonl");
    write_source(root.path(), "tmp/project/chats/child/other.jsonl");
    let lease = SessionRootLease::open(root.path()).unwrap();
    let clock = FrozenClock(Instant::now());

    let codex = collect_kind(
        &lease,
        SessionDiscoveryKind::Codex,
        &clock,
        discover_codex_sessions_slice_with_clock,
    );
    assert_eq!(
        codex,
        [
            (
                "live.jsonl".to_owned(),
                SessionDiscoverySourceFormat::CodexLiveJsonl
            ),
            (
                "archive.jsonl".to_owned(),
                SessionDiscoverySourceFormat::CodexArchiveJsonl,
            ),
        ]
    );

    let claude = collect_kind(
        &lease,
        SessionDiscoveryKind::Claude,
        &clock,
        discover_claude_sessions_slice_with_clock,
    );
    assert_eq!(
        claude,
        [
            (
                "direct.jsonl".to_owned(),
                SessionDiscoverySourceFormat::ClaudeJsonl,
            ),
            (
                "agent.jsonl".to_owned(),
                SessionDiscoverySourceFormat::ClaudeJsonl,
            ),
        ]
    );

    let gemini = collect_kind(
        &lease,
        SessionDiscoveryKind::Gemini,
        &clock,
        discover_gemini_sessions_slice_with_clock,
    );
    assert_eq!(
        gemini,
        [
            (
                "other.jsonl".to_owned(),
                SessionDiscoverySourceFormat::GeminiCurrentJsonl,
            ),
            (
                "session-pair.jsonl".to_owned(),
                SessionDiscoverySourceFormat::GeminiCurrentJsonl,
            ),
        ]
    );
}

fn collect_kind<C, F>(
    root: &SessionRootLease,
    kind: SessionDiscoveryKind,
    clock: &C,
    discover: F,
) -> Vec<(String, SessionDiscoverySourceFormat)>
where
    C: SessionDiscoveryClock,
    F: Fn(
        &SessionRootLease,
        &mut SessionDiscoveryCursor,
        SessionDiscoverySliceBudget,
        &C,
    ) -> Result<
        wokcore_sessions::discovery::SessionDiscoverySlice,
        wokcore_sessions::discovery::SessionDiscoverySliceError,
    >,
{
    let mut cursor = SessionDiscoveryCursor::new(kind);
    let mut output = Vec::new();
    loop {
        let slice = discover(
            root,
            &mut cursor,
            SessionDiscoverySliceBudget::default(),
            clock,
        )
        .unwrap();
        output.extend(
            slice
                .entries
                .into_iter()
                .map(|entry| (entry.file_name().to_owned(), entry.format())),
        );
        if slice.outcome == SessionDiscoverySliceOutcome::Complete {
            return output;
        }
    }
}

fn write_source(root: &Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"{}\n").unwrap();
}

struct SliceFixture {
    root: tempfile::TempDir,
    kind: SessionDiscoveryKind,
    expected_names: Vec<String>,
}

impl SliceFixture {
    fn codex() -> Self {
        Self::codex_count(SOURCE_COUNT)
    }

    fn codex_count(count: usize) -> Self {
        Self::new(
            SessionDiscoveryKind::Codex,
            Path::new("sessions/2026/07/27"),
            "source-",
            count,
        )
    }

    fn claude() -> Self {
        Self::new(
            SessionDiscoveryKind::Claude,
            Path::new("projects/project-1"),
            "source-",
            SOURCE_COUNT,
        )
    }

    fn gemini() -> Self {
        Self::new(
            SessionDiscoveryKind::Gemini,
            Path::new("tmp/project-1/chats"),
            "session-",
            SOURCE_COUNT,
        )
    }

    fn new(kind: SessionDiscoveryKind, relative: &Path, prefix: &str, count: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(relative);
        fs::create_dir_all(&directory).unwrap();
        let expected_names = (0..count)
            .map(|index| format!("{prefix}{index:04}.jsonl"))
            .collect::<Vec<_>>();
        for name in &expected_names {
            fs::write(directory.join(name), b"{}\n").unwrap();
        }
        Self {
            root,
            kind,
            expected_names,
        }
    }
}

struct FrozenClock(Instant);

impl SessionDiscoveryClock for FrozenClock {
    fn now(&self) -> Instant {
        self.0
    }
}

struct StepClock {
    next: Mutex<Instant>,
    step: Duration,
}

impl StepClock {
    fn new(step: Duration) -> Self {
        Self {
            next: Mutex::new(Instant::now()),
            step,
        }
    }
}

impl SessionDiscoveryClock for StepClock {
    fn now(&self) -> Instant {
        let mut next = self.next.lock().unwrap();
        let current = *next;
        *next += self.step;
        current
    }
}
