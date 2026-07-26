use std::{cell::Cell, collections::HashMap, fmt, path::Path};

use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, value::RawValue};
use sha2::{Digest, Sha256};
use wokcore_platform::sessions::{
    SessionError, SessionFile, SessionFileSnapshot, SessionRootLease,
};
use wokcore_storage::{
    SessionFileIdentity, SessionScanCursor, SessionSourceKind, StateStore, StorageError,
};

use crate::{
    claude, codex,
    gemini::{self, GeminiFormat},
    model::{OpaqueStreamHash, normalize_timestamp, opaque_hash},
};

pub const MAX_MESSAGE_PAGE_MESSAGES: usize = 128;
pub const MAX_MESSAGE_PAGE_UTF8_BYTES: usize = 256 * 1024;
pub const MAX_JSONL_PAGE_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MESSAGE_ITEM_UTF8_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_TOOLS: usize = 32;
const MAX_TOOL_NAME_UTF8_BYTES: usize = 256;
const MAX_ACTIVE_PAGE_REFS: usize = 16_384;
const MAX_MESSAGE_REF_WORKING_BYTES: usize = 512 * 1024;
const MAX_JSONL_PAGE_RECORD_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_PAGE_SEEK_WORK_BYTES: u64 =
    (MAX_MESSAGE_PAGE_MESSAGES * MAX_JSONL_PAGE_RECORD_BYTES) as u64;
const JSONL_PAGE_READ_CHUNK_BYTES: usize = 64 * 1024;
const LEGACY_READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_LEGACY_PAGE_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
const CURSOR_PAYLOAD_BYTES: usize = 34;
const CURSOR_NONCE_BYTES: usize = 16;
const CURSOR_TAG_BYTES: usize = 32;
const CURSOR_ENCODED_BYTES: usize =
    (CURSOR_NONCE_BYTES + CURSOR_PAYLOAD_BYTES + CURSOR_TAG_BYTES) * 2;
const CURSOR_SOURCE_BIND_BYTES: usize = 16;
const CURSOR_VERSION: u8 = 2;
const CURSOR_NONCE_DOMAIN: &[u8] = b"wokcore.message-page.cursor-nonce.v1";
const CURSOR_MASK_DOMAIN: &[u8] = b"wokcore.message-page.cursor-mask.v1";
const CURSOR_MAC_DOMAIN: &[u8] = b"wokcore.message-page.cursor-mac.v1";
const CURSOR_SOURCE_DOMAIN: &[u8] = b"wokcore.message-page.cursor-source.v1";
const LOGICAL_REF_DOMAIN: &[u8] = b"wokcore.message-page.logical-ref.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageToolType {
    Call,
    Result,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageTool {
    pub tool_type: MessageToolType,
    pub name: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: String,
    pub tools: Vec<MessageTool>,
}

impl fmt::Debug for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Message")
            .field("role", &self.role)
            .field("content", &"<redacted>")
            .field("timestamp", &self.timestamp)
            .field("tools", &self.tools)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessagePageCursor(String);

