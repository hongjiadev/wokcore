use std::path::PathBuf;

use axum::{
    body::Body,
    extract::{
        Extension, Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::{StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wokcore_diagnostics::event::UtcTimestamp;
use wokcore_sessions::messages::{
    MAX_MESSAGE_PAGE_UTF8_BYTES, Message, MessagePageCursor, MessagePageRequest, MessagePager,
    MessagePagerError, MessageRole,
};
use wokcore_storage::{
    GlobalSessionIndexPageKey, SessionAvailability, SessionIndexRecord, SessionSourceErrorCode,
    SessionSourceKind, SessionUsageAggregateBucket, SessionUsageAggregateFilter,
    SessionUsageAggregatePageKey, SessionUsageAggregateTotals, SessionUsageGroupBy, StateStore,
    StorageError,
};

use crate::{
    ServerState,
    observability::{IndexPhase, IndexStatus, SessionKind},
    query::QueryServiceError,
};

use super::{cursor, error::ApiError, request_id::RequestId};

const SESSION_LIST_DEFAULT_LIMIT: usize = 50;
const SESSION_LIST_MAX_LIMIT: usize = 200;
const SESSION_LIST_MAX_RESPONSE_BYTES: usize = 512 * 1024;
const SESSION_LIST_ROUTE: &str = "sessions.list";
const SESSION_MESSAGES_DEFAULT_LIMIT: usize = 100;
const SESSION_MESSAGES_MAX_LIMIT: usize = 500;
const SESSION_MESSAGES_DEFAULT_RESPONSE_BYTES: usize = 256 * 1024;
const SESSION_MESSAGES_MIN_RESPONSE_BYTES: usize = 4 * 1024;
const SESSION_MESSAGES_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const SESSION_MESSAGES_ROUTE: &str = "sessions.messages";
const USAGE_DEFAULT_LIMIT: usize = 100;
const USAGE_MAX_LIMIT: usize = 500;
const USAGE_MAX_RESPONSE_BYTES: usize = 512 * 1024;
const USAGE_ROUTE: &str = "usage.aggregate";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionListQuery {
    source: Option<String>,
    availability: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn list_sessions(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    query: Result<Query<SessionListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    let source =
        parse_source(query.source.as_deref()).ok_or_else(|| ApiError::invalid_query(request_id))?;
    let availability = parse_availability(query.availability.as_deref())
        .ok_or_else(|| ApiError::invalid_query(request_id))?;
    let limit = query.limit.unwrap_or(SESSION_LIST_DEFAULT_LIMIT);
    if !(1..=SESSION_LIST_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::limit_out_of_range(request_id));
    }
    let runtime = state
        .query
        .clone()
        .ok_or_else(|| ApiError::query_busy(request_id))?;
    let state_path = runtime.state_path().to_path_buf();
    let domain_key = *runtime.session_domain_key();
    let index_status = index_status_response(
        state
            .scheduler
            .as_ref()
            .map_or_else(IndexStatus::default, |scheduler| scheduler.status()),
    );
    let binding = session_list_binding(source, availability);
    let before = query.before;
    let pending = runtime
        .handle()
        .try_submit(move |cancellation| {
            build_session_list(
                state_path,
                domain_key,
                source,
                availability,
                before,
                limit,
                binding,
                index_status,
                cancellation,
            )
        })
        .map_err(|error| map_query_error(error, request_id))?;
    let bytes = pending
        .wait()
        .await
        .map_err(|error| map_query_error(error, request_id))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::internal_failure(request_id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionMessagesQuery {
    after: Option<String>,
    limit: Option<usize>,
    max_bytes: Option<usize>,
}

pub(super) async fn session_messages(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<SessionMessagesQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Path(session_key) = path.map_err(|_| ApiError::session_not_found(request_id))?;
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    let limit = query.limit.unwrap_or(SESSION_MESSAGES_DEFAULT_LIMIT);
    if !(1..=SESSION_MESSAGES_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::limit_out_of_range(request_id));
    }
    let maximum_bytes = query
        .max_bytes
        .unwrap_or(SESSION_MESSAGES_DEFAULT_RESPONSE_BYTES);
    if !(SESSION_MESSAGES_MIN_RESPONSE_BYTES..=SESSION_MESSAGES_MAX_RESPONSE_BYTES)
        .contains(&maximum_bytes)
    {
        return Err(ApiError::response_limit_out_of_range(request_id));
    }
    let runtime = state
        .query
        .clone()
        .ok_or_else(|| ApiError::query_busy(request_id))?;
    let state_path = runtime.state_path().to_path_buf();
    let roots = runtime.session_roots().cloned();
    let domain_key = *runtime.session_domain_key();
    let after = query.after;
    let pending = runtime
        .handle()
        .try_submit(move |cancellation| {
            build_session_messages(
                state_path,
                roots,
                domain_key,
                session_key,
                after,
                limit,
                maximum_bytes,
                cancellation,
            )
        })
        .map_err(|error| map_query_error(error, request_id))?;
    let bytes = pending
        .wait()
        .await
        .map_err(|error| map_query_error(error, request_id))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::internal_failure(request_id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageQuery {
    source: Option<String>,
    session_key: Option<String>,
    since: Option<String>,
    until: Option<String>,
    group_by: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn usage(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    let source =
        parse_source(query.source.as_deref()).ok_or_else(|| ApiError::invalid_query(request_id))?;
    let group_by = parse_usage_group(query.group_by.as_deref())
        .ok_or_else(|| ApiError::invalid_query(request_id))?;
    if query
        .since
        .as_deref()
        .is_some_and(|value| UtcTimestamp::parse(value).is_err())
        || query
            .until
            .as_deref()
            .is_some_and(|value| UtcTimestamp::parse(value).is_err())
        || matches!(
            (&query.since, &query.until),
            (Some(since), Some(until)) if since >= until
        )
    {
        return Err(ApiError::invalid_time_range(request_id));
    }
    let limit = query.limit.unwrap_or(USAGE_DEFAULT_LIMIT);
    if !(1..=USAGE_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::limit_out_of_range(request_id));
    }
    let filter = SessionUsageAggregateFilter {
        source,
        session_key: query.session_key,
        since: query.since,
        until: query.until,
    };
    let binding = usage_binding(&filter, group_by);
    let runtime = state
        .query
        .clone()
        .ok_or_else(|| ApiError::query_busy(request_id))?;
    let state_path = runtime.state_path().to_path_buf();
    let domain_key = *runtime.session_domain_key();
    let after = query.after;
    let pending = runtime
        .handle()
        .try_submit(move |cancellation| {
            build_usage(
                state_path,
                domain_key,
                filter,
                group_by,
                binding,
                after,
                limit,
                cancellation,
            )
        })
        .map_err(|error| map_query_error(error, request_id))?;
    let bytes = pending
        .wait()
        .await
        .map_err(|error| map_query_error(error, request_id))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::internal_failure(request_id))
}

#[allow(clippy::too_many_arguments)]
fn build_usage(
    state_path: PathBuf,
    domain_key: [u8; 32],
    filter: SessionUsageAggregateFilter,
    group_by: SessionUsageGroupBy,
    binding: String,
    after: Option<String>,
    limit: usize,
    cancellation: &crate::query::QueryCancellation,
) -> Result<Vec<u8>, QueryServiceError> {
    cancellation.check()?;
    let after = after
        .as_deref()
        .map(|token| {
            cursor::decode::<UsageCursor>(&domain_key, USAGE_ROUTE, &binding, token)
                .map_err(|_| QueryServiceError::InvalidCursor)
                .and_then(|cursor| match group_by {
                    SessionUsageGroupBy::Day if cursor.total_tokens.is_none() => {
                        SessionUsageAggregatePageKey::day(cursor.key)
                            .map_err(|_| QueryServiceError::InvalidCursor)
                    }
                    SessionUsageGroupBy::Source | SessionUsageGroupBy::Model
                        if cursor.total_tokens.is_some() =>
                    {
                        SessionUsageAggregatePageKey::ranked(
                            cursor
                                .total_tokens
                                .expect("a ranked cursor carries its token total"),
                            cursor.key,
                        )
                        .map_err(|_| QueryServiceError::InvalidCursor)
                    }
                    _ => Err(QueryServiceError::InvalidCursor),
                })
        })
        .transpose()?;
    let store =
        StateStore::open_live_reader(state_path).map_err(|_| QueryServiceError::Execution)?;
    let page = store
        .load_global_current_session_usage_aggregate_page(&filter, group_by, after.as_ref(), limit)
        .map_err(|error| match error {
            StorageError::StalePageKey => QueryServiceError::InvalidCursor,
            StorageError::InvalidStateRecord { .. } => QueryServiceError::Execution,
            _ => QueryServiceError::Execution,
        })?;
    cancellation.check()?;
    let next_cursor = page
        .next_page_key
        .as_ref()
        .map(|key| {
            cursor::encode(
                &domain_key,
                USAGE_ROUTE,
                &binding,
                UsageCursor {
                    key: key.key().to_owned(),
                    total_tokens: key.total_tokens(),
                },
            )
            .map_err(|_| QueryServiceError::Execution)
        })
        .transpose()?;
    let response = UsageResponse {
        schema_version: 1,
        group_by: usage_group_name(group_by),
        totals: UsageTotalsResponse::from(page.totals),
        buckets: page
            .buckets
            .into_iter()
            .map(UsageBucketResponse::from)
            .collect(),
        next_cursor,
    };
    let encoded = serde_json::to_vec(&response).map_err(|_| QueryServiceError::Execution)?;
    if encoded.len() > USAGE_MAX_RESPONSE_BYTES {
        return Err(QueryServiceError::ResponseLimit);
    }
    Ok(encoded)
}

fn parse_usage_group(value: Option<&str>) -> Option<SessionUsageGroupBy> {
    match value {
        None | Some("day") => Some(SessionUsageGroupBy::Day),
        Some("source") => Some(SessionUsageGroupBy::Source),
        Some("model") => Some(SessionUsageGroupBy::Model),
        Some(_) => None,
    }
}

fn usage_group_name(group_by: SessionUsageGroupBy) -> &'static str {
    match group_by {
        SessionUsageGroupBy::Day => "day",
        SessionUsageGroupBy::Source => "source",
        SessionUsageGroupBy::Model => "model",
    }
}

fn usage_binding(filter: &SessionUsageAggregateFilter, group_by: SessionUsageGroupBy) -> String {
    format!(
        "source={};session={};since={};until={};group={}",
        filter.source.map_or("", SessionSourceKind::as_str),
        filter.session_key.as_deref().unwrap_or_default(),
        filter.since.as_deref().unwrap_or_default(),
        filter.until.as_deref().unwrap_or_default(),
        usage_group_name(group_by)
    )
}

#[allow(clippy::too_many_arguments)]
fn build_session_messages(
    state_path: PathBuf,
    roots: Option<crate::observability::SessionRootPaths>,
    domain_key: [u8; 32],
    session_key: String,
    after: Option<String>,
    limit: usize,
    maximum_bytes: usize,
    cancellation: &crate::query::QueryCancellation,
) -> Result<Vec<u8>, QueryServiceError> {
    cancellation.check()?;
    let store =
        StateStore::open_live_reader(&state_path).map_err(|_| QueryServiceError::Execution)?;
    let index = store
        .load_global_current_session_index_by_key(&session_key)
        .map_err(|_| QueryServiceError::SessionNotFound)?
        .ok_or(QueryServiceError::SessionNotFound)?;
    if index.availability != SessionAvailability::Available {
        return Err(QueryServiceError::SessionUnavailable);
    }
    let roots = roots.ok_or(QueryServiceError::SessionUnavailable)?;
    let root = match index.source_kind {
        SessionSourceKind::Codex => roots.codex,
        SessionSourceKind::Claude => roots.claude,
        SessionSourceKind::Gemini => roots.gemini,
    };
    let binding = format!("session={session_key}");
    let mut position = after
        .as_deref()
        .map(|token| {
            cursor::decode::<SessionMessageCursor>(
                &domain_key,
                SESSION_MESSAGES_ROUTE,
                &binding,
                token,
            )
            .map_err(|_| QueryServiceError::InvalidCursor)
        })
        .transpose()?
        .unwrap_or(SessionMessageCursor {
            source_generation: index.generation,
            pager_cursor: None,
            fragment_offset_bytes: 0,
        });
    if position.source_generation != index.generation {
        return Err(QueryServiceError::SessionCursorStale);
    }
    let mut pager = MessagePager::open(index.source_kind, root, &state_path, domain_key)
        .map_err(map_pager_open_error)?;
    let mut items = Vec::with_capacity(limit);

    loop {
        cancellation.check()?;
        let pager_cursor = position
            .pager_cursor
            .as_deref()
            .map(MessagePageCursor::parse)
            .transpose()
            .map_err(|_| QueryServiceError::InvalidCursor)?;
        let page = pager
            .page(
                &index.source_key,
                MessagePageRequest {
                    maximum_messages: 1,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor: pager_cursor,
                },
            )
            .map_err(map_pager_page_error)?;
        let Some(message) = page.messages.into_iter().next() else {
            let Some(next) = page.next_cursor else {
                return encode_message_response(&items, None, index.generation, maximum_bytes);
            };
            position = SessionMessageCursor {
                source_generation: index.generation,
                pager_cursor: Some(next.as_str().to_owned()),
                fragment_offset_bytes: 0,
            };
            continue;
        };
        if position.fragment_offset_bytes > message.content.len()
            || !message
                .content
                .is_char_boundary(position.fragment_offset_bytes)
        {
            return Err(QueryServiceError::SessionCursorStale);
        }
        let full_end = message.content.len();
        let after_message = page.next_cursor.map(|next| SessionMessageCursor {
            source_generation: index.generation,
            pager_cursor: Some(next.as_str().to_owned()),
            fragment_offset_bytes: 0,
        });
        let full_item = message_item(
            &domain_key,
            &session_key,
            &message,
            position.fragment_offset_bytes,
            full_end,
        );
        let full_next = after_message
            .as_ref()
            .map(|next| encode_message_cursor(&domain_key, &binding, next))
            .transpose()?;
        let mut candidate = items.clone();
        candidate.push(full_item);
        let full_response =
            serialize_message_response(&candidate, full_next.clone(), index.generation)?;
        if full_response.len() <= maximum_bytes {
            items = candidate;
            if items.len() >= limit || after_message.is_none() {
                return Ok(full_response);
            }
            position = after_message.expect("the next message position exists");
            continue;
        }
        if !items.is_empty() {
            let next_cursor = encode_message_cursor(&domain_key, &binding, &position)?;
            return encode_message_response(
                &items,
                Some(next_cursor),
                index.generation,
                maximum_bytes,
            );
        }
        let fragment = fit_message_fragment(
            MessageFragmentContext {
                domain_key: &domain_key,
                binding: &binding,
                session_key: &session_key,
                source_generation: index.generation,
                maximum_bytes,
                cancellation,
            },
            &message,
            &position,
        )?;
        return Ok(fragment);
    }
}

struct MessageFragmentContext<'a> {
    domain_key: &'a [u8; 32],
    binding: &'a str,
    session_key: &'a str,
    source_generation: u64,
    maximum_bytes: usize,
    cancellation: &'a crate::query::QueryCancellation,
}

fn fit_message_fragment(
    context: MessageFragmentContext<'_>,
    message: &Message,
    position: &SessionMessageCursor,
) -> Result<Vec<u8>, QueryServiceError> {
    let start = position.fragment_offset_bytes;
    let Some(mut low) = next_char_boundary(&message.content, start) else {
        return Err(QueryServiceError::ResponseLimit);
    };
    let mut high = message.content.len();
    let mut best = None;
    while low <= high {
        context.cancellation.check()?;
        let mut middle = low + (high - low) / 2;
        while middle > low && !message.content.is_char_boundary(middle) {
            middle -= 1;
        }
        let next = SessionMessageCursor {
            source_generation: context.source_generation,
            pager_cursor: position.pager_cursor.clone(),
            fragment_offset_bytes: middle,
        };
        let next_cursor = encode_message_cursor(context.domain_key, context.binding, &next)?;
        let item = message_item(
            context.domain_key,
            context.session_key,
            message,
            start,
            middle,
        );
        let encoded =
            serialize_message_response(&[item], Some(next_cursor), context.source_generation)?;
        if encoded.len() <= context.maximum_bytes {
            best = Some(encoded);
            let Some(next) = next_char_boundary(&message.content, middle) else {
                break;
            };
            low = next;
        } else {
            let Some(previous) = previous_char_boundary(&message.content, middle) else {
                break;
            };
            high = previous;
        }
    }
    best.ok_or(QueryServiceError::ResponseLimit)
}

fn next_char_boundary(value: &str, offset: usize) -> Option<usize> {
    value
        .get(offset..)?
        .chars()
        .next()
        .map(|character| offset + character.len_utf8())
}

fn previous_char_boundary(value: &str, offset: usize) -> Option<usize> {
    if offset == 0 || offset > value.len() {
        return None;
    }
    let mut previous = offset - 1;
    while previous > 0 && !value.is_char_boundary(previous) {
        previous -= 1;
    }
    Some(previous)
}

fn message_item(
    domain_key: &[u8; 32],
    session_key: &str,
    message: &Message,
    start: usize,
    end: usize,
) -> SessionMessageItem {
    SessionMessageItem {
        message_key: message_key(domain_key, session_key, message),
        role: match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        },
        timestamp: message.timestamp.clone(),
        content: message.content[start..end].to_owned(),
        fragment_offset_bytes: start,
        fragment_final: end == message.content.len(),
    }
}

fn message_key(domain_key: &[u8; 32], session_key: &str, message: &Message) -> String {
    let mut digest = Sha256::new();
    digest.update(b"wokcore.session-message-key.v1");
    digest.update(domain_key);
    digest.update(session_key.as_bytes());
    digest.update([message.role as u8]);
    digest.update(message.timestamp.as_bytes());
    digest.update(message.content.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encode_message_cursor(
    domain_key: &[u8; 32],
    binding: &str,
    cursor_value: &SessionMessageCursor,
) -> Result<String, QueryServiceError> {
    cursor::encode(domain_key, SESSION_MESSAGES_ROUTE, binding, cursor_value)
        .map_err(|_| QueryServiceError::Execution)
}

fn encode_message_response(
    items: &[SessionMessageItem],
    next_cursor: Option<String>,
    source_generation: u64,
    maximum_bytes: usize,
) -> Result<Vec<u8>, QueryServiceError> {
    let encoded = serialize_message_response(items, next_cursor, source_generation)?;
    if encoded.len() > maximum_bytes {
        return Err(QueryServiceError::ResponseLimit);
    }
    Ok(encoded)
}

fn serialize_message_response(
    items: &[SessionMessageItem],
    next_cursor: Option<String>,
    source_generation: u64,
) -> Result<Vec<u8>, QueryServiceError> {
    serde_json::to_vec(&SessionMessagesResponse {
        schema_version: 1,
        items,
        next_cursor,
        source_generation,
    })
    .map_err(|_| QueryServiceError::Execution)
}

fn map_pager_open_error(error: MessagePagerError) -> QueryServiceError {
    match error {
        MessagePagerError::Root | MessagePagerError::SourceUnavailable => {
            QueryServiceError::SessionUnavailable
        }
        MessagePagerError::Storage(_)
        | MessagePagerError::InvalidPageLimit
        | MessagePagerError::InvalidCursor
        | MessagePagerError::StaleCursor
        | MessagePagerError::Read
        | MessagePagerError::ResourceLimit => QueryServiceError::Execution,
    }
}

fn map_pager_page_error(error: MessagePagerError) -> QueryServiceError {
    match error {
        MessagePagerError::InvalidCursor | MessagePagerError::StaleCursor => {
            QueryServiceError::SessionCursorStale
        }
        MessagePagerError::Root | MessagePagerError::SourceUnavailable => {
            QueryServiceError::SessionUnavailable
        }
        MessagePagerError::Storage(_)
        | MessagePagerError::InvalidPageLimit
        | MessagePagerError::Read
        | MessagePagerError::ResourceLimit => QueryServiceError::Execution,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_session_list(
    state_path: PathBuf,
    domain_key: [u8; 32],
    source: Option<SessionSourceKind>,
    availability: Option<SessionAvailability>,
    before: Option<String>,
    limit: usize,
    binding: String,
    index_status: IndexStatusResponse,
    cancellation: &crate::query::QueryCancellation,
) -> Result<Vec<u8>, QueryServiceError> {
    let mut after = before
        .as_deref()
        .map(|token| {
            cursor::decode::<SessionListCursor>(&domain_key, SESSION_LIST_ROUTE, &binding, token)
                .and_then(|cursor| {
                    GlobalSessionIndexPageKey::new(
                        cursor.last_active_at,
                        cursor.session_key,
                        cursor.source_key,
                    )
                    .map_err(|_| cursor::CursorError)
                })
                .map_err(|_| QueryServiceError::InvalidCursor)
        })
        .transpose()?;
    let store =
        StateStore::open_live_reader(state_path).map_err(|_| QueryServiceError::Execution)?;
    let mut items = Vec::with_capacity(limit.saturating_add(1));
    loop {
        cancellation.check()?;
        let page = store
            .load_global_current_session_index_page(after.as_ref(), SESSION_LIST_MAX_LIMIT)
            .map_err(|_| QueryServiceError::Execution)?;
        for record in page.items {
            if matches_session_filter(&record, source, availability) {
                items.push(session_item(record));
                if items.len() > limit {
                    break;
                }
            }
        }
        if items.len() > limit {
            break;
        }
        let Some(next) = page.next_page_key else {
            break;
        };
        after = Some(next);
    }

    let mut has_more = items.len() > limit;
    items.truncate(limit);
    loop {
        cancellation.check()?;
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| {
                    cursor::encode(
                        &domain_key,
                        SESSION_LIST_ROUTE,
                        &binding,
                        SessionListCursor {
                            last_active_at: item.last_active_at.clone(),
                            session_key: item.session_key.clone(),
                            source_key: item.source_key.clone(),
                        },
                    )
                    .map_err(|_| QueryServiceError::Execution)
                })
                .transpose()?
        } else {
            None
        };
        let response = SessionListResponse {
            schema_version: 1,
            items: &items,
            next_cursor,
            index_status: &index_status,
        };
        let encoded = serde_json::to_vec(&response).map_err(|_| QueryServiceError::Execution)?;
        if encoded.len() <= SESSION_LIST_MAX_RESPONSE_BYTES {
            return Ok(encoded);
        }
        if items.pop().is_none() {
            return Err(QueryServiceError::ResponseLimit);
        }
        has_more = true;
    }
}

fn matches_session_filter(
    record: &SessionIndexRecord,
    source: Option<SessionSourceKind>,
    availability: Option<SessionAvailability>,
) -> bool {
    source.is_none_or(|source| record.source_kind == source)
        && availability.is_none_or(|availability| record.availability == availability)
}

fn session_item(record: SessionIndexRecord) -> SessionListItem {
    SessionListItem {
        session_key: record.session_key,
        source_key: record.source_key,
        source: record.source_kind.as_str(),
        created_at: record.created_at,
        last_active_at: record.last_active_at,
        availability: record.availability.as_str(),
        message_count: record.message_count,
        usage_event_count: record.usage_event_count,
    }
}

fn parse_source(value: Option<&str>) -> Option<Option<SessionSourceKind>> {
    match value {
        None => Some(None),
        Some("codex") => Some(Some(SessionSourceKind::Codex)),
        Some("claude") => Some(Some(SessionSourceKind::Claude)),
        Some("gemini") => Some(Some(SessionSourceKind::Gemini)),
        Some(_) => None,
    }
}

fn parse_availability(value: Option<&str>) -> Option<Option<SessionAvailability>> {
    match value {
        None => Some(None),
        Some("available") => Some(Some(SessionAvailability::Available)),
        Some("unavailable") => Some(Some(SessionAvailability::Unavailable)),
        Some(_) => None,
    }
}

fn session_list_binding(
    source: Option<SessionSourceKind>,
    availability: Option<SessionAvailability>,
) -> String {
    format!(
        "source={};availability={}",
        source.map_or("", SessionSourceKind::as_str),
        availability.map_or("", SessionAvailability::as_str)
    )
}

fn index_status_response(status: IndexStatus) -> IndexStatusResponse {
    IndexStatusResponse {
        phase: match status.phase {
            IndexPhase::Starting => "starting",
            IndexPhase::Scanning => "scanning",
            IndexPhase::Idle => "idle",
        },
        sources: status
            .sources
            .into_iter()
            .map(|source| SourceIndexStatusResponse {
                source: match source.kind {
                    SessionKind::Codex => "codex",
                    SessionKind::Claude => "claude",
                    SessionKind::Gemini => "gemini",
                },
                status: source.status.as_str(),
                last_transition_at: source.last_transition_at,
                error_code: source.error_code.map(SessionSourceErrorCode::as_str),
            })
            .collect(),
    }
}

pub(super) fn map_query_error(error: QueryServiceError, request_id: RequestId) -> ApiError {
    match error {
        QueryServiceError::Busy | QueryServiceError::Closed => ApiError::query_busy(request_id),
        QueryServiceError::Timeout | QueryServiceError::Cancelled => {
            ApiError::query_timeout(request_id)
        }
        QueryServiceError::InvalidCursor => ApiError::invalid_cursor(request_id),
        QueryServiceError::SessionNotFound => ApiError::session_not_found(request_id),
        QueryServiceError::SessionUnavailable => ApiError::session_unavailable(request_id),
        QueryServiceError::SessionCursorStale => ApiError::session_cursor_stale(request_id),
        QueryServiceError::InvalidConfig
        | QueryServiceError::Worker
        | QueryServiceError::Execution
        | QueryServiceError::ResponseLimit => ApiError::internal_failure(request_id),
    }
}

#[derive(Deserialize, Serialize)]
struct SessionListCursor {
    last_active_at: String,
    session_key: String,
    source_key: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct SessionMessageCursor {
    source_generation: u64,
    pager_cursor: Option<String>,
    fragment_offset_bytes: usize,
}

#[derive(Deserialize, Serialize)]
struct UsageCursor {
    key: String,
    total_tokens: Option<u64>,
}

#[derive(Serialize)]
struct UsageResponse {
    schema_version: u8,
    group_by: &'static str,
    totals: UsageTotalsResponse,
    buckets: Vec<UsageBucketResponse>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct UsageTotalsResponse {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    session_count: u64,
}

impl From<SessionUsageAggregateTotals> for UsageTotalsResponse {
    fn from(value: SessionUsageAggregateTotals) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            reasoning_tokens: value.reasoning_tokens,
            session_count: value.session_count,
        }
    }
}

#[derive(Serialize)]
struct UsageBucketResponse {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    session_count: u64,
}

impl From<SessionUsageAggregateBucket> for UsageBucketResponse {
    fn from(value: SessionUsageAggregateBucket) -> Self {
        Self {
            key: value.key,
            start: value.start,
            end: value.end,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            reasoning_tokens: value.reasoning_tokens,
            session_count: value.session_count,
        }
    }
}

#[derive(Serialize)]
struct SessionMessagesResponse<'a> {
    schema_version: u8,
    items: &'a [SessionMessageItem],
    next_cursor: Option<String>,
    source_generation: u64,
}

#[derive(Clone, Serialize)]
struct SessionMessageItem {
    message_key: String,
    role: &'static str,
    timestamp: String,
    content: String,
    fragment_offset_bytes: usize,
    fragment_final: bool,
}

#[derive(Serialize)]
struct SessionListResponse<'a> {
    schema_version: u8,
    items: &'a [SessionListItem],
    next_cursor: Option<String>,
    index_status: &'a IndexStatusResponse,
}

#[derive(Serialize)]
struct SessionListItem {
    session_key: String,
    #[serde(skip)]
    source_key: String,
    source: &'static str,
    created_at: String,
    last_active_at: String,
    availability: &'static str,
    message_count: u64,
    usage_event_count: u64,
}

#[derive(Serialize)]
struct IndexStatusResponse {
    phase: &'static str,
    sources: Vec<SourceIndexStatusResponse>,
}

#[derive(Serialize)]
struct SourceIndexStatusResponse {
    source: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_transition_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}
