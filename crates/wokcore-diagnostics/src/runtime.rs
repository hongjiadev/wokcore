use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Default)]
pub struct StreamRuntimeDiagnostics {
    inner: Arc<StreamRuntimeCounters>,
}

#[derive(Default)]
struct StreamRuntimeCounters {
    active: AtomicU64,
    completed: AtomicU64,
    protocol_errors: AtomicU64,
    upstream_errors: AtomicU64,
    cancelled: AtomicU64,
    frames: AtomicU64,
    bytes: AtomicU64,
    elapsed_micros: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamRuntimeSnapshot {
    pub active: u64,
    pub completed: u64,
    pub protocol_errors: u64,
    pub upstream_errors: u64,
    pub cancelled: u64,
    pub frames: u64,
    pub bytes: u64,
    pub elapsed_micros: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamRuntimeOutcome {
    Completed,
    ProtocolError,
    UpstreamError,
    Cancelled,
}

impl StreamRuntimeDiagnostics {
    pub fn start(&self) -> StreamRuntimeObservation {
        self.inner.active.fetch_add(1, Ordering::Relaxed);
        StreamRuntimeObservation {
            diagnostics: self.clone(),
            started_at: Instant::now(),
            frames: 0,
            bytes: 0,
            finished: false,
        }
    }

    pub fn snapshot(&self) -> StreamRuntimeSnapshot {
        StreamRuntimeSnapshot {
            active: self.inner.active.load(Ordering::Relaxed),
            completed: self.inner.completed.load(Ordering::Relaxed),
            protocol_errors: self.inner.protocol_errors.load(Ordering::Relaxed),
            upstream_errors: self.inner.upstream_errors.load(Ordering::Relaxed),
            cancelled: self.inner.cancelled.load(Ordering::Relaxed),
            frames: self.inner.frames.load(Ordering::Relaxed),
            bytes: self.inner.bytes.load(Ordering::Relaxed),
            elapsed_micros: self.inner.elapsed_micros.load(Ordering::Relaxed),
        }
    }
}

impl fmt::Debug for StreamRuntimeDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamRuntimeDiagnostics")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

pub struct StreamRuntimeObservation {
    diagnostics: StreamRuntimeDiagnostics,
    started_at: Instant,
    frames: u64,
    bytes: u64,
    finished: bool,
}

impl StreamRuntimeObservation {
    pub fn observe_frame(&mut self, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub fn finish(&mut self, outcome: StreamRuntimeOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.diagnostics
            .inner
            .active
            .fetch_sub(1, Ordering::Relaxed);
        self.diagnostics
            .inner
            .frames
            .fetch_add(self.frames, Ordering::Relaxed);
        self.diagnostics
            .inner
            .bytes
            .fetch_add(self.bytes, Ordering::Relaxed);
        let elapsed = u64::try_from(self.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.diagnostics
            .inner
            .elapsed_micros
            .fetch_add(elapsed, Ordering::Relaxed);
        match outcome {
            StreamRuntimeOutcome::Completed => &self.diagnostics.inner.completed,
            StreamRuntimeOutcome::ProtocolError => &self.diagnostics.inner.protocol_errors,
            StreamRuntimeOutcome::UpstreamError => &self.diagnostics.inner.upstream_errors,
            StreamRuntimeOutcome::Cancelled => &self.diagnostics.inner.cancelled,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

impl fmt::Debug for StreamRuntimeObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamRuntimeObservation")
            .field("frames", &self.frames)
            .field("bytes", &self.bytes)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Drop for StreamRuntimeObservation {
    fn drop(&mut self) {
        self.finish(StreamRuntimeOutcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamRuntimeDiagnostics, StreamRuntimeOutcome};

    #[test]
    fn streaming_observations_publish_coarse_counters_only_at_completion() {
        let diagnostics = StreamRuntimeDiagnostics::default();
        let mut observation = diagnostics.start();
        observation.observe_frame(11);
        observation.observe_frame(13);

        assert_eq!(diagnostics.snapshot().frames, 0);
        observation.finish(StreamRuntimeOutcome::ProtocolError);

        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.protocol_errors, 1);
        assert_eq!(snapshot.frames, 2);
        assert_eq!(snapshot.bytes, 24);
    }

    #[test]
    fn streaming_dropped_observation_records_one_cancellation() {
        let diagnostics = StreamRuntimeDiagnostics::default();
        let observation = diagnostics.start();
        drop(observation);

        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.cancelled, 1);
    }
}