impl MessagePageCursor {
    pub fn parse(value: &str) -> Result<Self, MessagePagerError> {
        if value.len() != CURSOR_ENCODED_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(MessagePagerError::InvalidCursor);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MessagePageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MessagePageCursor(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagePageRequest {
    pub maximum_messages: usize,
    pub maximum_utf8_bytes: usize,
    pub cursor: Option<MessagePageCursor>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<MessagePageCursor>,
}

impl fmt::Debug for MessagePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagePage")
            .field("message_count", &self.messages.len())
            .field("next_cursor", &self.next_cursor)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessagePagerMetrics {
    pub parser_read_bytes: u64,
    pub parser_examined_bytes: u64,
    pub parser_compacted_bytes: u64,
    pub message_seek_read_bytes: u64,
    pub peak_parser_buffer_bytes: usize,
    pub jsonl_index_cache_hits: u64,
    pub jsonl_index_rebuilds: u64,
    pub legacy_index_cache_hits: u64,
    pub legacy_index_rebuilds: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum MessagePagerError {
    #[error("Session message storage read failed")]
    Storage(#[from] StorageError),
    #[error("Session message root is unavailable")]
    Root,
    #[error("Session message source is unavailable")]
    SourceUnavailable,
    #[error("Session message page limit is invalid")]
    InvalidPageLimit,
    #[error("Session message cursor is invalid")]
    InvalidCursor,
    #[error("Session message cursor is stale")]
    StaleCursor,
    #[error("Session message read failed")]
    Read,
    #[error("Session message source exceeds its bounded resource limit")]
    ResourceLimit,
}

pub struct MessagePager {
    kind: SessionSourceKind,
    root: SessionRootLease,
    state: StateStore,
    domain_key: [u8; 32],
    jsonl_cache: Option<JsonlRefCache>,
    legacy_cache: Option<LegacyRefCache>,
    metrics: MessagePagerMetrics,
}

impl MessagePager {
    pub fn open(
        kind: SessionSourceKind,
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
    ) -> Result<Self, MessagePagerError> {
        Ok(Self {
            kind,
            root: SessionRootLease::open(root_path).map_err(|_| MessagePagerError::Root)?,
            state: StateStore::open(state_path)?,
            domain_key,
            jsonl_cache: None,
            legacy_cache: None,
            metrics: MessagePagerMetrics::default(),
        })
    }

    pub fn metrics(&self) -> MessagePagerMetrics {
        self.metrics
    }

    pub fn page(
        &mut self,
        source_key: &str,
        request: MessagePageRequest,
    ) -> Result<MessagePage, MessagePagerError> {
        validate_page_limits(&request)?;
        let source = self
            .state
            .load_session_source(source_key)?
            .ok_or(MessagePagerError::SourceUnavailable)?;
        if source.source_kind != self.kind {
            return Err(MessagePagerError::SourceUnavailable);
        }
        let scan_cursor = self
            .state
            .load_current_session_scan_cursor(source_key)?
            .ok_or(MessagePagerError::SourceUnavailable)?;
        if scan_cursor.source_kind != self.kind {
            return Err(MessagePagerError::SourceUnavailable);
        }
        self.invalidate_jsonl_cache(source_key, &scan_cursor);
        let position = request.cursor.as_ref().map_or(Ok(0), |cursor| {
            self.decode_cursor(cursor, source_key, &scan_cursor)
        })?;
        match self.kind {
            SessionSourceKind::Codex => {
                let mut file = codex::open_source_for_paging(
                    &self.root,
                    &self.domain_key,
                    &scan_cursor,
                    u64::MAX,
                )
                .map_err(|error| match error {
                    codex::CodexScannerError::RecordTooLarge => MessagePagerError::ResourceLimit,
                    codex::CodexScannerError::Parse => MessagePagerError::StaleCursor,
                    _ => MessagePagerError::Read,
                })?;
                self.jsonl_cache = None;
                self.legacy_cache = None;
                self.page_codex_jsonl(&mut file, source_key, &scan_cursor, position, request)
            }
            SessionSourceKind::Claude => {
                let opened = claude::open_source_for_paging(
                    &self.root,
                    &self.domain_key,
                    &scan_cursor,
                    MAX_JSONL_PAGE_SOURCE_WORK_BYTES,
                );
                let mut file = match opened {
                    Ok(file) => file,
                    Err(claude::ClaudeScannerError::ResourceLimit) => {
                        return Err(MessagePagerError::ResourceLimit);
                    }
                    Err(claude::ClaudeScannerError::Parse) => {
                        self.jsonl_cache = None;
                        return Err(MessagePagerError::StaleCursor);
                    }
                    Err(_) => return Err(MessagePagerError::Read),
                };
                self.legacy_cache = None;
                self.page_indexed_jsonl(
                    &mut file,
                    source_key,
                    &scan_cursor,
                    position,
                    request,
                    JsonlKind::Claude,
                )
            }
            SessionSourceKind::Gemini => {
                let opened = gemini::open_source_for_paging(
                    &self.root,
                    &self.domain_key,
                    &scan_cursor,
                    MAX_JSONL_PAGE_SOURCE_WORK_BYTES,
                );
                let (mut file, format) = match opened {
                    Ok(opened) => opened,
                    Err(gemini::GeminiScannerError::ResourceLimit) => {
                        return Err(MessagePagerError::ResourceLimit);
                    }
                    Err(gemini::GeminiScannerError::Parse) => {
                        self.jsonl_cache = None;
                        return Err(MessagePagerError::StaleCursor);
                    }
                    Err(_) => return Err(MessagePagerError::Read),
                };
                match format {
                    GeminiFormat::CurrentJsonl => {
                        self.legacy_cache = None;
                        self.page_indexed_jsonl(
                            &mut file,
                            source_key,
                            &scan_cursor,
                            position,
                            request,
                            JsonlKind::Gemini,
                        )
                    }
                    GeminiFormat::LegacyJson => self.page_legacy_gemini(
                        &mut file,
                        source_key,
                        &scan_cursor,
                        position,
                        request,
                    ),
                }
            }
        }
    }

    fn page_codex_jsonl(
        &mut self,
        file: &mut SessionFile,
        source_key: &str,
        scan_cursor: &SessionScanCursor,
        position: u64,
        request: MessagePageRequest,
    ) -> Result<MessagePage, MessagePagerError> {
        if position > scan_cursor.complete_byte_offset {
            return Err(MessagePagerError::InvalidCursor);
        }
        let mut work = SourceWorkBudget::default();
        let result = (|| {
            let mut reader =
                BoundedJsonlReader::new(position, 1, scan_cursor.complete_byte_offset)?;
            let mut messages = Vec::with_capacity(request.maximum_messages);
            let mut utf8_bytes = 0usize;
            let mut next_offset = None;
            while let Some(record) = reader.next(file, JsonlKind::Claude, &mut work)? {
                let Some(value) = record.value.as_ref() else {
                    continue;
                };
                let Some(message) = parse_codex_message(value) else {
                    continue;
                };
                let item_bytes = message_utf8_bytes(&message);
                if item_bytes > MAX_MESSAGE_ITEM_UTF8_BYTES
                    || item_bytes > request.maximum_utf8_bytes
                {
                    continue;
                }
                if utf8_bytes.saturating_add(item_bytes) > request.maximum_utf8_bytes {
                    next_offset = Some(record.byte_start);
                    break;
                }
                utf8_bytes += item_bytes;
                messages.push(message);
                if messages.len() >= request.maximum_messages {
                    next_offset = (record.byte_end < scan_cursor.complete_byte_offset)
                        .then_some(record.byte_end);
                    break;
                }
            }
            let next_cursor = next_offset
                .map(|offset| self.encode_cursor(source_key, scan_cursor, offset))
                .transpose()?;
            Ok(MessagePage {
                messages,
                next_cursor,
            })
        })();
        self.record_source_work(&work);
        result
    }

    fn page_indexed_jsonl(
        &mut self,
        file: &mut SessionFile,
        source_key: &str,
        scan_cursor: &SessionScanCursor,
        position: u64,
        request: MessagePageRequest,
        kind: JsonlKind,
    ) -> Result<MessagePage, MessagePagerError> {
        let position = usize::try_from(position).map_err(|_| MessagePagerError::InvalidCursor)?;
        let snapshot = file.snapshot().clone();
        let cache_matches = self.jsonl_cache.as_ref().is_some_and(|cache| {
            cache.source_key == source_key
                && cache.generation == scan_cursor.generation
                && cache.kind == kind
                && cache.extent == scan_cursor.complete_byte_offset
                && cache.structural_hash == scan_cursor.parser_checkpoint.structural_hash
                && cache.snapshot == snapshot
        });
        if self.jsonl_cache.is_some() && !cache_matches {
            self.jsonl_cache = None;
            return Err(MessagePagerError::StaleCursor);
        }
        let mut work = SourceWorkBudget::default();
        let result = (|| {
            if cache_matches {
                self.metrics.jsonl_index_cache_hits =
                    self.metrics.jsonl_index_cache_hits.saturating_add(1);
            } else {
                self.jsonl_cache = None;
                let (refs, structural_hash) = build_jsonl_refs(
                    file,
                    kind,
                    &self.domain_key,
                    scan_cursor.complete_byte_offset,
                    &mut work,
                )?;
                if kind == JsonlKind::Gemini
                    && (structural_hash.is_none()
                        || structural_hash != scan_cursor.parser_checkpoint.structural_hash)
                {
                    return Err(MessagePagerError::StaleCursor);
                }
                self.jsonl_cache = Some(JsonlRefCache {
                    source_key: source_key.to_owned(),
                    generation: scan_cursor.generation,
                    file_identity: scan_cursor.file_identity.clone(),
                    kind,
                    snapshot,
                    extent: scan_cursor.complete_byte_offset,
                    structural_hash,
                    refs,
                });
                self.metrics.jsonl_index_rebuilds =
                    self.metrics.jsonl_index_rebuilds.saturating_add(1);
            }
            let refs = &self
                .jsonl_cache
                .as_ref()
                .expect("the JSONL reference cache was populated")
                .refs;
            let mut current = position.min(refs.len());
            let mut messages = Vec::with_capacity(request.maximum_messages.min(refs.len()));
            let mut utf8_bytes = 0usize;
            while current < refs.len() && messages.len() < request.maximum_messages {
                let reference = refs[current].clone();
                current += 1;
                let Some(message) = read_jsonl_message(file, &reference, kind, &mut work)? else {
                    continue;
                };
                let item_bytes = message_utf8_bytes(&message);
                if item_bytes > MAX_MESSAGE_ITEM_UTF8_BYTES
                    || item_bytes > request.maximum_utf8_bytes
                {
                    continue;
                }
                if utf8_bytes.saturating_add(item_bytes) > request.maximum_utf8_bytes {
                    current -= 1;
                    break;
                }
                utf8_bytes += item_bytes;
                messages.push(message);
            }
            let next_cursor = (current < refs.len())
                .then(|| self.encode_cursor(source_key, scan_cursor, current as u64))
                .transpose()?;
            Ok(MessagePage {
                messages,
                next_cursor,
            })
        })();
        self.record_source_work(&work);
        result
    }

    fn page_legacy_gemini(
        &mut self,
        file: &mut SessionFile,
        source_key: &str,
        scan_cursor: &SessionScanCursor,
        position: u64,
        request: MessagePageRequest,
    ) -> Result<MessagePage, MessagePagerError> {
        let position = usize::try_from(position).map_err(|_| MessagePagerError::InvalidCursor)?;
        let snapshot = file.snapshot().clone();
        let cache_matches = self.legacy_cache.as_ref().is_some_and(|cache| {
            cache.source_key == source_key
                && cache.generation == scan_cursor.generation
                && cache.file_identity == scan_cursor.file_identity
                && cache.extent == scan_cursor.complete_byte_offset
                && cache.structural_hash == scan_cursor.parser_checkpoint.structural_hash
                && cache.snapshot == snapshot
        });
        if self.legacy_cache.is_some() && !cache_matches {
            self.legacy_cache = None;
            return Err(MessagePagerError::StaleCursor);
        }
        let mut work = SourceWorkBudget::default();
        let result = (|| {
            if cache_matches {
                self.metrics.legacy_index_cache_hits =
                    self.metrics.legacy_index_cache_hits.saturating_add(1);
            } else {
                self.legacy_cache = None;
                let (refs, structural_hash) = build_legacy_refs(
                    file,
                    scan_cursor.complete_byte_offset,
                    &self.domain_key,
                    &mut work,
                )?;
                if Some(structural_hash) != scan_cursor.parser_checkpoint.structural_hash {
                    return Err(MessagePagerError::StaleCursor);
                }
                self.legacy_cache = Some(LegacyRefCache {
                    source_key: source_key.to_owned(),
                    generation: scan_cursor.generation,
                    file_identity: scan_cursor.file_identity.clone(),
                    snapshot,
                    extent: scan_cursor.complete_byte_offset,
                    structural_hash: Some(structural_hash),
                    refs,
                });
                self.metrics.legacy_index_rebuilds =
                    self.metrics.legacy_index_rebuilds.saturating_add(1);
            }
            let refs = &self
                .legacy_cache
                .as_ref()
                .expect("the legacy reference cache was populated")
                .refs;
            if position > refs.len() {
                return Err(MessagePagerError::InvalidCursor);
            }
            let mut current = position;
            let mut messages = Vec::with_capacity(request.maximum_messages.min(refs.len()));
            let mut utf8_bytes = 0usize;
            while current < refs.len() && messages.len() < request.maximum_messages {
                let reference = refs[current];
                current += 1;
                let Some(message) = read_legacy_message(file, reference, &mut work)? else {
                    continue;
                };
                let item_bytes = message_utf8_bytes(&message);
                if item_bytes > MAX_MESSAGE_ITEM_UTF8_BYTES
                    || item_bytes > request.maximum_utf8_bytes
                {
                    continue;
                }
                if utf8_bytes.saturating_add(item_bytes) > request.maximum_utf8_bytes {
                    current -= 1;
                    break;
                }
                utf8_bytes += item_bytes;
                messages.push(message);
            }
            let next_cursor = (current < refs.len())
                .then(|| self.encode_cursor(source_key, scan_cursor, current as u64))
                .transpose()?;
            Ok(MessagePage {
                messages,
                next_cursor,
            })
        })();
        self.record_source_work(&work);
        result
    }

    fn encode_cursor(
        &self,
        source_key: &str,
        scan_cursor: &SessionScanCursor,
        position: u64,
    ) -> Result<MessagePageCursor, MessagePagerError> {
        let mut plain = [0u8; CURSOR_PAYLOAD_BYTES];
        plain[0] = CURSOR_VERSION;
        plain[1] = kind_byte(self.kind);
        plain[2..10].copy_from_slice(&scan_cursor.generation.to_be_bytes());
        plain[10..18].copy_from_slice(&position.to_be_bytes());
        let source_bind = opaque_hash(
            &self.domain_key,
            CURSOR_SOURCE_DOMAIN,
            &[source_key.as_bytes()],
        );
        plain[18..].copy_from_slice(&source_bind[..CURSOR_SOURCE_BIND_BYTES]);
        let nonce_full = hmac_sha256(&self.domain_key, CURSOR_NONCE_DOMAIN, &plain);
        let nonce = &nonce_full[..CURSOR_NONCE_BYTES];
        let mask = cursor_mask(&self.domain_key, nonce);
        let mut encrypted = plain;
        for (byte, mask) in encrypted.iter_mut().zip(mask) {
            *byte ^= mask;
        }
        let mut authenticated =
            Vec::with_capacity(CURSOR_NONCE_BYTES.saturating_add(CURSOR_PAYLOAD_BYTES));
        authenticated.extend_from_slice(nonce);
        authenticated.extend_from_slice(&encrypted);
        let tag = hmac_sha256(&self.domain_key, CURSOR_MAC_DOMAIN, &authenticated);
        let mut encoded = String::with_capacity(CURSOR_ENCODED_BYTES);
        push_hex(&mut encoded, nonce);
        push_hex(&mut encoded, &encrypted);
        push_hex(&mut encoded, &tag);
        Ok(MessagePageCursor(encoded))
    }

    fn decode_cursor(
        &self,
        cursor: &MessagePageCursor,
        source_key: &str,
        scan_cursor: &SessionScanCursor,
    ) -> Result<u64, MessagePagerError> {
        let decoded = decode_hex(cursor.as_str()).ok_or(MessagePagerError::InvalidCursor)?;
        let (nonce, remainder) = decoded.split_at(CURSOR_NONCE_BYTES);
        let (encrypted, supplied_tag) = remainder.split_at(CURSOR_PAYLOAD_BYTES);
        let expected_tag = hmac_sha256(
            &self.domain_key,
            CURSOR_MAC_DOMAIN,
            &decoded[..CURSOR_NONCE_BYTES + CURSOR_PAYLOAD_BYTES],
        );
        if !constant_time_equal(supplied_tag, &expected_tag) {
            return Err(MessagePagerError::InvalidCursor);
        }
        let mask = cursor_mask(&self.domain_key, nonce);
        let mut plain = [0u8; CURSOR_PAYLOAD_BYTES];
        for index in 0..plain.len() {
            plain[index] = encrypted[index] ^ mask[index];
        }
        let expected_nonce = hmac_sha256(&self.domain_key, CURSOR_NONCE_DOMAIN, &plain);
        if !constant_time_equal(nonce, &expected_nonce[..CURSOR_NONCE_BYTES]) {
            return Err(MessagePagerError::InvalidCursor);
        }
        if plain[0] != CURSOR_VERSION || plain[1] != kind_byte(self.kind) {
            return Err(MessagePagerError::InvalidCursor);
        }
        let generation = u64::from_be_bytes(
            plain[2..10]
                .try_into()
                .map_err(|_| MessagePagerError::InvalidCursor)?,
        );
        if generation != scan_cursor.generation {
            return Err(MessagePagerError::StaleCursor);
        }
        let expected_source = opaque_hash(
            &self.domain_key,
            CURSOR_SOURCE_DOMAIN,
            &[source_key.as_bytes()],
        );
        if !constant_time_equal(&plain[18..], &expected_source[..CURSOR_SOURCE_BIND_BYTES]) {
            return Err(MessagePagerError::InvalidCursor);
        }
        let position = u64::from_be_bytes(
            plain[10..18]
                .try_into()
                .map_err(|_| MessagePagerError::InvalidCursor)?,
        );
        Ok(position)
    }

    fn record_source_work(&mut self, work: &SourceWorkBudget) {
        self.metrics.parser_read_bytes = self
            .metrics
            .parser_read_bytes
            .saturating_add(work.parser_read_bytes);
        self.metrics.message_seek_read_bytes = self
            .metrics
            .message_seek_read_bytes
            .saturating_add(work.message_seek_read_bytes);
        self.metrics.parser_examined_bytes = self
            .metrics
            .parser_examined_bytes
            .saturating_add(work.parser_examined_bytes);
        self.metrics.parser_compacted_bytes = self
            .metrics
            .parser_compacted_bytes
            .saturating_add(work.parser_compacted_bytes);
        self.metrics.peak_parser_buffer_bytes = self
            .metrics
            .peak_parser_buffer_bytes
            .max(work.peak_parser_buffer_bytes);
    }

    fn invalidate_jsonl_cache(&mut self, source_key: &str, scan_cursor: &SessionScanCursor) {
        let keep = self.jsonl_cache.as_ref().is_some_and(|cache| {
            self.kind != SessionSourceKind::Codex
                && cache.source_key == source_key
                && cache.generation == scan_cursor.generation
                && cache.file_identity == scan_cursor.file_identity
        });
        if !keep {
            self.jsonl_cache = None;
        }
        let keep_legacy = self.legacy_cache.as_ref().is_some_and(|cache| {
            self.kind == SessionSourceKind::Gemini
                && cache.source_key == source_key
                && cache.generation == scan_cursor.generation
                && cache.file_identity == scan_cursor.file_identity
        });
        if !keep_legacy {
            self.legacy_cache = None;
        }
    }
}

fn validate_page_limits(request: &MessagePageRequest) -> Result<(), MessagePagerError> {
    if request.maximum_messages == 0
        || request.maximum_messages > MAX_MESSAGE_PAGE_MESSAGES
        || request.maximum_utf8_bytes == 0
        || request.maximum_utf8_bytes > MAX_MESSAGE_PAGE_UTF8_BYTES
    {
        return Err(MessagePagerError::InvalidPageLimit);
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JsonlKind {
    Claude,
    Gemini,
}

struct JsonlRefCache {
    source_key: String,
    generation: u64,
    file_identity: SessionFileIdentity,
    kind: JsonlKind,
    snapshot: SessionFileSnapshot,
    extent: u64,
    structural_hash: Option<[u8; 32]>,
    refs: Vec<JsonlMessageRef>,
}

struct LegacyRefCache {
    source_key: String,
    generation: u64,
    file_identity: SessionFileIdentity,
    snapshot: SessionFileSnapshot,
    extent: u64,
    structural_hash: Option<[u8; 32]>,
    refs: Vec<LegacyMessageRef>,
}

#[derive(Clone, Copy)]
struct LegacyMessageRef {
    byte_start: u64,
    byte_end: u64,
}

#[derive(Clone)]
struct JsonlMessageRef {
    key: [u8; 32],
    byte_start: u64,
    byte_end: u64,
}

#[derive(Default)]
struct SourceWorkBudget {
    parser_read_bytes: u64,
    parser_examined_bytes: u64,
    parser_compacted_bytes: u64,
    message_seek_read_bytes: u64,
    peak_parser_buffer_bytes: usize,
}

impl SourceWorkBudget {
    fn consume_parser(&mut self, bytes: usize) -> Result<(), MessagePagerError> {
        let parser_read_bytes = self
            .parser_read_bytes
            .checked_add(bytes as u64)
            .ok_or(MessagePagerError::ResourceLimit)?;
        self.verify(parser_read_bytes, self.message_seek_read_bytes)?;
        self.parser_read_bytes = parser_read_bytes;
        Ok(())
    }

    fn consume_seek(&mut self, bytes: usize) -> Result<(), MessagePagerError> {
        let message_seek_read_bytes = self
            .message_seek_read_bytes
            .checked_add(bytes as u64)
            .ok_or(MessagePagerError::ResourceLimit)?;
        self.verify(self.parser_read_bytes, message_seek_read_bytes)?;
        self.message_seek_read_bytes = message_seek_read_bytes;
        Ok(())
    }

    fn observe_parser_buffer(&mut self, bytes: usize) {
        self.peak_parser_buffer_bytes = self.peak_parser_buffer_bytes.max(bytes);
    }

    fn examine_parser(&mut self, bytes: usize) {
        self.parser_examined_bytes = self.parser_examined_bytes.saturating_add(bytes as u64);
    }

    fn compact_parser(&mut self, bytes: usize) {
        self.parser_compacted_bytes = self.parser_compacted_bytes.saturating_add(bytes as u64);
    }

    fn verify(
        &self,
        parser_read_bytes: u64,
        message_seek_read_bytes: u64,
    ) -> Result<(), MessagePagerError> {
        if parser_read_bytes > MAX_JSONL_PAGE_SOURCE_WORK_BYTES
            || message_seek_read_bytes > MAX_MESSAGE_PAGE_SEEK_WORK_BYTES
        {
            return Err(MessagePagerError::ResourceLimit);
        }
        Ok(())
    }
}

struct BoundedJsonlRecord {
    ordinal: u64,
    byte_start: u64,
    byte_end: u64,
    value: Option<Value>,
    checkpoint_spans: Option<Vec<(u64, u64)>>,
}

struct BoundedJsonlReader {
    record_start: u64,
    buffer_offset: u64,
    read_offset: u64,
    next_ordinal: u64,
    extent: u64,
    buffer: Vec<u8>,
    buffer_head: usize,
    raw_fingerprint: Option<OpaqueStreamHash>,
}

impl BoundedJsonlReader {
    fn new(offset: u64, next_ordinal: u64, extent: u64) -> Result<Self, MessagePagerError> {
        if offset > extent {
            return Err(MessagePagerError::InvalidCursor);
        }
        Ok(Self {
            record_start: offset,
            buffer_offset: offset,
            read_offset: offset,
            next_ordinal: next_ordinal.max(1),
            extent,
            buffer: Vec::new(),
            buffer_head: 0,
            raw_fingerprint: None,
        })
    }

    fn with_raw_fingerprint(mut self, fingerprint: OpaqueStreamHash) -> Self {
        self.raw_fingerprint = Some(fingerprint);
        self
    }

    fn finish_raw_fingerprint(&mut self, promoted_extent: u64) -> Option<[u8; 32]> {
        self.raw_fingerprint.take()?.finalize(promoted_extent)
    }

    fn next(
        &mut self,
        file: &mut SessionFile,
        kind: JsonlKind,
        work: &mut SourceWorkBudget,
    ) -> Result<Option<BoundedJsonlRecord>, MessagePagerError> {
        loop {
            let unread = &self.buffer[self.buffer_head..];
            if let Some(relative_newline) = unread.iter().position(|byte| *byte == b'\n') {
                work.examine_parser(relative_newline.saturating_add(1));
                let newline = self
                    .buffer_head
                    .checked_add(relative_newline)
                    .ok_or(MessagePagerError::ResourceLimit)?;
                let byte_end = self
                    .buffer_offset
                    .checked_add(newline as u64)
                    .and_then(|offset| offset.checked_add(1))
                    .ok_or(MessagePagerError::ResourceLimit)?;
                if byte_end.saturating_sub(self.record_start) > MAX_JSONL_PAGE_RECORD_BYTES as u64 {
                    return Err(MessagePagerError::ResourceLimit);
                }
                let line = &self.buffer[self.buffer_head..newline];
                let value = serde_json::from_slice(line).ok();
                let checkpoint_spans = match (kind, value.as_ref()) {
                    (JsonlKind::Gemini, Some(value)) => {
                        gemini_checkpoint_element_spans(line, value, self.record_start)?
                    }
                    _ => None,
                };
                self.buffer_head = newline.saturating_add(1);
                let ordinal = self.next_ordinal;
                self.next_ordinal = self
                    .next_ordinal
                    .checked_add(1)
                    .ok_or(MessagePagerError::ResourceLimit)?;
                let byte_start = self.record_start;
                self.record_start = byte_end;
                return Ok(Some(BoundedJsonlRecord {
                    ordinal,
                    byte_start,
                    byte_end,
                    value,
                    checkpoint_spans,
                }));
            }
            work.examine_parser(unread.len());
            if self.read_offset >= self.extent {
                if self.buffer_head == self.buffer.len() {
                    return Ok(None);
                }
                return Err(MessagePagerError::Read);
            }
            let unread_len = self.buffer.len().saturating_sub(self.buffer_head);
            if unread_len > MAX_JSONL_PAGE_RECORD_BYTES {
                return Err(MessagePagerError::ResourceLimit);
            }
            if self.buffer_head != 0
                && (self.buffer_head >= JSONL_PAGE_READ_CHUNK_BYTES
                    || self.buffer_head.saturating_mul(2) >= self.buffer.len())
            {
                let remaining = self.buffer.len().saturating_sub(self.buffer_head);
                self.buffer.copy_within(self.buffer_head.., 0);
                self.buffer.truncate(remaining);
                self.buffer_offset = self
                    .buffer_offset
                    .checked_add(self.buffer_head as u64)
                    .ok_or(MessagePagerError::ResourceLimit)?;
                self.buffer_head = 0;
                work.compact_parser(remaining);
            }
            let unread_len = self.buffer.len().saturating_sub(self.buffer_head);
            let maximum_read = usize::try_from(self.extent - self.read_offset)
                .unwrap_or(usize::MAX)
                .min(JSONL_PAGE_READ_CHUNK_BYTES)
                .min(
                    MAX_JSONL_PAGE_RECORD_BYTES
                        .saturating_add(1)
                        .saturating_sub(unread_len),
                );
            if maximum_read == 0 {
                return Err(MessagePagerError::ResourceLimit);
            }
            let chunk = file
                .read_range_bounded(self.read_offset, maximum_read)
                .map_err(map_source_read_error)?;
            if chunk.is_empty() {
                return Err(MessagePagerError::Read);
            }
            work.consume_parser(chunk.len())?;
            if self
                .raw_fingerprint
                .as_mut()
                .is_some_and(|fingerprint| !fingerprint.update(&chunk))
            {
                return Err(MessagePagerError::ResourceLimit);
            }
            self.read_offset = self
                .read_offset
                .checked_add(chunk.len() as u64)
                .ok_or(MessagePagerError::ResourceLimit)?;
            self.buffer.extend_from_slice(&chunk);
            work.observe_parser_buffer(self.buffer.len().saturating_sub(self.buffer_head));
        }
    }
}

fn build_jsonl_refs(
    file: &mut SessionFile,
    kind: JsonlKind,
    domain_key: &[u8; 32],
    extent: u64,
    work: &mut SourceWorkBudget,
) -> Result<(Vec<JsonlMessageRef>, Option<[u8; 32]>), MessagePagerError> {
    let mut reader = BoundedJsonlReader::new(0, 1, extent)?;
    if kind == JsonlKind::Gemini {
        reader = reader.with_raw_fingerprint(gemini::structural_fingerprint(
            domain_key,
            GeminiFormat::CurrentJsonl,
        ));
    }
    let mut refs = Vec::new();
    let mut positions = HashMap::<[u8; 32], usize>::new();
    while let Some(record) = reader.next(file, kind, work)? {
        let Some(value) = record.value.as_ref() else {
            continue;
        };
        match kind {
            JsonlKind::Claude => {
                if let Some(key) = claude_message_key(value, record.ordinal, domain_key) {
                    insert_ref(
                        JsonlMessageRef {
                            key,
                            byte_start: record.byte_start,
                            byte_end: record.byte_end,
                        },
                        true,
                        &mut refs,
                        &mut positions,
                    )?;
                }
            }
            JsonlKind::Gemini => {
                apply_gemini_jsonl_record(value, &record, domain_key, &mut refs, &mut positions)?
            }
        }
    }
    let structural_hash = if kind == JsonlKind::Gemini {
        Some(
            reader
                .finish_raw_fingerprint(extent)
                .ok_or(MessagePagerError::Read)?,
        )
    } else {
        None
    };
    Ok((refs, structural_hash))
}

fn build_legacy_refs(
    file: &mut SessionFile,
    extent: u64,
    domain_key: &[u8; 32],
    work: &mut SourceWorkBudget,
) -> Result<(Vec<LegacyMessageRef>, [u8; 32]), MessagePagerError> {
    if extent > MAX_LEGACY_PAGE_SOURCE_WORK_BYTES {
        return Err(MessagePagerError::ResourceLimit);
    }
    let mut scanner = LegacyMessageArrayScanner::default();
    let mut fingerprint = gemini::structural_fingerprint(domain_key, GeminiFormat::LegacyJson);
    let mut offset = 0u64;
    while offset < extent {
        let length = usize::try_from(extent - offset)
            .unwrap_or(usize::MAX)
            .min(LEGACY_READ_CHUNK_BYTES);
        let chunk = file
            .read_range_bounded(offset, length)
            .map_err(map_source_read_error)?;
        if chunk.is_empty() {
            return Err(MessagePagerError::Read);
        }
        work.consume_parser(chunk.len())?;
        work.observe_parser_buffer(chunk.len());
        if !fingerprint.update(&chunk) {
            return Err(MessagePagerError::ResourceLimit);
        }
        scanner.feed(&chunk, offset)?;
        offset = offset
            .checked_add(chunk.len() as u64)
            .ok_or(MessagePagerError::ResourceLimit)?;
    }
    let refs = scanner.finish()?;
    let structural_hash = fingerprint
        .finalize(extent)
        .ok_or(MessagePagerError::Read)?;
    Ok((refs, structural_hash))
}

#[derive(Default)]
struct LegacyMessageArrayScanner {
    container_stack: Vec<u8>,
    in_string: bool,
    escaped: bool,
    capture_root_string: bool,
    root_expect_key: bool,
    root_string: Vec<u8>,
    possible_messages_key: bool,
    expect_messages_array: bool,
    in_messages: bool,
    element_start: Option<u64>,
    element_containers: Vec<u8>,
    element_last_non_whitespace: u64,
    refs: Vec<LegacyMessageRef>,
    finished: bool,
}

impl LegacyMessageArrayScanner {
    fn feed(&mut self, bytes: &[u8], base_offset: u64) -> Result<(), MessagePagerError> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.finished {
                if !byte.is_ascii_whitespace() {
                    return Err(MessagePagerError::Read);
                }
                continue;
            }
            let offset = base_offset
                .checked_add(index as u64)
                .ok_or(MessagePagerError::ResourceLimit)?;
            if self.in_messages {
                self.feed_message_byte(byte, offset)?;
            } else {
                self.feed_document_byte(byte)?;
            }
        }
        Ok(())
    }

    fn feed_document_byte(&mut self, byte: u8) -> Result<(), MessagePagerError> {
        if self.in_string {
            if self.escaped {
                self.escaped = false;
                if self.capture_root_string {
                    if self.root_string.len() >= 384 {
                        return Err(MessagePagerError::ResourceLimit);
                    }
                    self.root_string.push(byte);
                }
                return Ok(());
            }
            match byte {
                b'\\' => {
                    self.escaped = true;
                    if self.capture_root_string {
                        if self.root_string.len() >= 384 {
                            return Err(MessagePagerError::ResourceLimit);
                        }
                        self.root_string.push(byte);
                    }
                }
                b'"' => {
                    self.in_string = false;
                    if self.capture_root_string {
                        let mut quoted =
                            Vec::with_capacity(self.root_string.len().saturating_add(2));
                        quoted.push(b'"');
                        quoted.extend_from_slice(&self.root_string);
                        quoted.push(b'"');
                        self.possible_messages_key = serde_json::from_slice::<String>(&quoted)
                            .is_ok_and(|key| key == "messages");
                    }
                    self.capture_root_string = false;
                }
                _ if self.capture_root_string && self.root_string.len() < 384 => {
                    self.root_string.push(byte);
                }
                _ if self.capture_root_string => {
                    return Err(MessagePagerError::ResourceLimit);
                }
                _ => {}
            }
            return Ok(());
        }
        if byte.is_ascii_whitespace() {
            return Ok(());
        }
        if self.expect_messages_array {
            self.expect_messages_array = false;
            if byte == b'[' {
                self.refs.clear();
                self.in_messages = true;
                return Ok(());
            }
            return Err(MessagePagerError::Read);
        }
        if self.possible_messages_key {
            self.possible_messages_key = false;
            if byte == b':' {
                self.expect_messages_array = true;
                return Ok(());
            }
        }
        match byte {
            b'"' => {
                self.in_string = true;
                self.capture_root_string = self.container_stack.len() == 1
                    && self.container_stack.last() == Some(&b'{')
                    && self.root_expect_key;
                if self.capture_root_string {
                    self.root_expect_key = false;
                }
                self.root_string.clear();
                self.possible_messages_key = false;
            }
            b'{' | b'[' => {
                if self.container_stack.len() >= 128 {
                    return Err(MessagePagerError::ResourceLimit);
                }
                self.container_stack.push(byte);
                if self.container_stack.len() == 1 && byte == b'{' {
                    self.root_expect_key = true;
                }
            }
            b'}' if self.container_stack.pop() != Some(b'{') => {
                return Err(MessagePagerError::Read);
            }
            b']' if self.container_stack.pop() != Some(b'[') => {
                return Err(MessagePagerError::Read);
            }
            b',' if self.container_stack.len() == 1
                && self.container_stack.last() == Some(&b'{') =>
            {
                self.root_expect_key = true;
            }
            _ => {}
        }
        if byte == b'}' && self.container_stack.is_empty() {
            self.finished = true;
        }
        Ok(())
    }

    fn feed_message_byte(&mut self, byte: u8, offset: u64) -> Result<(), MessagePagerError> {
        if self.in_string {
            self.element_last_non_whitespace = offset.saturating_add(1);
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
            return Ok(());
        }
        if self.element_start.is_none() {
            if byte.is_ascii_whitespace() || byte == b',' {
                return Ok(());
            }
            if byte == b']' {
                self.in_messages = false;
                return Ok(());
            }
            self.element_start = Some(offset);
            self.element_last_non_whitespace = offset.saturating_add(1);
            match byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => self.element_containers.push(byte),
                _ => {}
            }
            return Ok(());
        }
        if self.element_containers.is_empty() && matches!(byte, b',' | b']') {
            self.push_element()?;
            if byte == b']' {
                self.in_messages = false;
            }
            return Ok(());
        }
        if !byte.is_ascii_whitespace() {
            self.element_last_non_whitespace = offset.saturating_add(1);
        }
        match byte {
            b'"' => self.in_string = true,
            b'{' | b'[' => {
                if self.element_containers.len() >= 128 {
                    return Err(MessagePagerError::ResourceLimit);
                }
                self.element_containers.push(byte);
            }
            b'}' => {
                if self.element_containers.pop() != Some(b'{') {
                    return Err(MessagePagerError::Read);
                }
            }
            b']' if self.element_containers.pop() != Some(b'[') => {
                return Err(MessagePagerError::Read);
            }
            _ => {}
        }
        Ok(())
    }

    fn push_element(&mut self) -> Result<(), MessagePagerError> {
        let byte_start = self.element_start.take().ok_or(MessagePagerError::Read)?;
        if self.refs.len() >= MAX_ACTIVE_PAGE_REFS
            || self
                .refs
                .capacity()
                .saturating_mul(std::mem::size_of::<LegacyMessageRef>())
                > MAX_MESSAGE_REF_WORKING_BYTES
        {
            return Err(MessagePagerError::ResourceLimit);
        }
        self.refs.push(LegacyMessageRef {
            byte_start,
            byte_end: self.element_last_non_whitespace,
        });
        self.element_containers.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<LegacyMessageRef>, MessagePagerError> {
        if self.in_messages
            || !self.finished
            || self.in_string
            || self.escaped
            || !self.container_stack.is_empty()
            || self.expect_messages_array
        {
            return Err(MessagePagerError::Read);
        }
        self.refs.shrink_to_fit();
        if self
            .refs
            .capacity()
            .saturating_mul(std::mem::size_of::<LegacyMessageRef>())
            > MAX_MESSAGE_REF_WORKING_BYTES
        {
            return Err(MessagePagerError::ResourceLimit);
        }
        Ok(self.refs)
    }
}

fn read_legacy_message(
    file: &mut SessionFile,
    reference: LegacyMessageRef,
    work: &mut SourceWorkBudget,
) -> Result<Option<Message>, MessagePagerError> {
    let length = usize::try_from(reference.byte_end.saturating_sub(reference.byte_start))
        .map_err(|_| MessagePagerError::ResourceLimit)?;
    if length > MAX_JSONL_PAGE_RECORD_BYTES {
        return Err(MessagePagerError::ResourceLimit);
    }
    work.consume_seek(length)?;
    let bytes = file
        .read_range_bounded(reference.byte_start, length)
        .map_err(map_source_read_error)?;
    if bytes.len() != length {
        return Err(MessagePagerError::Read);
    }
    let resource_limit = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let result = LegacyPageMessageSeed {
        resource_limit: &resource_limit,
    }
    .deserialize(&mut deserializer)
    .and_then(|message| deserializer.end().map(|()| message));
    result.map_err(|_| {
        if resource_limit.get() {
            MessagePagerError::ResourceLimit
        } else {
            MessagePagerError::Read
        }
    })
}

fn insert_ref(
    reference: JsonlMessageRef,
    replace: bool,
    refs: &mut Vec<JsonlMessageRef>,
    positions: &mut HashMap<[u8; 32], usize>,
) -> Result<(), MessagePagerError> {
    if replace && let Some(&position) = positions.get(&reference.key) {
        refs[position] = reference;
        return Ok(());
    }
    if refs.len() >= MAX_ACTIVE_PAGE_REFS {
        return Err(MessagePagerError::ResourceLimit);
    }
    positions.insert(reference.key, refs.len());
    refs.push(reference);
    let working_bytes = refs
        .capacity()
        .saturating_mul(std::mem::size_of::<JsonlMessageRef>())
        .saturating_add(
            positions
                .capacity()
                .saturating_mul(std::mem::size_of::<([u8; 32], usize)>().saturating_add(16)),
        );
    if working_bytes > MAX_MESSAGE_REF_WORKING_BYTES {
        return Err(MessagePagerError::ResourceLimit);
    }
    Ok(())
}

#[derive(Default)]
struct RawCheckpointObject<'a> {
    set: Option<&'a RawValue>,
    messages: Option<&'a RawValue>,
}

impl<'de> Deserialize<'de> for RawCheckpointObject<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawCheckpointObjectVisitor)
    }
}

struct RawCheckpointObjectVisitor;

impl<'de> Visitor<'de> for RawCheckpointObjectVisitor {
    type Value = RawCheckpointObject<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini checkpoint object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut output = RawCheckpointObject::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "$set" => output.set = Some(map.next_value()?),
                "messages" => output.messages = Some(map.next_value()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(output)
    }
}

struct CheckpointElementsSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for CheckpointElementsSeed<'_> {
    type Value = Vec<&'de RawValue>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(CheckpointElementsVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct CheckpointElementsVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for CheckpointElementsVisitor<'_> {
    type Value = Vec<&'de RawValue>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini checkpoint message array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut output = Vec::new();
        while let Some(element) = sequence.next_element::<&RawValue>()? {
            if output.len() >= MAX_ACTIVE_PAGE_REFS {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "Gemini checkpoint message count exceeds its bound",
                ));
            }
            output.push(element);
        }
        Ok(output)
    }
}

