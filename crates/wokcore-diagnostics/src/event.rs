use std::{fmt, sync::Arc};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, Visitor},
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MAX_PREPARED_EVENT_BYTES: usize = 16_384;
pub const MAX_ERROR_SOURCE_CHAIN: usize = 4;
pub const MAX_EVENT_SUMMARIES: usize = 4;
// Retained text leaves deterministic room for worst-case fixed-grammar JSON escaping and the
// event envelope; public decode still rejects truncated summaries whose full hash it cannot verify.
pub const MAX_SAFE_SUMMARY_BYTES: usize = 8_192;
const SEQUENCE_DIGITS: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DiagnosticBuildError {
    #[error("diagnostic value is invalid")]
    InvalidValue,
    #[error("diagnostic collection exceeds its bound")]
    CollectionLimit,
    #[error("prepared diagnostic event exceeds its byte bound")]
    EventTooLarge,
    #[error("diagnostic serialization failed")]
    Serialization,
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.chars().all(|character| {
            !character.is_whitespace()
                && !character.is_control()
                && (character.is_alphanumeric() || matches!(character, '_' | '-' | '.'))
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
}

macro_rules! validated_identifier {
    ($name:ident, $maximum:expr, $debug_name:literal) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Box<str>);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, DiagnosticBuildError> {
                if !valid_identifier(value, $maximum) {
                    return Err(DiagnosticBuildError::InvalidValue);
                }
                Ok(Self(value.into()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($debug_name)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }
    };
}

validated_identifier!(RequestId, 64, "RequestId([redacted])");
validated_identifier!(TraceId, 64, "TraceId([redacted])");
validated_identifier!(AttemptId, 64, "AttemptId([redacted])");
validated_identifier!(ClientId, 64, "ClientId([redacted])");
validated_identifier!(OpaqueSessionId, 96, "OpaqueSessionId([redacted])");
validated_identifier!(ModelId, 128, "ModelId([redacted])");
validated_identifier!(RouteId, 64, "RouteId([redacted])");
validated_identifier!(OpaqueAccountId, 96, "OpaqueAccountId([redacted])");

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventId(Uuid);

impl EventId {
    pub fn parse(value: &str) -> Result<Self, DiagnosticBuildError> {
        let parsed = Uuid::parse_str(value).map_err(|_| DiagnosticBuildError::InvalidValue)?;
        if parsed.hyphenated().to_string() != value {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(Self(parsed))
    }
}

impl fmt::Debug for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EventId([redacted])")
    }
}

impl Serialize for EventId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UtcTimestamp(Box<str>);

impl UtcTimestamp {
    pub fn parse(value: &str) -> Result<Self, DiagnosticBuildError> {
        let bytes = value.as_bytes();
        let fixed = bytes.len() == 20
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'Z'
            && bytes.iter().enumerate().all(|(index, byte)| {
                matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
            });
        if !fixed {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        let number = |range: std::ops::Range<usize>| {
            value[range]
                .parse::<u32>()
                .map_err(|_| DiagnosticBuildError::InvalidValue)
        };
        let year = number(0..4)?;
        let month = number(5..7)?;
        let day = number(8..10)?;
        let hour = number(11..13)?;
        let minute = number(14..16)?;
        let second = number(17..19)?;
        let leap_year =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap_year => 29,
            2 => 28,
            _ => 0,
        };
        if year == 0
            || !(1..=days_in_month).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Debug for UtcTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UtcTimestamp([redacted])")
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct WokcoreVersion(Box<str>);

impl WokcoreVersion {
    pub fn parse(value: &str) -> Result<Self, DiagnosticBuildError> {
        if value.is_empty()
            || value.len() > 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            || !valid_semantic_version(value)
        {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(Self(value.into()))
    }
}

fn valid_semantic_version(value: &str) -> bool {
    let mut build_parts = value.split('+');
    let core_and_pre = build_parts.next().unwrap_or_default();
    let build = build_parts.next();
    if build_parts.next().is_some() || build.is_some_and(|part| !valid_version_suffix(part, false))
    {
        return false;
    }

    let mut pre_parts = core_and_pre.split('-');
    let core = pre_parts.next().unwrap_or_default();
    let pre = pre_parts.next();
    if pre_parts.next().is_some() || pre.is_some_and(|part| !valid_version_suffix(part, true)) {
        return false;
    }

    let mut components = core.split('.');
    let major = components.next();
    let minor = components.next();
    let patch = components.next();
    components.next().is_none()
        && [major, minor, patch]
            .into_iter()
            .all(|part| part.is_some_and(valid_version_number))
}

fn valid_version_number(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_version_suffix(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!reject_numeric_leading_zero
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || identifier == "0"
                    || !identifier.starts_with('0'))
        })
}

impl fmt::Debug for WokcoreVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WokcoreVersion([redacted])")
    }
}

impl Serialize for WokcoreVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GitCommit([u8; 20]);

impl GitCommit {
    pub fn parse(value: &str) -> Result<Self, DiagnosticBuildError> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        let mut output = [0u8; 20];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            output[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Ok(Self(output))
    }
}

impl fmt::Debug for GitCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitCommit([redacted])")
    }
}

struct HexDisplay<'a>(&'a [u8]);

impl fmt::Display for HexDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for GitCommit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&HexDisplay(&self.0))
    }
}

fn hex_value(value: u8) -> Result<u8, DiagnosticBuildError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(DiagnosticBuildError::InvalidValue),
    }
}

