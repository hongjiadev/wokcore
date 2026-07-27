use std::path::Path;

use wokcore_storage::{
    CandidateBeginOutcome, CleanupBatchOutcome, CodexReplaySignaturePage, ReplaySignaturePageKey,
    SessionBatch, SessionIndexPage, SessionIndexPageKey, SessionIndexRecord, SessionScanCursor,
    SessionSourceErrorCode, SessionSourcePage, SessionSourcePageKey, SessionSourceState,
    SessionUsagePage, SessionUsagePageKey, StateStore, StateStoreWriteError,
    StateStoreWriteReceipt, StateStoreWriterClient, StateStoreWriterSubmitError, StorageError,
    SupplementalBatchOutcome,
};

pub(crate) struct SessionState {
    reader: StateStore,
    writer: Option<StateStoreWriterClient>,
}

impl SessionState {
    pub(crate) fn open(
        state_path: impl AsRef<Path>,
        writer: Option<StateStoreWriterClient>,
    ) -> Result<Self, StorageError> {
        let reader = if writer.is_some() {
            StateStore::open_live_reader(state_path)?
        } else {
            StateStore::open(state_path)?
        };
        Ok(Self { reader, writer })
    }

    pub(crate) fn open_read_only(state_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Ok(Self {
            reader: StateStore::open_live_reader(state_path)?,
            writer: None,
        })
    }

    pub(crate) fn reader(&self) -> &StateStore {
        &self.reader
    }