fn gemini_checkpoint_element_spans(
    line: &[u8],
    value: &Value,
    absolute_start: u64,
) -> Result<Option<Vec<(u64, u64)>>, MessagePagerError> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    enum Selection {
        Set(usize),
        Metadata(usize),
    }
    let selection = if let Some(set) = object.get("$set").and_then(Value::as_object) {
        let Some(messages) = set.get("messages") else {
            return Ok(None);
        };
        Selection::Set(messages.as_array().ok_or(MessagePagerError::Read)?.len())
    } else if object.contains_key("sessionId") && !object.contains_key("id") {
        let Some(messages) = object.get("messages") else {
            return Ok(None);
        };
        Selection::Metadata(messages.as_array().ok_or(MessagePagerError::Read)?.len())
    } else {
        return Ok(None);
    };

    let root: RawCheckpointObject<'_> =
        serde_json::from_slice(line).map_err(|_| MessagePagerError::Read)?;
    let (messages, expected_len) = match selection {
        Selection::Set(expected_len) => {
            let set = root.set.ok_or(MessagePagerError::Read)?;
            let set: RawCheckpointObject<'_> =
                serde_json::from_str(set.get()).map_err(|_| MessagePagerError::Read)?;
            (set.messages.ok_or(MessagePagerError::Read)?, expected_len)
        }
        Selection::Metadata(expected_len) => {
            (root.messages.ok_or(MessagePagerError::Read)?, expected_len)
        }
    };
    let resource_limit = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_str(messages.get());
    let elements = CheckpointElementsSeed {
        resource_limit: &resource_limit,
    }
    .deserialize(&mut deserializer)
    .and_then(|elements| deserializer.end().map(|()| elements))
    .map_err(|_| {
        if resource_limit.get() {
            MessagePagerError::ResourceLimit
        } else {
            MessagePagerError::Read
        }
    })?;
    if elements.len() != expected_len {
        return Err(MessagePagerError::Read);
    }

    let line_start = line.as_ptr() as usize;
    let line_end = line_start
        .checked_add(line.len())
        .ok_or(MessagePagerError::ResourceLimit)?;
    elements
        .into_iter()
        .map(|element| {
            let element_start = element.get().as_ptr() as usize;
            let element_end = element_start
                .checked_add(element.get().len())
                .ok_or(MessagePagerError::ResourceLimit)?;
            if element_start < line_start || element_end > line_end {
                return Err(MessagePagerError::Read);
            }
            let relative_start = u64::try_from(element_start - line_start)
                .map_err(|_| MessagePagerError::ResourceLimit)?;
            let relative_end = u64::try_from(element_end - line_start)
                .map_err(|_| MessagePagerError::ResourceLimit)?;
            Ok((
                absolute_start
                    .checked_add(relative_start)
                    .ok_or(MessagePagerError::ResourceLimit)?,
                absolute_start
                    .checked_add(relative_end)
                    .ok_or(MessagePagerError::ResourceLimit)?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn apply_gemini_jsonl_record(
    value: &Value,
    record: &BoundedJsonlRecord,
    domain_key: &[u8; 32],
    refs: &mut Vec<JsonlMessageRef>,
    positions: &mut HashMap<[u8; 32], usize>,
) -> Result<(), MessagePagerError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if let Some(rewind) = object.get("$rewindTo").and_then(Value::as_str) {
        let key = opaque_hash(domain_key, LOGICAL_REF_DOMAIN, &[rewind.as_bytes()]);
        let keep = positions.get(&key).copied().unwrap_or(0);
        for reference in refs.drain(keep..) {
            positions.remove(&reference.key);
        }
        return Ok(());
    }
    let checkpoint = if let Some(set) = object.get("$set").and_then(Value::as_object) {
        set.get("messages")
            .map(|messages| messages.as_array().ok_or(MessagePagerError::Read))
            .transpose()?
    } else if object.contains_key("sessionId") && !object.contains_key("id") {
        object
            .get("messages")
            .map(|messages| messages.as_array().ok_or(MessagePagerError::Read))
            .transpose()?
    } else {
        None
    };
    if let Some(checkpoint) = checkpoint {
        let checkpoint_spans = record
            .checkpoint_spans
            .as_deref()
            .ok_or(MessagePagerError::Read)?;
        if checkpoint_spans.len() != checkpoint.len() {
            return Err(MessagePagerError::Read);
        }
        refs.clear();
        positions.clear();
        for (index, (message, &(byte_start, byte_end))) in
            checkpoint.iter().zip(checkpoint_spans).enumerate()
        {
            let Some(key) = gemini_message_key(message, record.ordinal + index as u64, domain_key)
            else {
                continue;
            };
            insert_ref(
                JsonlMessageRef {
                    key,
                    byte_start,
                    byte_end,
                },
                true,
                refs,
                positions,
            )?;
        }
        return Ok(());
    }
    let Some(key) = gemini_message_key(value, record.ordinal, domain_key) else {
        return Ok(());
    };
    insert_ref(
        JsonlMessageRef {
            key,
            byte_start: record.byte_start,
            byte_end: record.byte_end,
        },
        true,
        refs,
        positions,
    )
}

fn claude_message_key(value: &Value, ordinal: u64, domain_key: &[u8; 32]) -> Option<[u8; 32]> {
    if !claude::is_visible_message_record(value) {
        return None;
    }
    let object = value.as_object()?;
    let message = object.get("message")?.as_object()?;
    let external = message
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| object.get("uuid").and_then(Value::as_str));
    Some(match external {
        Some(external) => opaque_hash(domain_key, LOGICAL_REF_DOMAIN, &[external.as_bytes()]),
        None => opaque_hash(domain_key, LOGICAL_REF_DOMAIN, &[&ordinal.to_be_bytes()]),
    })
}

fn gemini_message_key(value: &Value, _ordinal: u64, domain_key: &[u8; 32]) -> Option<[u8; 32]> {
    let object = value.as_object()?;
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("user" | "gemini" | "info" | "error" | "warning")
    ) {
        return None;
    }
    let id = object.get("id").and_then(Value::as_str)?;
    Some(opaque_hash(
        domain_key,
        LOGICAL_REF_DOMAIN,
        &[id.as_bytes()],
    ))
}

fn read_jsonl_message(
    file: &mut SessionFile,
    reference: &JsonlMessageRef,
    kind: JsonlKind,
    work: &mut SourceWorkBudget,
) -> Result<Option<Message>, MessagePagerError> {
    let length = reference
        .byte_end
        .checked_sub(reference.byte_start)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(MessagePagerError::ResourceLimit)?;
    if length > MAX_JSONL_PAGE_RECORD_BYTES {
        return Err(MessagePagerError::ResourceLimit);
    }
    work.consume_seek(length)?;
    let bytes = file
        .read_range_bounded(reference.byte_start, length)
        .map_err(map_source_read_error)?;
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let value: Value = serde_json::from_slice(line).map_err(|_| MessagePagerError::Read)?;
    Ok(match kind {
        JsonlKind::Claude => parse_claude_message(&value),
        JsonlKind::Gemini => parse_gemini_message(&value),
    })
}

fn map_source_read_error(error: SessionError) -> MessagePagerError {
    match error {
        SessionError::SessionFileChanged | SessionError::SessionFileUnavailable => {
            MessagePagerError::StaleCursor
        }
        SessionError::ReadLimitExceeded => MessagePagerError::ResourceLimit,
        SessionError::MissingPlatformData { .. }
        | SessionError::UnsafePath
        | SessionError::EnumerationLimitExceeded
        | SessionError::Io { .. } => MessagePagerError::Read,
    }
}

fn parse_codex_message(value: &Value) -> Option<Message> {
    let object = value.as_object()?;
    let payload = object.get("payload")?.as_object()?;
    let item_type = payload.get("type")?.as_str()?;
    let (role, content, tools) = match item_type {
        "message" => {
            let role = parse_role(payload.get("role").and_then(Value::as_str))?;
            let content = normalized_content(payload.get("content")?)?;
            (role, content, normalized_tools(payload))
        }
        "user_message" => {
            let content =
                normalized_content(payload.get("message").or_else(|| payload.get("content"))?)?;
            (MessageRole::User, content, normalized_tools(payload))
        }
        "agent_message" => {
            let content =
                normalized_content(payload.get("message").or_else(|| payload.get("content"))?)?;
            (MessageRole::Assistant, content, normalized_tools(payload))
        }
        "function_call" | "custom_tool_call" => (
            MessageRole::Tool,
            String::new(),
            vec![bounded_tool(
                MessageToolType::Call,
                payload.get("name").and_then(Value::as_str),
            )],
        ),
        "function_call_output" | "custom_tool_call_output" => (
            MessageRole::Tool,
            String::new(),
            vec![bounded_tool(
                MessageToolType::Result,
                payload.get("name").and_then(Value::as_str),
            )],
        ),
        _ => return None,
    };
    let timestamp = object
        .get("timestamp")
        .or_else(|| payload.get("timestamp"))
        .and_then(normalize_timestamp)?;
    Some(Message {
        role,
        content,
        timestamp,
        tools,
    })
}

fn parse_claude_message(value: &Value) -> Option<Message> {
    if !claude::is_visible_message_record(value) {
        return None;
    }
    let object = value.as_object()?;
    let message = object.get("message")?.as_object()?;
    let role = parse_role(message.get("role").and_then(Value::as_str))?;
    let timestamp = object
        .get("timestamp")
        .or_else(|| message.get("timestamp"))
        .and_then(normalize_timestamp)?;
    let tools = normalized_tools(message);
    let content = message
        .get("content")
        .and_then(normalized_content)
        .unwrap_or_default();
    if content.is_empty() && tools.is_empty() {
        return None;
    }
    Some(Message {
        role,
        content,
        timestamp,
        tools,
    })
}

fn parse_gemini_message(value: &Value) -> Option<Message> {
    let object = value.as_object()?;
    let role = match object.get("type").and_then(Value::as_str)? {
        "user" => MessageRole::User,
        "gemini" => MessageRole::Assistant,
        "info" | "warning" | "error" => MessageRole::System,
        _ => return None,
    };
    let timestamp = object.get("timestamp").and_then(normalize_timestamp)?;
    let tools = normalized_tools(object);
    let content = object
        .get("content")
        .and_then(normalized_gemini_content)
        .unwrap_or_default();
    if content.is_empty() && tools.is_empty() {
        return None;
    }
    Some(Message {
        role,
        content,
        timestamp,
        tools,
    })
}

fn parse_role(value: Option<&str>) -> Option<MessageRole> {
    match value? {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "system" | "developer" => Some(MessageRole::System),
        "tool" => Some(MessageRole::Tool),
        _ => None,
    }
}

fn normalized_content(value: &Value) -> Option<String> {
    let mut output = String::new();
    match value {
        Value::String(text) => append_text(&mut output, text)?,
        Value::Array(parts) => {
            for part in parts {
                let Some(object) = part.as_object() else {
                    continue;
                };
                if !matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                ) {
                    continue;
                }
                let Some(text) = object.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !output.is_empty() {
                    append_text(&mut output, "\n")?;
                }
                append_text(&mut output, text)?;
            }
        }
        _ => return None,
    }
    (!output.is_empty()).then_some(output)
}

