use std::fmt;

use serde_json::Value;
use wokcore_platform::sessions::{SessionError, SessionFile};

pub const MAX_JSONL_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_JSONL_RECORDS_PER_SCAN: usize = 512;
pub const MAX_JSONL_BATCH_INPUT_BYTES: usize = 16 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlRecordStatus {
    Valid,
    InvalidUtf8,
    InvalidJson,
}

#[derive(Clone)]
pub struct JsonlRecord {
    pub ordinal: u64,
    pub byte_end: u64,
    pub status: JsonlRecordStatus,
    value: Option<Value>,
}

impl JsonlRecord {
    pub fn invalid(ordinal: u64, byte_end: u64, status: JsonlRecordStatus) -> Self {
        debug_assert!(status != JsonlRecordStatus::Valid);
        Self {
            ordinal,
            byte_end,
            status,
            value: None,
        }
    }

    pub fn value(&self) -> &Value {
        self.value
            .as_ref()
            .expect("only valid JSONL records expose a parsed value")
    }
}

impl fmt::Debug for JsonlRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsonlRecord")
            .field("ordinal", &self.ordinal)
            .field("byte_end", &self.byte_end)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct JsonlScan {
    pub complete_byte_offset: u64,
    pub next_record_ordinal: u64,
    pub records: Vec<JsonlRecord>,
    pub peak_buffer_bytes: usize,
    pub read_bytes: u64,
    pub reached_end: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum JsonlError {
    #[error("Session JSONL record exceeds the bounded line limit")]
    RecordTooLarge { peak_buffer_bytes: usize },
    #[error("Session JSONL source changed during bounded parsing")]
    SourceChanged,
    #[error("Session JSONL source is unavailable")]
    SourceUnavailable,
    #[error("Session JSONL read failed")]
    ReadFailed,
    #[error("Session JSONL cursor exceeds its numeric bound")]
    CursorOverflow,
}

impl JsonlError {
    pub fn stable_code(&self) -> &'static str {
        match self {
            Self::RecordTooLarge { .. } => "session_record_too_large",
            Self::SourceChanged => "session_source_changed",
            Self::SourceUnavailable => "session_source_unavailable",
            Self::ReadFailed => "session_read_failed",
            Self::CursorOverflow => "session_cursor_overflow",
        }
    }

    pub fn peak_buffer_bytes(&self) -> usize {
        match self {
            Self::RecordTooLarge { peak_buffer_bytes } => *peak_buffer_bytes,
            _ => 0,
        }
    }
}

impl From<SessionError> for JsonlError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::SessionFileChanged | SessionError::UnsafePath => Self::SourceChanged,
            SessionError::SessionFileUnavailable => Self::SourceUnavailable,
            SessionError::MissingPlatformData { .. }
            | SessionError::EnumerationLimitExceeded
            | SessionError::ReadLimitExceeded
            | SessionError::Io { .. } => Self::ReadFailed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonlCursor {
    complete_byte_offset: u64,
    next_record_ordinal: u64,
}

impl JsonlCursor {
    pub fn new(complete_byte_offset: u64, next_record_ordinal: u64) -> Self {
        Self {
            complete_byte_offset,
            next_record_ordinal: next_record_ordinal.max(1),
        }
    }

    pub fn scan(self, file: &mut SessionFile) -> Result<JsonlScan, JsonlError> {
        JsonlReader::new(self).scan(file)
    }
}

pub(crate) struct JsonlReader {
    complete_byte_offset: u64,
    next_record_ordinal: u64,
    buffer_start: u64,
    read_offset: u64,
    buffer: Vec<u8>,
}

impl JsonlReader {
    pub(crate) fn new(cursor: JsonlCursor) -> Self {
        Self {
            complete_byte_offset: cursor.complete_byte_offset,
            next_record_ordinal: cursor.next_record_ordinal,
            buffer_start: cursor.complete_byte_offset,
            read_offset: cursor.complete_byte_offset,
            buffer: Vec::new(),
        }
    }

    pub(crate) fn scan(&mut self, file: &mut SessionFile) -> Result<JsonlScan, JsonlError> {
        self.scan_bounded(
            file,
            MAX_JSONL_RECORDS_PER_SCAN,
            MAX_JSONL_BATCH_INPUT_BYTES,
        )
    }

