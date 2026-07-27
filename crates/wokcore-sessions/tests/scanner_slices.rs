use std::{collections::HashSet, fs, path::Path, time::Instant};

use tempfile::TempDir;
use wokcore_sessions::{
    claude::ClaudeScanner,
    codex::{CodexScanner, ScanControl, ScanOutcome},
    discovery::{
        DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES, SessionDiscoveryClock, SessionDiscoverySliceBudget,
    },
    gemini::GeminiScanner,
    model::{SessionScanControl, SessionScanOutcome},
};

const SOURCE_COUNT: usize = 1_025;
const NOW: &str = "2026-07-27T00:00:00Z";

#[test]
fn scanners_reuse_bounded_discovery_cycles_across_more_than_1024_files() {
    let clock = FrozenClock(Instant::now());

    let codex_root = flat_sources("sessions/2026/07/27", "source-");
    let codex_state = TempDir::new().unwrap();
    let mut codex = CodexScanner::open(
        codex_root.path(),
        codex_state.path().join("state.db"),
        [0x11; 32],
    )
    .unwrap();
    for _ in 0..5 {
        let slice = codex
            .scan_slice_with_clock(
                NOW,
                ScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        assert_eq!(slice.outcome, ScanOutcome::Interrupted);
        assert!(slice.sources.is_empty());
    }
    let mut codex_source_keys = Vec::new();
    let mut codex_completed = false;
    for _ in 0..32 {
        let slice = codex
            .scan_slice_with_clock(
                NOW,
                ScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        codex_completed = slice.outcome == ScanOutcome::Complete;
        codex_source_keys.extend(slice.sources.into_iter().map(|source| source.source_key));
        if codex_completed {
            break;
        }
    }
    assert!(codex_completed);
    assert_unique_complete_cycle(&codex_source_keys);

    let claude_root = flat_sources("projects/project", "source-");
    let claude_state = TempDir::new().unwrap();
    let mut claude = ClaudeScanner::open(
        claude_root.path(),
        claude_state.path().join("state.db"),
        [0x22; 32],
    )
    .unwrap();
    for _ in 0..5 {
        let slice = claude
            .scan_slice_with_clock(
                NOW,
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        assert_eq!(slice.outcome, SessionScanOutcome::Interrupted);
        assert!(slice.sources.is_empty());
    }
    let mut claude_source_keys = Vec::new();
    let mut claude_completed = false;
    for _ in 0..32 {
        let slice = claude
            .scan_slice_with_clock(
                NOW,
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        claude_completed = slice.outcome == SessionScanOutcome::Complete;
        claude_source_keys.extend(slice.sources.into_iter().map(|source| source.source_key));
        if claude_completed {
            break;
        }
    }
    assert!(claude_completed);
    assert_unique_complete_cycle(&claude_source_keys);

    let gemini_root = flat_sources("tmp/project/chats", "session-");
    let gemini_state = TempDir::new().unwrap();
    let mut gemini = GeminiScanner::open(
        gemini_root.path(),
        gemini_state.path().join("state.db"),
        [0x33; 32],
    )
    .unwrap();
    for _ in 0..5 {
        let slice = gemini
            .scan_slice_with_clock(
                NOW,
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        assert_eq!(slice.outcome, SessionScanOutcome::Interrupted);
        assert!(slice.sources.is_empty());
    }
    let mut gemini_source_keys = Vec::new();
    let mut gemini_completed = false;
    for _ in 0..32 {
        let slice = gemini
            .scan_slice_with_clock(
                NOW,
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
                &clock,
            )
            .unwrap();
        gemini_completed = slice.outcome == SessionScanOutcome::Complete;
        gemini_source_keys.extend(slice.sources.into_iter().map(|source| source.source_key));
        if gemini_completed {
            break;
        }
    }
    assert!(gemini_completed);
    assert_unique_complete_cycle(&gemini_source_keys);
    assert_eq!(DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES, 256);
}

fn assert_unique_complete_cycle(source_keys: &[String]) {
    assert_eq!(source_keys.len(), SOURCE_COUNT);
    assert_eq!(
        source_keys
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>()
            .len(),
        SOURCE_COUNT
    );
}

fn flat_sources(relative: &str, prefix: &str) -> TempDir {
    let root = TempDir::new().unwrap();
    let directory = root.path().join(Path::new(relative));
    fs::create_dir_all(&directory).unwrap();
    for index in 0..SOURCE_COUNT {
        fs::write(directory.join(format!("{prefix}{index:04}.jsonl")), b"{}\n").unwrap();
    }
    root
}

struct FrozenClock(Instant);

impl SessionDiscoveryClock for FrozenClock {
    fn now(&self) -> Instant {
        self.0
    }
}