fn normalized_gemini_content(value: &Value) -> Option<String> {
    let mut output = String::new();
    match value {
        Value::String(text) => append_text(&mut output, text)?,
        Value::Array(parts) => {
            for part in parts {
                let Some(object) = part.as_object() else {
                    continue;
                };
                let is_text = object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| matches!(kind, "text" | "input_text" | "output_text"));
                if !is_text {
                    continue;
                }
                let Some(text) = object.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !output.is_empty() {
                    append_text(&mut output, "\n")?;
                }
                append_text(&mut output, text)?;
            }
        }
        _ => return None,
    }
    (!output.is_empty()).then_some(output)
}

fn append_text(output: &mut String, text: &str) -> Option<()> {
    if text.chars().any(|character| character == '\0')
        || output.len().saturating_add(text.len()) > MAX_MESSAGE_ITEM_UTF8_BYTES
    {
        return None;
    }
    output.push_str(text);
    Some(())
}

fn normalized_tools(object: &serde_json::Map<String, Value>) -> Vec<MessageTool> {
    let mut output = Vec::new();
    if let Some(parts) = object.get("content").and_then(Value::as_array) {
        for part in parts {
            let Some(part) = part.as_object() else {
                continue;
            };
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use" | "function_call") => push_tool(
                    &mut output,
                    MessageToolType::Call,
                    part.get("name").and_then(Value::as_str),
                ),
                Some("tool_result" | "function_call_output") => push_tool(
                    &mut output,
                    MessageToolType::Result,
                    part.get("name").and_then(Value::as_str),
                ),
                _ => {
                    if let Some(call) = part.get("functionCall").and_then(Value::as_object) {
                        push_tool(
                            &mut output,
                            MessageToolType::Call,
                            call.get("name").and_then(Value::as_str),
                        );
                    } else if let Some(result) =
                        part.get("functionResponse").and_then(Value::as_object)
                    {
                        push_tool(
                            &mut output,
                            MessageToolType::Result,
                            result.get("name").and_then(Value::as_str),
                        );
                    }
                }
            }
        }
    }
    if let Some(tools) = object.get("toolCalls").and_then(Value::as_array) {
        for tool in tools {
            let Some(tool) = tool.as_object() else {
                continue;
            };
            push_tool(
                &mut output,
                MessageToolType::Call,
                tool.get("name").and_then(Value::as_str),
            );
        }
    }
    output
}