    pub(crate) fn scan_bounded(
        &mut self,
        file: &mut SessionFile,
        max_records: usize,
        max_batch_input_bytes: usize,
    ) -> Result<JsonlScan, JsonlError> {
        let file_size = file.snapshot().size;
        let max_records = max_records.clamp(1, MAX_JSONL_RECORDS_PER_SCAN);
        let max_batch_input_bytes = max_batch_input_bytes.clamp(1, MAX_JSONL_BATCH_INPUT_BYTES);
        let scan_start = self.complete_byte_offset;
        let mut committed_offset = scan_start;
        let mut next_ordinal = self.next_record_ordinal;
        let mut line_start = 0usize;
        let mut records = Vec::with_capacity(max_records);
        let mut peak_buffer_bytes = 0;
        let mut read_bytes = 0u64;
        let mut batch_full = false;

        while records.len() < max_records {
            if let Some(relative_newline) = self.buffer[line_start..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let newline = line_start
                    .checked_add(relative_newline)
                    .ok_or(JsonlError::CursorOverflow)?;
                let line_bytes = newline
                    .checked_sub(line_start)
                    .and_then(|length| length.checked_add(1))
                    .ok_or(JsonlError::CursorOverflow)?;
                peak_buffer_bytes = peak_buffer_bytes.max(line_bytes);
                if line_bytes > MAX_JSONL_LINE_BYTES {
                    return Err(JsonlError::RecordTooLarge { peak_buffer_bytes });
                }
                let byte_end = self
                    .buffer_start
                    .checked_add(newline as u64)
                    .and_then(|offset| offset.checked_add(1))
                    .ok_or(JsonlError::CursorOverflow)?;
                let batch_bytes = byte_end
                    .checked_sub(scan_start)
                    .ok_or(JsonlError::CursorOverflow)?;
                if !records.is_empty() && batch_bytes > max_batch_input_bytes as u64 {
                    batch_full = true;
                    break;
                }
                let content = &self.buffer[line_start..newline];
                let record = match std::str::from_utf8(content) {
                    Err(_) => {
                        JsonlRecord::invalid(next_ordinal, byte_end, JsonlRecordStatus::InvalidUtf8)
                    }
                    Ok(text) => match serde_json::from_str(text) {
                        Ok(value) => JsonlRecord {
                            ordinal: next_ordinal,
                            byte_end,
                            status: JsonlRecordStatus::Valid,
                            value: Some(value),
                        },
                        Err(_) => JsonlRecord::invalid(
                            next_ordinal,
                            byte_end,
                            JsonlRecordStatus::InvalidJson,
                        ),
                    },
                };
                records.push(record);
                committed_offset = byte_end;
                next_ordinal = next_ordinal
                    .checked_add(1)
                    .ok_or(JsonlError::CursorOverflow)?;
                line_start = newline.checked_add(1).ok_or(JsonlError::CursorOverflow)?;
                continue;
            }

            let incomplete_bytes = self
                .buffer
                .len()
                .checked_sub(line_start)
                .ok_or(JsonlError::CursorOverflow)?;
            peak_buffer_bytes = peak_buffer_bytes.max(incomplete_bytes);
            if incomplete_bytes > MAX_JSONL_LINE_BYTES {
                return Err(JsonlError::RecordTooLarge { peak_buffer_bytes });
            }
            if self.read_offset >= file_size {
                break;
            }
            let remaining_line_capacity = MAX_JSONL_LINE_BYTES
                .saturating_add(1)
                .saturating_sub(incomplete_bytes);
            let maximum_read = READ_CHUNK_BYTES.min(remaining_line_capacity.max(1));
            let chunk = file.read_range_bounded(self.read_offset, maximum_read)?;
            if chunk.is_empty() {
                break;
            }
            self.read_offset = self
                .read_offset
                .checked_add(chunk.len() as u64)
                .ok_or(JsonlError::CursorOverflow)?;
            read_bytes = read_bytes
                .checked_add(chunk.len() as u64)
                .ok_or(JsonlError::CursorOverflow)?;
            self.buffer.extend_from_slice(&chunk);
        }

        if line_start != 0 {
            self.buffer.drain(..line_start);
            self.buffer_start = committed_offset;
        }
        self.complete_byte_offset = committed_offset;
        self.next_record_ordinal = next_ordinal;
        let reached_end =
            !batch_full && self.read_offset >= file_size && !self.buffer.contains(&b'\n');

        Ok(JsonlScan {
            complete_byte_offset: committed_offset,
            next_record_ordinal: next_ordinal,
            records,
            peak_buffer_bytes,
            read_bytes,
            reached_end,
        })
    }
}
