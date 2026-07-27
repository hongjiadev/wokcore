use wokcore_platform::process_metrics::{
    ProcessMetricError, ProcessMetricSample, ProcessMetricValidator, ProcessMetricValues,
};

fn sample(
    pid: u32,
    identity: u64,
    observed_ms: u64,
    read_bytes: u64,
    write_bytes: u64,
) -> ProcessMetricSample {
    ProcessMetricSample::from_values(ProcessMetricValues {
        pid,
        identity_token: identity,
        observed_ms,
        private_working_set_bytes: 32 * 1024 * 1024,
        peak_private_bytes: 48 * 1024 * 1024,
        read_bytes,
        write_bytes,
        handle_count: 40,
        thread_count: 8,
        lifetime_ms: observed_ms.saturating_sub(100),
    })
}

#[test]
fn process_metrics_validator_rejects_restart_rollover_and_time_reversal() {
    let mut validator = ProcessMetricValidator::new(42, 7);
    validator.push(&sample(42, 7, 1_000, 20, 30)).unwrap();
    validator.push(&sample(42, 7, 1_100, 21, 31)).unwrap();
    assert_eq!(
        validator.push(&sample(42, 8, 1_200, 22, 32)).unwrap_err(),
        ProcessMetricError::IdentityChanged
    );

    let mut validator = ProcessMetricValidator::new(42, 7);
    validator.push(&sample(42, 7, 1_000, 20, 30)).unwrap();
    assert_eq!(
        validator.push(&sample(42, 7, 1_100, 19, 31)).unwrap_err(),
        ProcessMetricError::CounterRollback
    );

    let mut validator = ProcessMetricValidator::new(42, 7);
    validator.push(&sample(42, 7, 1_000, 20, 30)).unwrap();
    assert_eq!(
        validator.push(&sample(42, 7, 999, 21, 31)).unwrap_err(),
        ProcessMetricError::TimeReversed
    );
}

#[test]
fn process_metrics_evidence_is_bounded_and_content_free() {
    let value = serde_json::to_string(&sample(42, 7, 1_000, 20, 30)).unwrap();
    assert!(value.len() < 1024);
    for forbidden in [
        "command_line",
        "environment",
        "username",
        "Computer",
        "payload",
        "prompt",
    ] {
        assert!(!value.contains(forbidden));
    }
}

#[cfg(windows)]
#[test]
fn process_metrics_windows_sampler_targets_one_exact_pid_and_path() {
    use wokcore_platform::process_metrics::WindowsProcessSampler;

    let pid = std::process::id();
    let executable = std::env::current_exe().unwrap();
    let sampler = WindowsProcessSampler::open(pid, &executable).unwrap();
    let first = sampler.sample().unwrap();
    assert_eq!(first.pid(), pid);
    assert!(first.private_working_set_bytes() > 0);
    assert!(first.peak_private_bytes() > 0);
    assert!(first.handle_count() > 0);
    assert!(first.thread_count() > 0);
    assert!(first.lifetime_ms() > 0);

    let wrong = executable.with_file_name("wokcore-provider-sim.exe");
    assert_eq!(
        WindowsProcessSampler::open(pid, &wrong).unwrap_err(),
        ProcessMetricError::ExecutableMismatch
    );
}