fn push_tool(output: &mut Vec<MessageTool>, tool_type: MessageToolType, name: Option<&str>) {
    if output.len() >= MAX_MESSAGE_TOOLS {
        return;
    }
    output.push(bounded_tool(tool_type, name));
}

fn bounded_tool(tool_type: MessageToolType, name: Option<&str>) -> MessageTool {
    let name = name
        .filter(|name| {
            !name.is_empty()
                && name.len() <= MAX_TOOL_NAME_UTF8_BYTES
                && !name.chars().any(char::is_control)
        })
        .map(ToOwned::to_owned);
    MessageTool { tool_type, name }
}

fn message_utf8_bytes(message: &Message) -> usize {
    message.content.len()
        + message.timestamp.len()
        + message
            .tools
            .iter()
            .map(|tool| tool.name.as_ref().map_or(0, String::len))
            .sum::<usize>()
}

fn kind_byte(kind: SessionSourceKind) -> u8 {
    match kind {
        SessionSourceKind::Codex => 1,
        SessionSourceKind::Claude => 2,
        SessionSourceKind::Gemini => 3,
    }
}

fn hmac_sha256(key: &[u8; 32], domain: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut inner_key = [0x36u8; BLOCK_BYTES];
    let mut outer_key = [0x5cu8; BLOCK_BYTES];
    for index in 0..key.len() {
        inner_key[index] ^= key[index];
        outer_key[index] ^= key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update((domain.len() as u64).to_be_bytes());
    inner.update(domain);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner);
    outer.finalize().into()
}