macro_rules! closed_code {
    (
        pub enum $name:ident {
            $($variant:ident => $code:literal),+ $(,)?
        }
    ) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant),+
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let code = match self {
                    $(Self::$variant => $code),+
                };
                serializer.serialize_str(code)
            }
        }

    };
}

closed_code! {
    pub enum DiagnosticLevel {
        Trace => "trace",
        Debug => "debug",
        Info => "info",
        Warn => "warn",
        Error => "error",
    }
}

closed_code! {
    pub enum DiagnosticComponent {
        Core => "core",
        Router => "router",
        Provider => "provider",
        Storage => "storage",
        Sessions => "sessions",
        Diagnostics => "diagnostics",
        Platform => "platform",
    }
}

closed_code! {
    pub enum DiagnosticEventCode {
        LifecycleTransition => "lifecycle_transition",
        RequestCompleted => "request_completed",
        RequestFailed => "request_failed",
        RetryDecision => "retry_decision",
        FailoverDecision => "failover_decision",
        DiagnosticDrop => "diagnostics.events_dropped",
    }
}

closed_code! {
    pub enum ProviderProtocol {
        OpenAiResponses => "open_ai_responses",
        OpenAiChat => "open_ai_chat",
        AnthropicMessages => "anthropic_messages",
        Gemini => "gemini",
    }
}

closed_code! {
    pub enum StateTransition {
        StartingToReady => "starting_to_ready",
        ReadyToDegraded => "ready_to_degraded",
        DegradedToReady => "degraded_to_ready",
        ReadyToDraining => "ready_to_draining",
        DrainingToAwaitingCancellation => "draining_to_awaiting_cancellation",
        DrainingToReady => "draining_to_ready",
        ReadyToStopping => "ready_to_stopping",
        StoppingToStopped => "stopping_to_stopped",
    }
}

closed_code! {
    pub enum RetryDecision {
        NotApplicable => "not_applicable",
        NotRetried => "not_retried",
        Scheduled => "scheduled",
        Exhausted => "exhausted",
    }
}

closed_code! {
    pub enum FailoverDecision {
        NotApplicable => "not_applicable",
        NotSelected => "not_selected",
        Selected => "selected",
        Exhausted => "exhausted",
    }
}

closed_code! {
    pub enum StageCode {
        Admission => "admission",
        Routing => "routing",
        Upstream => "upstream",
        Response => "response",
    }
}

closed_code! {
    pub enum ErrorCode {
        UpstreamTimeout => "upstream_timeout",
        UpstreamUnavailable => "upstream_unavailable",
        InvalidResponse => "invalid_response",
        InternalInvariant => "internal_invariant",
        ResourceLimit => "resource_limit",
    }
}

closed_code! {
    pub enum ErrorSourceCode {
        Router => "router",
        Provider => "provider",
        Protocol => "protocol",
        Platform => "platform",
        Storage => "storage",
    }
}

