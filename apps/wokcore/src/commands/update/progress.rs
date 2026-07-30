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
    pub bytes_completed: Option<u64>,
    pub bytes_total: Option<u64>,
    pub active_requests: Option<usize>,
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
    pub error_code: Option<&'static str>,
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

    pub(super) fn running(
        &mut self,
        output: &mut dyn CommandOutput,
        phase: ProgressPhase,
        details: Option<ProgressDetails>,
    ) {
        self.emit(output, ProgressState::Running, phase, details, None);
    }

    pub(super) fn succeeded(
        &mut self,
        output: &mut dyn CommandOutput,
        phase: ProgressPhase,
        details: Option<ProgressDetails>,
    ) {
        self.emit(output, ProgressState::Succeeded, phase, details, None);
    }

    pub(super) fn failed(
        &mut self,
        output: &mut dyn CommandOutput,
        phase: ProgressPhase,
        details: Option<ProgressDetails>,
        error_code: &'static str,
    ) {
        self.emit(
            output,
            ProgressState::Failed,
            phase,
            details,
            Some(error_code),
        );
    }

    fn emit(
        &mut self,
        output: &mut dyn CommandOutput,
        state: ProgressState,
        phase: ProgressPhase,
        details: Option<ProgressDetails>,
        error_code: Option<&'static str>,
    ) {
        if !self.enabled || self.write_failed {
            return;
        }

        let details = details.unwrap_or_default();
        let event = CoreOperationProgress {
            schema_version: 1,
            sequence: self.next_sequence,
            operation: "update",
            state,
            phase,
            current_version: details.current_version,
            target_version: details.target_version,
            bytes_completed: details.bytes_completed,
            bytes_total: details.bytes_total,
            active_requests: details.active_requests,
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

    use super::{ProgressDetails, ProgressPhase, ProgressReporter, ProgressState};
    use crate::{BufferOutput, CommandOutput};

    #[test]
    fn reporter_writes_compact_monotonic_json_lines() {
        let mut output = BufferOutput::default();
        let mut reporter = ProgressReporter::new(true);
        reporter.running(&mut output, ProgressPhase::CheckingRelease, None);
        reporter.running(
            &mut output,
            ProgressPhase::Downloading,
            Some(ProgressDetails {
                current_version: Some("0.1.0".into()),
                target_version: Some("0.1.1".into()),
                bytes_completed: Some(7),
                bytes_total: Some(11),
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

        reporter.succeeded(&mut output, ProgressPhase::Completed, None);
        reporter.failed(
            &mut output,
            ProgressPhase::RollingBack,
            Some(ProgressDetails {
                active_requests: Some(2),
                ..ProgressDetails::default()
            }),
            "update_failed",
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
        assert_eq!(events[1]["error_code"], "update_failed");
    }

    #[test]
    fn reporter_stops_attempting_output_after_a_write_failure_and_when_disabled() {
        let mut output = FailingOutput::default();
        let mut reporter = ProgressReporter::new(true);

        reporter.running(&mut output, ProgressPhase::CheckingRelease, None);
        reporter.running(&mut output, ProgressPhase::Verifying, None);
        assert_eq!(output.stderr_attempts, 1);

        let mut disabled = ProgressReporter::new(false);
        disabled.running(&mut output, ProgressPhase::CheckingRelease, None);
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