fn cursor_mask(key: &[u8; 32], nonce: &[u8]) -> [u8; CURSOR_PAYLOAD_BYTES] {
    let mut first_input = Vec::with_capacity(nonce.len() + 1);
    first_input.extend_from_slice(nonce);
    first_input.push(0);
    let mut second_input = Vec::with_capacity(nonce.len() + 1);
    second_input.extend_from_slice(nonce);
    second_input.push(1);
    let first = hmac_sha256(key, CURSOR_MASK_DOMAIN, &first_input);
    let second = hmac_sha256(key, CURSOR_MASK_DOMAIN, &second_input);
    let mut output = [0u8; CURSOR_PAYLOAD_BYTES];
    output[..first.len()].copy_from_slice(&first);
    output[first.len()..].copy_from_slice(&second[..CURSOR_PAYLOAD_BYTES - first.len()]);
    output
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        output.push((decode_nibble(pair[0])? << 4) | decode_nibble(pair[1])?);
    }
    Some(output)
}

fn decode_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

struct PageBoundedStringSeed<'a, const MAXIMUM: usize> {
    resource_limit: &'a Cell<bool>,
}

struct PageBoundedStringVisitor<'a, const MAXIMUM: usize> {
    resource_limit: &'a Cell<bool>,
}

impl<'de, const MAXIMUM: usize> DeserializeSeed<'de> for PageBoundedStringSeed<'_, MAXIMUM> {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(PageBoundedStringVisitor::<MAXIMUM> {
            resource_limit: self.resource_limit,
        })
    }
}