closed_code! {
    pub enum PlatformCategory {
        None => "none",
        Network => "network",
        Permission => "permission",
        Filesystem => "filesystem",
        Process => "process",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CapabilityVersion(u32);

impl CapabilityVersion {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    wokcore_version: WokcoreVersion,
    git_commit: GitCommit,
    api_major: u16,
    capability_version: CapabilityVersion,
}

impl BuildIdentity {
    pub const fn new(
        wokcore_version: WokcoreVersion,
        git_commit: GitCommit,
        api_major: u16,
        capability_version: CapabilityVersion,
    ) -> Self {
        Self {
            wokcore_version,
            git_commit,
            api_major,
            capability_version,
        }
    }
}

impl fmt::Debug for BuildIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BuildIdentity([redacted])")
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Correlations {
    request_id: Option<RequestId>,
    trace_id: Option<TraceId>,
    attempt_id: Option<AttemptId>,
    client_id: Option<ClientId>,
    parent_event_id: Option<EventId>,
    opaque_session_id: Option<OpaqueSessionId>,
}

impl Correlations {
    pub const fn new(
        request_id: Option<RequestId>,
        trace_id: Option<TraceId>,
        attempt_id: Option<AttemptId>,
        client_id: Option<ClientId>,
        parent_event_id: Option<EventId>,
        opaque_session_id: Option<OpaqueSessionId>,
    ) -> Self {
        Self {
            request_id,
            trace_id,
            attempt_id,
            client_id,
            parent_event_id,
            opaque_session_id,
        }
    }
}

impl fmt::Debug for Correlations {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Correlations([redacted])")
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderContext {
    protocol: ProviderProtocol,
    model: Option<ModelId>,
    route: Option<RouteId>,
    opaque_account_id: Option<OpaqueAccountId>,
}

impl ProviderContext {
    pub const fn new(protocol: ProviderProtocol) -> Self {
        Self {
            protocol,
            model: None,
            route: None,
            opaque_account_id: None,
        }
    }

    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_route(mut self, route: RouteId) -> Self {
        self.route = Some(route);
        self
    }

    pub fn with_opaque_account(mut self, account: OpaqueAccountId) -> Self {
        self.opaque_account_id = Some(account);
        self
    }
}

impl fmt::Debug for ProviderContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderContext([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticDecision {
    state_transition: StateTransition,
    retry: RetryDecision,
    failover: FailoverDecision,
}

impl DiagnosticDecision {
    pub const fn new(
        state_transition: StateTransition,
        retry: RetryDecision,
        failover: FailoverDecision,
    ) -> Self {
        Self {
            state_transition,
            retry,
            failover,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCounts {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
}

impl TokenCounts {
    pub const fn new(input: u64, output: u64, cached: u64, reasoning: u64) -> Self {
        Self {
            input,
            output,
            cached,
            reasoning,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Measurements {
    stage: StageCode,
    duration_micros: u64,
    request_bytes: u64,
    response_bytes: u64,
    tokens: TokenCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticDropCounts {
    ingress_full: u64,
    ingress_closed: u64,
    writer_failures: u64,
    invalid_events: u64,
    oversized_events: u64,
}

impl DiagnosticDropCounts {
    pub const fn new(
        ingress_full: u64,
        ingress_closed: u64,
        writer_failures: u64,
        invalid_events: u64,
        oversized_events: u64,
    ) -> Self {
        Self {
            ingress_full,
            ingress_closed,
            writer_failures,
            invalid_events,
            oversized_events,
        }
    }

    pub const fn ingress_full(self) -> u64 {
        self.ingress_full
    }

    pub const fn ingress_closed(self) -> u64 {
        self.ingress_closed
    }

    pub const fn writer_failures(self) -> u64 {
        self.writer_failures
    }

    pub const fn invalid_events(self) -> u64 {
        self.invalid_events
    }

    pub const fn oversized_events(self) -> u64 {
        self.oversized_events
    }

    pub const fn total(self) -> u64 {
        self.ingress_full
            .saturating_add(self.ingress_closed)
            .saturating_add(self.writer_failures)
            .saturating_add(self.invalid_events)
            .saturating_add(self.oversized_events)
    }
}

impl Measurements {
    pub const fn new(
        stage: StageCode,
        duration_micros: u64,
        request_bytes: u64,
        response_bytes: u64,
        tokens: TokenCounts,
    ) -> Self {
        Self {
            stage,
            duration_micros,
            request_bytes,
            response_bytes,
            tokens,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticError {
    code: ErrorCode,
    source_chain: Box<[ErrorSourceCode]>,
    platform: PlatformCategory,
}

impl DiagnosticError {
    pub fn new<const N: usize>(
        code: ErrorCode,
        source_chain: [ErrorSourceCode; N],
        platform: PlatformCategory,
    ) -> Result<Self, DiagnosticBuildError> {
        if N > MAX_ERROR_SOURCE_CHAIN {
            return Err(DiagnosticBuildError::CollectionLimit);
        }
        Ok(Self {
            code,
            source_chain: source_chain.into(),
            platform,
        })
    }
}

impl fmt::Debug for DiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticError([redacted])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SafeSummary {
    text: Box<str>,
    truncated: bool,
    original_safe_utf8_bytes: u32,
    full_safe_sha256: [u8; 32],
}

impl SafeSummary {
    pub(crate) fn from_already_safe(value: &str) -> Result<Self, DiagnosticBuildError> {
        if value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        }) {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        let original_safe_utf8_bytes =
            u32::try_from(value.len()).map_err(|_| DiagnosticBuildError::InvalidValue)?;
        let full_safe_sha256: [u8; 32] = Sha256::digest(value.as_bytes()).into();
        let mut retained = value.len().min(MAX_SAFE_SUMMARY_BYTES);
        while !value.is_char_boundary(retained) {
            retained -= 1;
        }
        Ok(Self {
            text: value[..retained].into(),
            truncated: retained != value.len(),
            original_safe_utf8_bytes,
            full_safe_sha256,
        })
    }
}

impl fmt::Debug for SafeSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SafeSummary([redacted])")
    }
}

impl Serialize for SafeSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            text: &'a str,
            truncated: bool,
            original_safe_utf8_bytes: u32,
            #[serde(serialize_with = "serialize_digest")]
            full_safe_sha256: &'a [u8; 32],
        }
        Wire {
            text: &self.text,
            truncated: self.truncated,
            original_safe_utf8_bytes: self.original_safe_utf8_bytes,
            full_safe_sha256: &self.full_safe_sha256,
        }
        .serialize(serializer)
    }
}

fn serialize_digest<S>(value: &&[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.collect_str(&HexDisplay(*value))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RedactionCounts {
    authorization_values_removed: u64,
    cookie_values_removed: u64,
    body_values_removed: u64,
    path_values_removed: u64,
    token_values_removed: u64,
    credential_values_removed: u64,
}

impl RedactionCounts {
    pub(crate) const fn new(
        authorization_values_removed: u64,
        cookie_values_removed: u64,
        body_values_removed: u64,
        path_values_removed: u64,
        token_values_removed: u64,
        credential_values_removed: u64,
    ) -> Self {
        Self {
            authorization_values_removed,
            cookie_values_removed,
            body_values_removed,
            path_values_removed,
            token_values_removed,
            credential_values_removed,
        }
    }

    pub(crate) fn saturating_merge(&mut self, other: Self) {
        self.authorization_values_removed = self
            .authorization_values_removed
            .saturating_add(other.authorization_values_removed);
        self.cookie_values_removed = self
            .cookie_values_removed
            .saturating_add(other.cookie_values_removed);
        self.body_values_removed = self
            .body_values_removed
            .saturating_add(other.body_values_removed);
        self.path_values_removed = self
            .path_values_removed
            .saturating_add(other.path_values_removed);
        self.token_values_removed = self
            .token_values_removed
            .saturating_add(other.token_values_removed);
        self.credential_values_removed = self
            .credential_values_removed
            .saturating_add(other.credential_values_removed);
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DiagnosticEvent {
    schema_version: u8,
    sequence: u64,
    event_id: EventId,
    occurred_at: UtcTimestamp,
    level: DiagnosticLevel,
    component: DiagnosticComponent,
    code: DiagnosticEventCode,
    correlations: Option<Correlations>,
    build: BuildIdentity,
    provider: Option<ProviderContext>,
    decision: Option<DiagnosticDecision>,
    measurements: Option<Measurements>,
    error: Option<DiagnosticError>,
    diagnostic_drop: Option<DiagnosticDropCounts>,
    summaries: Box<[SafeSummary]>,
    redaction_counts: RedactionCounts,
}

#[derive(Clone, Eq, PartialEq)]
pub struct DiagnosticEventDraft(DiagnosticEvent);

impl DiagnosticEventDraft {
    pub fn new(
        event_id: EventId,
        occurred_at: UtcTimestamp,
        level: DiagnosticLevel,
        component: DiagnosticComponent,
        code: DiagnosticEventCode,
        build: BuildIdentity,
    ) -> Self {
        Self(DiagnosticEvent::new(
            0,
            event_id,
            occurred_at,
            level,
            component,
            code,
            build,
        ))
    }

    pub fn with_correlations(mut self, correlations: Correlations) -> Self {
        self.0 = self.0.with_correlations(correlations);
        self
    }

    pub fn with_provider(mut self, provider: ProviderContext) -> Self {
        self.0 = self.0.with_provider(provider);
        self
    }

    pub fn with_decision(mut self, decision: DiagnosticDecision) -> Self {
        self.0 = self.0.with_decision(decision);
        self
    }

    pub fn with_measurements(mut self, measurements: Measurements) -> Self {
        self.0 = self.0.with_measurements(measurements);
        self
    }

    pub fn with_error(mut self, error: DiagnosticError) -> Self {
        self.0 = self.0.with_error(error);
        self
    }

    pub fn with_diagnostic_drop_counts(mut self, counts: DiagnosticDropCounts) -> Self {
        self.0.diagnostic_drop = Some(counts);
        self
    }

    pub fn with_redacted_summaries(
        mut self,
        summaries: crate::redaction::RedactedSummaries,
    ) -> Self {
        let (summaries, redaction_counts) = summaries.into_event_parts();
        self.0.summaries = summaries;
        self.0.redaction_counts = redaction_counts;
        self
    }
}

impl fmt::Debug for DiagnosticEventDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticEventDraft([redacted])")
    }
}

impl DiagnosticEvent {
    pub(crate) fn new(
        sequence: u64,
        event_id: EventId,
        occurred_at: UtcTimestamp,
        level: DiagnosticLevel,
        component: DiagnosticComponent,
        code: DiagnosticEventCode,
        build: BuildIdentity,
    ) -> Self {
        Self {
            schema_version: 1,
            sequence,
            event_id,
            occurred_at,
            level,
            component,
            code,
            correlations: None,
            build,
            provider: None,
            decision: None,
            measurements: None,
            error: None,
            diagnostic_drop: None,
            summaries: Box::new([]),
            redaction_counts: RedactionCounts::new(0, 0, 0, 0, 0, 0),
        }
    }

    pub(crate) fn with_correlations(mut self, correlations: Correlations) -> Self {
        self.correlations = Some(correlations);
        self
    }

    pub(crate) fn with_provider(mut self, provider: ProviderContext) -> Self {
        self.provider = Some(provider);
        self
    }

    pub(crate) fn with_decision(mut self, decision: DiagnosticDecision) -> Self {
        self.decision = Some(decision);
        self
    }

    pub(crate) fn with_measurements(mut self, measurements: Measurements) -> Self {
        self.measurements = Some(measurements);
        self
    }

    pub(crate) fn with_error(mut self, error: DiagnosticError) -> Self {
        self.error = Some(error);
        self
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    pub const fn code(&self) -> DiagnosticEventCode {
        self.code
    }

    pub const fn level(&self) -> DiagnosticLevel {
        self.level
    }

    pub const fn diagnostic_drop_counts(&self) -> Option<DiagnosticDropCounts> {
        self.diagnostic_drop
    }
}

impl fmt::Debug for DiagnosticEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticEvent([redacted])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PreparedDiagnosticEvent {
    sequence: u64,
    event_id: EventId,
    encoded: Arc<[u8]>,
}

pub(crate) struct DiagnosticEventTemplate {
    event_id: EventId,
    encoded: Vec<u8>,
    sequence_offset: usize,
}

impl DiagnosticEventDraft {
    pub(crate) fn prepare_template(self) -> Result<DiagnosticEventTemplate, DiagnosticBuildError> {
        if (self.0.code == DiagnosticEventCode::DiagnosticDrop)
            != self
                .0
                .diagnostic_drop
                .is_some_and(|counts| counts.total() != 0)
        {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        let event_id = self.0.event_id;
        let encoded =
            serde_json::to_vec(&self.0).map_err(|_| DiagnosticBuildError::Serialization)?;
        if encoded.len() > MAX_PREPARED_EVENT_BYTES {
            return Err(DiagnosticBuildError::EventTooLarge);
        }
        let marker = b"\"sequence\":\"00000000000000000000\"";
        let marker_start = encoded
            .windows(marker.len())
            .position(|window| window == marker)
            .ok_or(DiagnosticBuildError::Serialization)?;
        Ok(DiagnosticEventTemplate {
            event_id,
            encoded,
            sequence_offset: marker_start + b"\"sequence\":\"".len(),
        })
    }
}

impl DiagnosticEventTemplate {
    pub(crate) fn finalize(
        mut self,
        sequence: u64,
    ) -> Result<PreparedDiagnosticEvent, DiagnosticBuildError> {
        if sequence == 0 {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        let mut remaining = sequence;
        for index in (self.sequence_offset..self.sequence_offset + SEQUENCE_DIGITS).rev() {
            self.encoded[index] = b'0'
                + u8::try_from(remaining % 10).map_err(|_| DiagnosticBuildError::Serialization)?;
            remaining /= 10;
        }
        if remaining != 0 {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(PreparedDiagnosticEvent {
            sequence,
            event_id: self.event_id,
            encoded: Arc::from(self.encoded.into_boxed_slice()),
        })
    }
}

impl fmt::Debug for DiagnosticEventTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticEventTemplate([redacted])")
    }
}

impl PreparedDiagnosticEvent {
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn encoded_handle(&self) -> Arc<[u8]> {
        Arc::clone(&self.encoded)
    }

    pub fn encoded_len(&self) -> usize {
        self.encoded.len()
    }
}

impl fmt::Debug for PreparedDiagnosticEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedDiagnosticEvent([redacted])")
    }
}

impl Serialize for DiagnosticEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            schema_version: u8,
            sequence: SequenceWire,
            event_id: EventId,
            occurred_at: &'a UtcTimestamp,
            level: DiagnosticLevel,
            component: DiagnosticComponent,
            code: DiagnosticEventCode,
            correlations: &'a Option<Correlations>,
            build: &'a BuildIdentity,
            provider: &'a Option<ProviderContext>,
            decision: &'a Option<DiagnosticDecision>,
            measurements: &'a Option<Measurements>,
            error: &'a Option<DiagnosticError>,
            diagnostic_drop: &'a Option<DiagnosticDropCounts>,
            summaries: &'a [SafeSummary],
            redaction_counts: RedactionCounts,
        }
        Wire {
            schema_version: self.schema_version,
            sequence: SequenceWire(self.sequence),
            event_id: self.event_id,
            occurred_at: &self.occurred_at,
            level: self.level,
            component: self.component,
            code: self.code,
            correlations: &self.correlations,
            build: &self.build,
            provider: &self.provider,
            decision: &self.decision,
            measurements: &self.measurements,
            error: &self.error,
            diagnostic_drop: &self.diagnostic_drop,
            summaries: &self.summaries,
            redaction_counts: self.redaction_counts,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Copy)]
struct SequenceWire(u64);

impl Serialize for SequenceWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        struct FixedSequence(u64);
        impl fmt::Display for FixedSequence {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{:020}", self.0)
            }
        }
        serializer.collect_str(&FixedSequence(self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DiagnosticDecodeError {
    #[error("invalid diagnostic event")]
    Invalid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeEvent {
    schema_version: u8,
    sequence: Box<str>,
    event_id: Box<str>,
    occurred_at: Box<str>,
    level: Box<str>,
    component: Box<str>,
    code: Box<str>,
    correlations: Option<DecodeCorrelations>,
    build: DecodeBuild,
    provider: Option<DecodeProvider>,
    decision: Option<DecodeDecision>,
    measurements: Option<DecodeMeasurements>,
    error: Option<DecodeError>,
    diagnostic_drop: Option<DecodeDiagnosticDropCounts>,
    summaries: BoundedSequence<DecodeSummary, MAX_EVENT_SUMMARIES>,
    redaction_counts: DecodeRedactionCounts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeCorrelations {
    request_id: Option<Box<str>>,
    trace_id: Option<Box<str>>,
    attempt_id: Option<Box<str>>,
    client_id: Option<Box<str>>,
    parent_event_id: Option<Box<str>>,
    opaque_session_id: Option<Box<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeBuild {
    wokcore_version: Box<str>,
    git_commit: Box<str>,
    api_major: u16,
    capability_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeProvider {
    protocol: Box<str>,
    model: Option<Box<str>>,
    route: Option<Box<str>>,
    opaque_account_id: Option<Box<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeDecision {
    state_transition: Box<str>,
    retry: Box<str>,
    failover: Box<str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeTokenCounts {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeMeasurements {
    stage: Box<str>,
    duration_micros: u64,
    request_bytes: u64,
    response_bytes: u64,
    tokens: DecodeTokenCounts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeError {
    code: Box<str>,
    source_chain: BoundedSequence<Box<str>, MAX_ERROR_SOURCE_CHAIN>,
    platform: Box<str>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeDiagnosticDropCounts {
    ingress_full: u64,
    ingress_closed: u64,
    writer_failures: u64,
    invalid_events: u64,
    oversized_events: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeSummary {
    text: Box<str>,
    truncated: bool,
    original_safe_utf8_bytes: u32,
    full_safe_sha256: Box<str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeRedactionCounts {
    authorization_values_removed: u64,
    cookie_values_removed: u64,
    body_values_removed: u64,
    path_values_removed: u64,
    token_values_removed: u64,
    credential_values_removed: u64,
}

struct BoundedSequence<T, const N: usize>(Vec<T>);

impl<'de, T, const N: usize> Deserialize<'de> for BoundedSequence<T, N>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SequenceVisitor<T, const N: usize>(std::marker::PhantomData<T>);

        impl<'de, T, const N: usize> Visitor<'de> for SequenceVisitor<T, N>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedSequence<T, N>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded diagnostic sequence")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(N);
                for _ in 0..N {
                    match sequence.next_element()? {
                        Some(value) => values.push(value),
                        None => return Ok(BoundedSequence(values)),
                    }
                }
                if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom("diagnostic collection exceeds its bound"));
                }
                Ok(BoundedSequence(values))
            }
        }

        deserializer.deserialize_seq(SequenceVisitor(std::marker::PhantomData))
    }
}

impl DiagnosticEvent {
    pub fn decode(encoded: &[u8]) -> Result<Self, DiagnosticDecodeError> {
        decode_event(encoded, false)
    }
}

pub(crate) fn decode_trusted_prepared_encoding(
    encoded: &[u8],
) -> Result<DiagnosticEvent, DiagnosticDecodeError> {
    decode_event(encoded, true)
}

fn decode_event(
    encoded: &[u8],
    allow_truncated_summary: bool,
) -> Result<DiagnosticEvent, DiagnosticDecodeError> {
    if encoded.is_empty() || encoded.len() > MAX_PREPARED_EVENT_BYTES {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let wire: DecodeEvent =
        serde_json::from_slice(encoded).map_err(|_| DiagnosticDecodeError::Invalid)?;
    decode_event_wire(wire, allow_truncated_summary)
}

impl TryFrom<DecodeEvent> for DiagnosticEvent {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeEvent) -> Result<Self, Self::Error> {
        decode_event_wire(wire, false)
    }
}

fn decode_event_wire(
    wire: DecodeEvent,
    allow_truncated_summary: bool,
) -> Result<DiagnosticEvent, DiagnosticDecodeError> {
    let sequence = decode_sequence(&wire.sequence)?;
    if wire.schema_version != 1 {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let correlations = wire.correlations.map(TryInto::try_into).transpose()?;
    let provider = wire.provider.map(TryInto::try_into).transpose()?;
    let decision = wire.decision.map(TryInto::try_into).transpose()?;
    let measurements = wire.measurements.map(TryInto::try_into).transpose()?;
    let error = wire.error.map(TryInto::try_into).transpose()?;
    let code = decode_event_code(&wire.code)?;
    let diagnostic_drop = wire.diagnostic_drop.map(Into::into);
    if (code == DiagnosticEventCode::DiagnosticDrop)
        != diagnostic_drop.is_some_and(|counts: DiagnosticDropCounts| counts.total() != 0)
    {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let summaries = wire
        .summaries
        .0
        .into_iter()
        .map(|summary| decode_summary(summary, allow_truncated_summary))
        .collect::<Result<Vec<_>, _>>()?
        .into_boxed_slice();
    Ok(DiagnosticEvent {
        schema_version: 1,
        sequence,
        event_id: EventId::parse(&wire.event_id).map_err(decode_invalid)?,
        occurred_at: UtcTimestamp::parse(&wire.occurred_at).map_err(decode_invalid)?,
        level: decode_level(&wire.level)?,
        component: decode_component(&wire.component)?,
        code,
        correlations,
        build: wire.build.try_into()?,
        provider,
        decision,
        measurements,
        error,
        diagnostic_drop,
        summaries,
        redaction_counts: wire.redaction_counts.into(),
    })
}

impl From<DecodeDiagnosticDropCounts> for DiagnosticDropCounts {
    fn from(wire: DecodeDiagnosticDropCounts) -> Self {
        Self::new(
            wire.ingress_full,
            wire.ingress_closed,
            wire.writer_failures,
            wire.invalid_events,
            wire.oversized_events,
        )
    }
}

impl TryFrom<DecodeCorrelations> for Correlations {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeCorrelations) -> Result<Self, Self::Error> {
        Ok(Self::new(
            parse_optional::<RequestId>(wire.request_id)?,
            parse_optional::<TraceId>(wire.trace_id)?,
            parse_optional::<AttemptId>(wire.attempt_id)?,
            parse_optional::<ClientId>(wire.client_id)?,
            wire.parent_event_id
                .map(|value| EventId::parse(&value).map_err(decode_invalid))
                .transpose()?,
            parse_optional::<OpaqueSessionId>(wire.opaque_session_id)?,
        ))
    }
}

trait ParseDiagnosticIdentifier: Sized {
    fn parse_decode(value: &str) -> Result<Self, DiagnosticBuildError>;
}

macro_rules! parse_identifier {
    ($($name:ident),+ $(,)?) => {
        $(
            impl ParseDiagnosticIdentifier for $name {
                fn parse_decode(value: &str) -> Result<Self, DiagnosticBuildError> {
                    Self::parse(value)
                }
            }
        )+
    };
}

parse_identifier!(
    RequestId,
    TraceId,
    AttemptId,
    ClientId,
    OpaqueSessionId,
    ModelId,
    RouteId,
    OpaqueAccountId,
);

fn parse_optional<T: ParseDiagnosticIdentifier>(
    value: Option<Box<str>>,
) -> Result<Option<T>, DiagnosticDecodeError> {
    value
        .map(|value| T::parse_decode(&value).map_err(decode_invalid))
        .transpose()
}

impl TryFrom<DecodeBuild> for BuildIdentity {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeBuild) -> Result<Self, Self::Error> {
        Ok(Self::new(
            WokcoreVersion::parse(&wire.wokcore_version).map_err(decode_invalid)?,
            GitCommit::parse(&wire.git_commit).map_err(decode_invalid)?,
            wire.api_major,
            CapabilityVersion::new(wire.capability_version),
        ))
    }
}

impl TryFrom<DecodeProvider> for ProviderContext {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeProvider) -> Result<Self, Self::Error> {
        let mut provider = Self::new(decode_protocol(&wire.protocol)?);
        if let Some(model) = wire.model {
            provider = provider.with_model(ModelId::parse(&model).map_err(decode_invalid)?);
        }
        if let Some(route) = wire.route {
            provider = provider.with_route(RouteId::parse(&route).map_err(decode_invalid)?);
        }
        if let Some(account) = wire.opaque_account_id {
            provider = provider
                .with_opaque_account(OpaqueAccountId::parse(&account).map_err(decode_invalid)?);
        }
        Ok(provider)
    }
}

impl TryFrom<DecodeDecision> for DiagnosticDecision {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeDecision) -> Result<Self, Self::Error> {
        Ok(Self::new(
            decode_transition(&wire.state_transition)?,
            decode_retry(&wire.retry)?,
            decode_failover(&wire.failover)?,
        ))
    }
}

impl TryFrom<DecodeMeasurements> for Measurements {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeMeasurements) -> Result<Self, Self::Error> {
        Ok(Self::new(
            decode_stage(&wire.stage)?,
            wire.duration_micros,
            wire.request_bytes,
            wire.response_bytes,
            TokenCounts::new(
                wire.tokens.input,
                wire.tokens.output,
                wire.tokens.cached,
                wire.tokens.reasoning,
            ),
        ))
    }
}

impl TryFrom<DecodeError> for DiagnosticError {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeError) -> Result<Self, Self::Error> {
        let mut sources = Vec::with_capacity(MAX_ERROR_SOURCE_CHAIN);
        for source in wire.source_chain.0 {
            sources.push(decode_error_source(&source)?);
        }
        Ok(Self {
            code: decode_error_code(&wire.code)?,
            source_chain: sources.into_boxed_slice(),
            platform: decode_platform(&wire.platform)?,
        })
    }
}

impl TryFrom<DecodeSummary> for SafeSummary {
    type Error = DiagnosticDecodeError;

    fn try_from(wire: DecodeSummary) -> Result<Self, Self::Error> {
        decode_summary(wire, false)
    }
}

fn decode_summary(
    wire: DecodeSummary,
    allow_truncated: bool,
) -> Result<SafeSummary, DiagnosticDecodeError> {
    let digest = decode_digest(&wire.full_safe_sha256)?;
    if wire.truncated {
        let original_safe_utf8_bytes = usize::try_from(wire.original_safe_utf8_bytes)
            .map_err(|_| DiagnosticDecodeError::Invalid)?;
        if !allow_truncated
            || wire.text.len() > MAX_SAFE_SUMMARY_BYTES
            || original_safe_utf8_bytes <= wire.text.len()
            || !valid_structural_summary_prefix(&wire.text)
        {
            return Err(DiagnosticDecodeError::Invalid);
        }
        return Ok(SafeSummary {
            text: wire.text,
            truncated: true,
            original_safe_utf8_bytes: wire.original_safe_utf8_bytes,
            full_safe_sha256: digest,
        });
    }
    if !valid_structural_summary(&wire.text) {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let summary = SafeSummary::from_already_safe(&wire.text).map_err(decode_invalid)?;
    if summary.original_safe_utf8_bytes != wire.original_safe_utf8_bytes
        || summary.full_safe_sha256 != digest
    {
        return Err(DiagnosticDecodeError::Invalid);
    }
    Ok(summary)
}

fn valid_structural_summary_prefix(value: &str) -> bool {
    let Some((header, observations)) = value.split_once(";observations=") else {
        return false;
    };
    let candidate = format!("{header};observations=none");
    valid_structural_summary(&candidate) && valid_structural_observations_prefix(observations)
}

fn valid_structural_observations_prefix(value: &str) -> bool {
    const OBSERVATIONS: [&str; 7] = [
        "none",
        "admission_accepted",
        "route_selected",
        "cache_hit",
        "cache_miss",
        r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#,
        "category=结构化诊断",
    ];
    const EMOJI: &str = "category=stream_🧪_👩‍💻";

    let mut parts = value.split('|').peekable();
    let mut count = 0usize;
    while let Some(part) = parts.next() {
        count = count.saturating_add(1);
        if count > crate::redaction::MAX_STRUCTURAL_OBSERVATIONS {
            return false;
        }
        let is_last = parts.peek().is_none();
        if !is_last
            && !OBSERVATIONS
                .iter()
                .copied()
                .chain(std::iter::once(EMOJI))
                .any(|candidate| candidate == part)
        {
            return false;
        }
        if is_last {
            return OBSERVATIONS
                .iter()
                .copied()
                .chain(std::iter::once(EMOJI))
                .any(|candidate| candidate.starts_with(part));
        }
    }
    false
}

fn valid_structural_summary(value: &str) -> bool {
    let mut parts = value.split(';');
    let Some(protocol) = parts.next().and_then(|part| part.strip_prefix("protocol=")) else {
        return false;
    };
    let Some(stage) = parts.next().and_then(|part| part.strip_prefix("stage=")) else {
        return false;
    };
    let Some(retry) = parts.next().and_then(|part| part.strip_prefix("retry=")) else {
        return false;
    };
    let Some(failover) = parts.next().and_then(|part| part.strip_prefix("failover=")) else {
        return false;
    };
    let Some(streaming) = parts
        .next()
        .and_then(|part| part.strip_prefix("streaming="))
    else {
        return false;
    };
    let Some(platform) = parts.next().and_then(|part| part.strip_prefix("platform=")) else {
        return false;
    };
    let Some(model) = parts.next().and_then(|part| part.strip_prefix("model=")) else {
        return false;
    };
    let Some(observations) = parts
        .next()
        .and_then(|part| part.strip_prefix("observations="))
    else {
        return false;
    };
    parts.next().is_none()
        && decode_protocol(protocol).is_ok()
        && decode_stage(stage).is_ok()
        && decode_retry(retry).is_ok()
        && decode_failover(failover).is_ok()
        && matches!(streaming, "true" | "false")
        && decode_platform(platform).is_ok()
        && (model == "unavailable" || ModelId::parse(model).is_ok())
        && valid_structural_observations(observations)
}

fn valid_structural_observations(value: &str) -> bool {
    if value == "none" {
        return true;
    }
    let mut count = 0usize;
    for observation in value.split('|') {
        count += 1;
        if count > crate::redaction::MAX_STRUCTURAL_OBSERVATIONS
            || !matches!(
                observation,
                "admission_accepted"
                    | "route_selected"
                    | "cache_hit"
                    | "cache_miss"
                    | r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#
                    | "category=结构化诊断"
                    | "category=stream_🧪_👩‍💻"
            )
        {
            return false;
        }
    }
    count != 0
}

impl From<DecodeRedactionCounts> for RedactionCounts {
    fn from(wire: DecodeRedactionCounts) -> Self {
        Self::new(
            wire.authorization_values_removed,
            wire.cookie_values_removed,
            wire.body_values_removed,
            wire.path_values_removed,
            wire.token_values_removed,
            wire.credential_values_removed,
        )
    }
}

fn decode_invalid(_: DiagnosticBuildError) -> DiagnosticDecodeError {
    DiagnosticDecodeError::Invalid
}

fn decode_digest(value: &str) -> Result<[u8; 32], DiagnosticDecodeError> {
    if value.len() != 64 {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let mut output = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = hex_value(pair[0])
            .and_then(|high| hex_value(pair[1]).map(|low| (high << 4) | low))
            .map_err(decode_invalid)?;
    }
    Ok(output)
}

fn decode_sequence(value: &str) -> Result<u64, DiagnosticDecodeError> {
    if value.len() != SEQUENCE_DIGITS || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DiagnosticDecodeError::Invalid);
    }
    let sequence = value
        .parse::<u64>()
        .map_err(|_| DiagnosticDecodeError::Invalid)?;
    if sequence == 0 || format!("{sequence:020}") != value {
        return Err(DiagnosticDecodeError::Invalid);
    }
    Ok(sequence)
}

macro_rules! decode_code {
    ($function:ident, $type:ident, {$($code:literal => $variant:ident),+ $(,)?}) => {
        fn $function(value: &str) -> Result<$type, DiagnosticDecodeError> {
            match value {
                $($code => Ok($type::$variant),)+
                _ => Err(DiagnosticDecodeError::Invalid),
            }
        }
    };
}

decode_code!(decode_level, DiagnosticLevel, {
    "trace" => Trace,
    "debug" => Debug,
    "info" => Info,
    "warn" => Warn,
    "error" => Error,
});
decode_code!(decode_component, DiagnosticComponent, {
    "core" => Core,
    "router" => Router,
    "provider" => Provider,
    "storage" => Storage,
    "sessions" => Sessions,
    "diagnostics" => Diagnostics,
    "platform" => Platform,
});
decode_code!(decode_event_code, DiagnosticEventCode, {
    "lifecycle_transition" => LifecycleTransition,
    "request_completed" => RequestCompleted,
    "request_failed" => RequestFailed,
    "retry_decision" => RetryDecision,
    "failover_decision" => FailoverDecision,
    "diagnostics.events_dropped" => DiagnosticDrop,
});
decode_code!(decode_protocol, ProviderProtocol, {
    "open_ai_responses" => OpenAiResponses,
    "open_ai_chat" => OpenAiChat,
    "anthropic_messages" => AnthropicMessages,
    "gemini" => Gemini,
});
decode_code!(decode_transition, StateTransition, {
    "starting_to_ready" => StartingToReady,
    "ready_to_degraded" => ReadyToDegraded,
    "degraded_to_ready" => DegradedToReady,
    "ready_to_draining" => ReadyToDraining,
    "draining_to_awaiting_cancellation" => DrainingToAwaitingCancellation,
    "draining_to_ready" => DrainingToReady,
    "ready_to_stopping" => ReadyToStopping,
    "stopping_to_stopped" => StoppingToStopped,
});
decode_code!(decode_retry, RetryDecision, {
    "not_applicable" => NotApplicable,
    "not_retried" => NotRetried,
    "scheduled" => Scheduled,
    "exhausted" => Exhausted,
});
decode_code!(decode_failover, FailoverDecision, {
    "not_applicable" => NotApplicable,
    "not_selected" => NotSelected,
    "selected" => Selected,
    "exhausted" => Exhausted,
});
decode_code!(decode_stage, StageCode, {
    "admission" => Admission,
    "routing" => Routing,
    "upstream" => Upstream,
    "response" => Response,
});
decode_code!(decode_error_code, ErrorCode, {
    "upstream_timeout" => UpstreamTimeout,
    "upstream_unavailable" => UpstreamUnavailable,
    "invalid_response" => InvalidResponse,
    "internal_invariant" => InternalInvariant,
    "resource_limit" => ResourceLimit,
});
decode_code!(decode_error_source, ErrorSourceCode, {
    "router" => Router,
    "provider" => Provider,
    "protocol" => Protocol,
    "platform" => Platform,
    "storage" => Storage,
});
decode_code!(decode_platform, PlatformCategory, {
    "none" => None,
    "network" => Network,
    "permission" => Permission,
    "filesystem" => Filesystem,
    "process" => Process,
});
