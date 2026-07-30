use serde::Serialize;

use crate::CommandOutput;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProgressState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProgressPhase {
    CheckingRelease,
    Downloading,
    Verifying,
    Installing,
    PreparingService,
    Draining,
    Stopping,
    Starting,
    VerifyingRuntime,
    RollingBack,
    Completed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ProgressDetails {
    pub current_version: Option<String>,
    pub target_version: Option<String>,
    pub active_requests: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DownloadProgressDetails {
    pub current_version: Option<String>,
    pub target_version: Option<String>,
    pub bytes_completed: u64,
    pub bytes_total: u64,
    pub active_requests: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProgressErrorCode {
    UpdateUnavailable,
    IncompatibleManifest,
    UpdateVerificationFailed,
    UpdateInstallFailed,
    ActiveRequestsRemain,
    RolledBack,
    RecoveryRequired,
    OperationInProgress,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CoreOperationProgress {
    pub schema_version: u8,
    pub sequence: u64,
    pub operation: &'static str,
    pub state: ProgressState,
    pub phase: ProgressPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_completed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_requests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ProgressErrorCode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProgressEvent {
    CheckingRelease(ProgressDetails),
    Downloading(DownloadProgressDetails),
    Verifying(ProgressDetails),
    Installing(ProgressDetails),
    PreparingService(ProgressDetails),
    Draining(ProgressDetails),
    Stopping(ProgressDetails),
    Starting(ProgressDetails),
    VerifyingRuntime(ProgressDetails),
    RollingBack(ProgressDetails),
    Completed(ProgressDetails),
}

impl ProgressEvent {
    fn into_parts(self) -> (ProgressPhase, EventDetails) {
        match self {
            Self::CheckingRelease(details) => (
                ProgressPhase::CheckingRelease,
                EventDetails::Standard(details),
            ),
            Self::Downloading(details) => (
                ProgressPhase::Downloading,
                EventDetails::Downloading(details),
            ),
            Self::Verifying(details) => (ProgressPhase::Verifying, EventDetails::Standard(details)),
            Self::Installing(details) => {
                (ProgressPhase::Installing, EventDetails::Standard(details))
            }
            Self::PreparingService(details) => (
                ProgressPhase::PreparingService,
                EventDetails::Standard(details),
            ),
            Self::Draining(details) => (ProgressPhase::Draining, EventDetails::Standard(details)),
            Self::Stopping(details) => (ProgressPhase::Stopping, EventDetails::Standard(details)),
            Self::Starting(details) => (ProgressPhase::Starting, EventDetails::Standard(details)),
            Self::VerifyingRuntime(details) => (
                ProgressPhase::VerifyingRuntime,
                EventDetails::Standard(details),
            ),
            Self::RollingBack(details) => {
                (ProgressPhase::RollingBack, EventDetails::Standard(details))
            }
            Self::Completed(details) => (ProgressPhase::Completed, EventDetails::Standard(details)),
        }
    }
}

enum EventDetails {
    Standard(ProgressDetails),
    Downloading(DownloadProgressDetails),
}

impl EventDetails {
    fn into_fields(
        self,
    ) -> (
        Option<String>,
        Option<String>,
        Option<u64>,
        Option<u64>,
        Option<usize>,
    ) {
        match self {
            Self::Standard(details) => (
                details.current_version,
                details.target_version,
                None,
                None,
                details.active_requests,
            ),
            Self::Downloading(details) => (
                details.current_version,
                details.target_version,
                Some(details.bytes_completed),
                Some(details.bytes_total),
                details.active_requests,
            ),
        }
    }
}

pub(super) struct ProgressReporter {
    enabled: bool,
    next_sequence: u64,
    write_failed: bool,
}

impl ProgressReporter {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_sequence: 0,
            write_failed: false,
        }
    }

    pub(super) fn running(&mut self, output: &mut dyn CommandOutput, event: ProgressEvent) {
        self.emit(output, ProgressState::Running, event, None);
    }

    pub(super) fn succeeded(&mut self, output: &mut dyn CommandOutput, event: ProgressEvent) {
        self.emit(output, ProgressState::Succeeded, event, None);
    }

    pub(super) fn failed(
        &mut self,
        output: &mut dyn CommandOutput,
        event: ProgressEvent,
        error_code: ProgressErrorCode,
    ) {
        self.emit(output, ProgressState::Failed, event, Some(error_code));
    }

    fn emit(
        &mut self,
        output: &mut dyn CommandOutput,
        state: ProgressState,
        event: ProgressEvent,
        error_code: Option<ProgressErrorCode>,
    ) {
        if !self.enabled || self.write_failed {
            return;
        }

        let (phase, details) = event.into_parts();
        let (current_version, target_version, bytes_completed, bytes_total, active_requests) =
            details.into_fields();
        let event = CoreOperationProgress {
            schema_version: 1,
            sequence: self.next_sequence,
            operation: "update",
            state,
            phase,
            current_version,
            target_version,
            bytes_completed,
            bytes_total,
            active_requests,
            error_code,
        };
        let line = match serde_json::to_string(&event) {
            Ok(serialized) => serialized,
            Err(_) => {
                self.write_failed = true;
                return;
            }
        };
        self.next_sequence += 1;

        if output.write_stderr(&format!("{line}\n")).is_err() {
            self.write_failed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use serde_json::Value;

    use super::{
        DownloadProgressDetails, ProgressDetails, ProgressErrorCode, ProgressEvent,
        ProgressReporter, ProgressState,
    };
    use crate::{BufferOutput, CommandOutput};

    #[test]
    fn reporter_writes_compact_monotonic_json_lines() {
        let mut output = BufferOutput::default();
        let mut reporter = ProgressReporter::new(true);
        reporter.running(
            &mut output,
            ProgressEvent::CheckingRelease(ProgressDetails::default()),
        );
        reporter.running(
            &mut output,
            ProgressEvent::Downloading(DownloadProgressDetails {
                current_version: Some("0.1.0".into()),
                target_version: Some("0.1.1".into()),
                bytes_completed: 7,
                bytes_total: 11,
                active_requests: None,
            }),
        );

        let lines = output.stderr().lines().collect::<Vec<_>>();
        let events = lines
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(events[0]["schema_version"], 1);
        assert_eq!(events[0]["operation"], "update");
        assert_eq!(events[0]["state"], "running");
        assert_eq!(events[0]["phase"], "checking_release");
        assert_eq!(events[0]["sequence"], 0);
        assert_eq!(events[1]["sequence"], 1);
        assert_eq!(events[1]["bytes_completed"], 7);
        assert_eq!(events[1]["bytes_total"], 11);
        assert!(events[1].get("active_requests").is_none());
        assert!(lines.iter().all(|line| !line.contains("\n")));
    }

    #[test]
    fn reporter_serializes_terminal_states() {
        let mut output = BufferOutput::default();
        let mut reporter = ProgressReporter::new(true);

        reporter.succeeded(
            &mut output,
            ProgressEvent::Completed(ProgressDetails::default()),
        );
        reporter.failed(
            &mut output,
            ProgressEvent::RollingBack(ProgressDetails {
                active_requests: Some(2),
                ..ProgressDetails::default()
            }),
            ProgressErrorCode::UpdateInstallFailed,
        );

        let events = output
            .stderr()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events[0]["state"],
            serde_json::json!(ProgressState::Succeeded)
        );
        assert_eq!(events[0]["phase"], "completed");
        assert_eq!(events[1]["state"], serde_json::json!(ProgressState::Failed));
        assert_eq!(events[1]["phase"], "rolling_back");
        assert_eq!(events[1]["active_requests"], 2);
        assert_eq!(events[1]["error_code"], "update_install_failed");
    }

    #[test]
    fn reporter_serializes_only_the_documented_error_codes() {
        let mut output = BufferOutput::default();
        let mut reporter = ProgressReporter::new(true);

        for error_code in [
            ProgressErrorCode::UpdateUnavailable,
            ProgressErrorCode::IncompatibleManifest,
            ProgressErrorCode::UpdateVerificationFailed,
            ProgressErrorCode::UpdateInstallFailed,
            ProgressErrorCode::ActiveRequestsRemain,
            ProgressErrorCode::RolledBack,
            ProgressErrorCode::RecoveryRequired,
            ProgressErrorCode::OperationInProgress,
        ] {
            reporter.failed(
                &mut output,
                ProgressEvent::RollingBack(ProgressDetails::default()),
                error_code,
            );
        }

        let error_codes = output
            .stderr()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["error_code"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            error_codes,
            [
                "update_unavailable",
                "incompatible_manifest",
                "update_verification_failed",
                "update_install_failed",
                "active_requests_remain",
                "rolled_back",
                "recovery_required",
                "operation_in_progress",
            ]
            .map(Value::from)
        );
    }

    #[test]
    fn non_downloading_events_omit_bytes_while_downloading_requires_both_counts() {
        let mut output = BufferOutput::default();
        let mut reporter = ProgressReporter::new(true);

        reporter.running(
            &mut output,
            ProgressEvent::CheckingRelease(ProgressDetails::default()),
        );
        reporter.running(
            &mut output,
            ProgressEvent::Downloading(DownloadProgressDetails {
                current_version: None,
                target_version: None,
                bytes_completed: 0,
                bytes_total: 0,
                active_requests: None,
            }),
        );

        let events = output
            .stderr()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(events[0].get("bytes_completed").is_none());
        assert!(events[0].get("bytes_total").is_none());
        assert_eq!(events[1]["bytes_completed"], 0);
        assert_eq!(events[1]["bytes_total"], 0);
    }

    #[test]
    fn reporter_stops_attempting_output_after_a_write_failure_and_when_disabled() {
        let mut output = FailingOutput::default();
        let mut reporter = ProgressReporter::new(true);

        reporter.running(
            &mut output,
            ProgressEvent::CheckingRelease(ProgressDetails::default()),
        );
        reporter.running(
            &mut output,
            ProgressEvent::Verifying(ProgressDetails::default()),
        );
        assert_eq!(output.stderr_attempts, 1);

        let mut disabled = ProgressReporter::new(false);
        disabled.running(
            &mut output,
            ProgressEvent::CheckingRelease(ProgressDetails::default()),
        );
        assert_eq!(output.stderr_attempts, 1);
    }

    #[derive(Default)]
    struct FailingOutput {
        stderr_attempts: usize,
    }

    impl CommandOutput for FailingOutput {
        fn write_stdout(&mut self, _value: &str) -> io::Result<()> {
            Ok(())
        }

        fn write_stderr(&mut self, _value: &str) -> io::Result<()> {
            self.stderr_attempts += 1;
            Err(io::Error::other("broken progress pipe"))
        }
    }
}