impl<const MAXIMUM: usize> Visitor<'_> for PageBoundedStringVisitor<'_, MAXIMUM> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a UTF-8 string no longer than {MAXIMUM} bytes")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.accept(value)
    }

    fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.accept(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAXIMUM {
            self.resource_limit.set(true);
            Err(E::custom("legacy message string exceeds its field bound"))
        } else {
            Ok(value)
        }
    }
}

impl<const MAXIMUM: usize> PageBoundedStringVisitor<'_, MAXIMUM> {
    fn accept<E>(self, value: &str) -> Result<String, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAXIMUM {
            self.resource_limit.set(true);
            Err(E::custom("legacy message string exceeds its field bound"))
        } else {
            Ok(value.to_owned())
        }
    }
}

struct LegacyPageMessageSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyPageMessageSeed<'_> {
    type Value = Option<Message>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyPageMessageVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyPageMessageVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyPageMessageVisitor<'_> {
    type Value = Option<Message>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini legacy page message")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut message_type = None;
        let mut timestamp = None;
        let mut content = None;
        let mut tools = Vec::new();
        while let Some(key) = map.next_key_seed(PageBoundedStringSeed::<64> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "type" => {
                    message_type = Some(map.next_value_seed(PageBoundedStringSeed::<16> {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "timestamp" => {
                    timestamp = Some(map.next_value_seed(PageBoundedStringSeed::<64> {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "content" => {
                    content = map.next_value_seed(LegacyPageContentSeed {
                        resource_limit: self.resource_limit,
                    })?
                }
                "toolCalls" => {
                    tools = map.next_value_seed(LegacyPageToolsSeed {
                        resource_limit: self.resource_limit,
                    })?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let role = match message_type.as_deref() {
            Some("user") => MessageRole::User,
            Some("gemini") => MessageRole::Assistant,
            Some("info" | "warning" | "error") => MessageRole::System,
            _ => return Ok(None),
        };
        let Some(timestamp) =
            timestamp.and_then(|value| normalize_timestamp(&Value::String(value)))
        else {
            return Ok(None);
        };
        if content.is_none() && tools.is_empty() {
            return Ok(None);
        }
        Ok(Some(Message {
            role,
            content: content.unwrap_or_default(),
            timestamp,
            tools,
        }))
    }
}

struct LegacyPageContentSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyPageContentSeed<'_> {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(LegacyPageContentVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyPageContentVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyPageContentVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded Gemini legacy text content")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok((value.len() <= MAX_MESSAGE_ITEM_UTF8_BYTES).then(|| value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok((value.len() <= MAX_MESSAGE_ITEM_UTF8_BYTES).then_some(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut output = String::new();
        let mut count = 0usize;
        while let Some(text) = sequence.next_element_seed(LegacyPageContentPartSeed {
            resource_limit: self.resource_limit,
        })? {
            if count >= 128 {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "legacy message content array exceeds its bound",
                ));
            }
            if let Some(text) = text {
                if !output.is_empty() {
                    if output.len() == MAX_MESSAGE_ITEM_UTF8_BYTES {
                        return Ok(None);
                    }
                    output.push('\n');
                }
                if output.len().saturating_add(text.len()) > MAX_MESSAGE_ITEM_UTF8_BYTES {
                    return Ok(None);
                }
                output.push_str(&text);
            }
            count += 1;
        }
        Ok((!output.is_empty()).then_some(output))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

struct LegacyPageContentPartSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyPageContentPartSeed<'_> {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyPageContentPartVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyPageContentPartVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyPageContentPartVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini legacy text content part")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut text = None;
        while let Some(key) = map.next_key_seed(PageBoundedStringSeed::<64> {
            resource_limit: self.resource_limit,
        })? {
            if key == "text" {
                text = Some(map.next_value_seed(PageBoundedStringSeed::<
                    MAX_MESSAGE_ITEM_UTF8_BYTES,
                > {
                    resource_limit: self.resource_limit,
                })?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(text)
    }
}

struct LegacyPageToolsSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyPageToolsSeed<'_> {
    type Value = Vec<MessageTool>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(LegacyPageToolsVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyPageToolsVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyPageToolsVisitor<'_> {
    type Value = Vec<MessageTool>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini legacy tool array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut output = Vec::new();
        loop {
            let present = if output.len() < MAX_MESSAGE_TOOLS {
                let tool = sequence.next_element_seed(LegacyPageToolSeed {
                    resource_limit: self.resource_limit,
                })?;
                let Some(tool) = tool else {
                    break;
                };
                output.push(tool);
                true
            } else {
                sequence.next_element::<IgnoredAny>()?.is_some()
            };
            if !present {
                break;
            }
        }
        Ok(output)
    }
}

struct LegacyPageToolSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyPageToolSeed<'_> {
    type Value = MessageTool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyPageToolVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyPageToolVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyPageToolVisitor<'_> {
    type Value = MessageTool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini legacy tool object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut name = None;
        while let Some(key) = map.next_key_seed(PageBoundedStringSeed::<64> {
            resource_limit: self.resource_limit,
        })? {
            if key == "name" {
                name = Some(map.next_value_seed(PageBoundedStringSeed::<
                    MAX_TOOL_NAME_UTF8_BYTES,
                > {
                    resource_limit: self.resource_limit,
                })?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(MessageTool {
            tool_type: MessageToolType::Call,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LegacyMessageArrayScanner, MAX_JSONL_PAGE_SOURCE_WORK_BYTES,
        MAX_MESSAGE_PAGE_SEEK_WORK_BYTES, MessagePagerError, SourceWorkBudget,
        gemini_checkpoint_element_spans,
    };

    fn legacy_refs(bytes: &[u8]) -> Result<Vec<&[u8]>, MessagePagerError> {
        let mut scanner = LegacyMessageArrayScanner::default();
        for (index, chunk) in bytes.chunks(7).enumerate() {
            scanner.feed(chunk, (index * 7) as u64)?;
        }
        scanner.finish().map(|refs| {
            refs.into_iter()
                .map(|reference| &bytes[reference.byte_start as usize..reference.byte_end as usize])
                .collect()
        })
    }

    #[test]
    fn legacy_index_decodes_root_keys_and_uses_the_last_duplicate_value() {
        let document = br#"{"messages":[{"content":"old"}],"m\u0065ssages":[{"content":"new"}]}"#;
        let refs = legacy_refs(document).unwrap();
        assert_eq!(refs, vec![br#"{"content":"new"}"#.as_slice()]);
    }

    #[test]
    fn legacy_index_rejects_non_array_last_value_and_invalid_structure() {
        assert!(legacy_refs(br#"{"messages":[],"messages":null}"#).is_err());
        assert!(legacy_refs(br#"{"messages":[{"content":"broken"]}"#).is_err());
        assert!(legacy_refs(br#"{"messages":[]}"#.strip_suffix(b"}").unwrap()).is_err());
    }

    #[test]
    fn source_and_seek_work_budgets_accept_exact_limits_and_reject_one_over() {
        let mut exact = SourceWorkBudget::default();
        exact
            .consume_parser(MAX_JSONL_PAGE_SOURCE_WORK_BYTES as usize)
            .unwrap();
        exact
            .consume_seek(MAX_MESSAGE_PAGE_SEEK_WORK_BYTES as usize)
            .unwrap();

        let mut parser_over = SourceWorkBudget::default();
        assert!(matches!(
            parser_over.consume_parser((MAX_JSONL_PAGE_SOURCE_WORK_BYTES + 1) as usize),
            Err(MessagePagerError::ResourceLimit)
        ));

        let mut seek_over = SourceWorkBudget::default();
        assert!(matches!(
            seek_over.consume_seek((MAX_MESSAGE_PAGE_SEEK_WORK_BYTES + 1) as usize),
            Err(MessagePagerError::ResourceLimit)
        ));
    }

    #[test]
    fn current_checkpoint_span_extractor_rejects_non_array_last_duplicate() {
        let line = br#"{"$set":{"messages":[{"id":"valid"}],"m\u0065ssages":null}}"#;
        let value = serde_json::from_slice(line).unwrap();
        assert!(matches!(
            gemini_checkpoint_element_spans(line, &value, 0),
            Err(MessagePagerError::Read)
        ));
    }

    #[test]
    fn current_checkpoint_span_extractor_preserves_set_branch_priority() {
        let line = br#"{"sessionId":"metadata","messages":[{"id":"must-not-fallback"}],"$set":{"summary":"only"}}"#;
        let value = serde_json::from_slice(line).unwrap();
        assert!(
            gemini_checkpoint_element_spans(line, &value, 0)
                .unwrap()
                .is_none()
        );
    }
}