    pub(crate) fn load_current_session_scan_cursor(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionScanCursor>, StorageError> {
        self.reader.load_current_session_scan_cursor(source_key)
    }

    pub(crate) fn load_staging_session_scan_cursor(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionScanCursor>, StorageError> {
        self.reader.load_staging_session_scan_cursor(source_key)
    }

    pub(crate) fn load_session_source(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionSourceState>, StorageError> {
        self.reader.load_session_source(source_key)
    }

    pub(crate) fn load_staging_session_index_record(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionIndexRecord>, StorageError> {
        self.reader.load_staging_session_index_record(source_key)
    }

    pub(crate) fn load_session_sources_page(
        &self,
        after: Option<&SessionSourcePageKey>,
        limit: usize,
    ) -> Result<SessionSourcePage, StorageError> {
        self.reader.load_session_sources_page(after, limit)
    }

    pub(crate) fn load_current_session_index_page(
        &self,
        source_key: &str,
        after: Option<&SessionIndexPageKey>,
        limit: usize,
    ) -> Result<SessionIndexPage, StorageError> {
        self.reader
            .load_current_session_index_page(source_key, after, limit)
    }

    pub(crate) fn load_current_session_usage_page(
        &self,
        source_key: &str,
        after: Option<&SessionUsagePageKey>,
        limit: usize,
    ) -> Result<SessionUsagePage, StorageError> {
        self.reader
            .load_current_session_usage_page(source_key, after, limit)
    }

    pub(crate) fn load_codex_replay_signature_page(
        &self,
        parent_source_key: &str,
        parent_generation: u64,
        after: Option<&ReplaySignaturePageKey>,
        limit: usize,
    ) -> Result<CodexReplaySignaturePage, StorageError> {
        self.reader.load_codex_replay_signature_page(
            parent_source_key,
            parent_generation,
            after,
            limit,
        )
    }

    pub(crate) fn codex_replay_index_is_complete(
        &self,
        parent_source_key: &str,
        parent_generation: u64,
        expected_events: u64,
    ) -> Result<bool, StorageError> {
        self.reader.codex_replay_index_is_complete(
            parent_source_key,
            parent_generation,
            expected_events,
        )
    }

    pub(crate) fn begin_or_resume_candidate(
        &mut self,
        cursor: &SessionScanCursor,
    ) -> Result<CandidateBeginOutcome, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self.reader.begin_or_resume_candidate(cursor);
        };
        loop {
            let cursor = cursor.clone();
            match writer.try_execute(move |store| store.begin_or_resume_candidate(&cursor)) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn commit_candidate_batch(
        &mut self,
        batch: &SessionBatch,
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self.reader.commit_candidate_batch(batch);
        };
        loop {
            match writer.try_commit_candidate_batch(batch.clone()) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn commit_session_batch(
        &mut self,
        batch: &SessionBatch,
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self.reader.commit_session_batch(batch);
        };
        loop {
            match writer.try_commit_session_batch(batch.clone()) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn promote_candidate(
        &mut self,
        source_key: &str,
        generation: u64,
        transition_at: &str,
    ) -> Result<(), StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self
                .reader
                .promote_candidate(source_key, generation, transition_at);
        };
        loop {
            let source_key = source_key.to_owned();
            let transition_at = transition_at.to_owned();
            match writer.try_execute(move |store| {
                store.promote_candidate(&source_key, generation, &transition_at)
            }) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn fail_candidate(
        &mut self,
        source_key: &str,
        generation: u64,
        error_code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self
                .reader
                .fail_candidate(source_key, generation, error_code, transition_at);
        };
        loop {
            let source_key = source_key.to_owned();
            let transition_at = transition_at.to_owned();
            match writer.try_execute(move |store| {
                store.fail_candidate(&source_key, generation, error_code, &transition_at)
            }) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn record_source_success(
        &mut self,
        source_key: &str,
        generation: u64,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self
                .reader
                .record_source_success(source_key, generation, transition_at);
        };
        loop {
            let source_key = source_key.to_owned();
            let transition_at = transition_at.to_owned();
            match writer.try_execute(move |store| {
                store.record_source_success(&source_key, generation, &transition_at)
            }) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn mark_source_unavailable(
        &mut self,
        source_key: &str,
        error_code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self
                .reader
                .mark_source_unavailable(source_key, error_code, transition_at);
        };
        loop {
            let source_key = source_key.to_owned();
            let transition_at = transition_at.to_owned();
            match writer.try_execute(move |store| {
                store.mark_source_unavailable(&source_key, error_code, &transition_at)
            }) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }

    pub(crate) fn cleanup_generation_batch(
        &mut self,
        source_key: &str,
        generation: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<CleanupBatchOutcome, StorageError> {
        let Some(writer) = self.writer.as_ref() else {
            return self
                .reader
                .cleanup_generation_batch(source_key, generation, max_rows, max_bytes);
        };
        loop {
            let source_key = source_key.to_owned();
            match writer.try_execute(move |store| {
                store.cleanup_generation_batch(&source_key, generation, max_rows, max_bytes)
            }) {
                Ok(receipt) => return wait(receipt),
                Err(StateStoreWriterSubmitError::QueueFull) => std::thread::yield_now(),
                Err(StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StorageError::StateWriterUnavailable);
                }
            }
        }
    }
}

fn wait<T>(receipt: StateStoreWriteReceipt<T>) -> Result<T, StorageError> {
    match receipt.blocking_wait() {
        Ok(value) => Ok(value),
        Err(StateStoreWriteError::Storage(error)) => Err(error),
        Err(StateStoreWriteError::WriterStopped) => Err(StorageError::StateWriterUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use wokcore_storage::{
        ParserCheckpoint, SessionFileIdentity, SessionGenerationState, SessionScanResultCode,
        SessionSourceKind, state_store_writer,
    };

    use super::*;

    #[test]
    fn scanner_mutations_use_the_shared_writer_and_remain_visible_to_the_reader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = StateStore::open(&path).unwrap();
        let (client, shutdown, writer) = state_store_writer(store);
        let writer_thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(writer.run());
        });
        let mut state = SessionState::open(&path, Some(client.clone())).unwrap();
        let source_key = "01".repeat(32);
        let cursor = SessionScanCursor {
            source_key: source_key.clone(),
            source_kind: SessionSourceKind::Codex,
            generation: 1,
            generation_state: SessionGenerationState::Staging,
            file_identity: SessionFileIdentity::new("02".repeat(32)).unwrap(),
            observed_size: 128,
            modified_at: "2026-07-27T00:00:00Z".to_owned(),
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: ParserCheckpoint {
                version: 1,
                previous_input_tokens: 0,
                previous_output_tokens: 0,
                previous_cache_read_tokens: 0,
                previous_cache_write_tokens: 0,
                previous_reasoning_tokens: 0,
                current_model: None,
                event_ordinal: 0,
                lineage_source_key: None,
                lineage_generation: None,
                lineage_record_ordinal: 0,
                structural_hash: None,
            },
            head_fingerprint: [0x11; 32],
            boundary_fingerprint: [0x22; 32],
            parent_source_key: None,
            parent_generation: None,
            replay_boundary_fingerprint: None,
            result_code: Some(SessionScanResultCode::Advanced),
            result_changed_at: Some("2026-07-27T00:00:00Z".to_owned()),
        };

        assert_eq!(
            state.begin_or_resume_candidate(&cursor).unwrap(),
            CandidateBeginOutcome::Started
        );
        assert_eq!(
            state.load_staging_session_scan_cursor(&source_key).unwrap(),
            Some(cursor)
        );
        assert!(client.activity_revision() >= 2);

        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                shutdown.shutdown().await.unwrap().wait().await.unwrap();
            });
        writer_thread.join().unwrap();
    }
}
