use std::{ffi::OsStr, fmt};

use crate::event::{
    DiagnosticBuildError, FailoverDecision, ModelId, PlatformCategory, ProviderProtocol,
    RedactionCounts, RetryDecision, SafeSummary, StageCode,
};

pub const MAX_SENSITIVE_VALUES: usize = 64;
pub const MAX_STRUCTURAL_OBSERVATIONS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructuralObservation {
    AdmissionAccepted,
    RouteSelected,
    CacheHit,
    CacheMiss,
    JsonShape,
    LocalizedCategory,
    EmojiCategory,
}

#[derive(Clone, Eq, PartialEq)]
pub struct StructuralObservations {
    values: [Option<StructuralObservation>; MAX_STRUCTURAL_OBSERVATIONS],
    count: usize,
}

impl StructuralObservations {
    pub const fn new() -> Self {
        Self {
            values: [None; MAX_STRUCTURAL_OBSERVATIONS],
            count: 0,
        }
    }

    pub fn push(
        mut self,
        observation: StructuralObservation,
    ) -> Result<Self, DiagnosticBuildError> {
        if self.count == MAX_STRUCTURAL_OBSERVATIONS {
            return Err(DiagnosticBuildError::CollectionLimit);
        }
        self.values[self.count] = Some(observation);
        self.count += 1;
        Ok(self)
    }
}

impl Default for StructuralObservations {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for StructuralObservations {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StructuralObservations([typed])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralSummaryInput {
    protocol: ProviderProtocol,
    stage: StageCode,
    retry: RetryDecision,
    failover: FailoverDecision,
    streaming: bool,
    platform: PlatformCategory,
    model: Option<ModelId>,
    observations: StructuralObservations,
}

impl StructuralSummaryInput {
    pub const fn new(
        protocol: ProviderProtocol,
        stage: StageCode,
        retry: RetryDecision,
        failover: FailoverDecision,
        streaming: bool,
    ) -> Self {
        Self {
            protocol,
            stage,
            retry,
            failover,
            streaming,
            platform: PlatformCategory::None,
            model: None,
            observations: StructuralObservations::new(),
        }
    }

    pub const fn with_platform(mut self, platform: PlatformCategory) -> Self {
        self.platform = platform;
        self
    }

    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_observations(mut self, observations: StructuralObservations) -> Self {
        self.observations = observations;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SensitiveCategory {
    Authorization,
    Cookie,
    Body,
    Path,
    Token,
    Credential,
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum SensitiveMaterial<'a> {
    Utf8(&'a str),
    Bytes(&'a [u8]),
    Os(&'a OsStr),
}

#[derive(Clone, Copy)]
pub struct SensitiveValue<'a> {
    category: SensitiveCategory,
    _material: SensitiveMaterial<'a>,
}

#[derive(Clone)]
pub struct SensitiveValues<'a> {
    values: [Option<SensitiveValue<'a>>; MAX_SENSITIVE_VALUES],
    count: usize,
}

impl<'a> SensitiveValues<'a> {
    pub const fn new() -> Self {
        Self {
            values: [None; MAX_SENSITIVE_VALUES],
            count: 0,
        }
    }

    pub fn push(mut self, value: SensitiveValue<'a>) -> Result<Self, DiagnosticBuildError> {
        if self.count == MAX_SENSITIVE_VALUES {
            return Err(DiagnosticBuildError::CollectionLimit);
        }
        self.values[self.count] = Some(value);
        self.count += 1;
        Ok(self)
    }
}

impl Default for SensitiveValues<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SensitiveValues<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveValues([redacted])")
    }
}

impl fmt::Debug for SensitiveValue<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveValue([redacted])")
    }
}

impl<'a> SensitiveValue<'a> {
    fn utf8(category: SensitiveCategory, material: &'a str) -> Self {
        Self {
            category,
            _material: SensitiveMaterial::Utf8(material),
        }
    }

    pub fn authorization(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Authorization, material)
    }

    pub fn proxy_authorization(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Authorization, material)
    }

    pub fn cookie(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Cookie, material)
    }

    pub fn set_cookie(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Cookie, material)
    }

    pub fn body(material: &'a [u8]) -> Self {
        Self {
            category: SensitiveCategory::Body,
            _material: SensitiveMaterial::Bytes(material),
        }
    }

    pub fn prompt(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Body, material)
    }

    pub fn response(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Body, material)
    }

    pub fn tool_json(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Body, material)
    }

    pub fn sse_frame(material: &'a [u8]) -> Self {
        Self {
            category: SensitiveCategory::Body,
            _material: SensitiveMaterial::Bytes(material),
        }
    }

    pub fn path(material: &'a OsStr) -> Self {
        Self {
            category: SensitiveCategory::Path,
            _material: SensitiveMaterial::Os(material),
        }
    }

    pub fn token(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Token, material)
    }

    pub fn api_key(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Token, material)
    }

    pub fn oauth_token(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Token, material)
    }

    pub fn credential(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Credential, material)
    }

    pub fn raw_credential_bytes(material: &'a [u8]) -> Self {
        Self {
            category: SensitiveCategory::Credential,
            _material: SensitiveMaterial::Bytes(material),
        }
    }

    pub fn account_name(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Credential, material)
    }

    pub fn backend_error(material: &'a str) -> Self {
        Self::utf8(SensitiveCategory::Credential, material)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RedactedSummary {
    summary: SafeSummary,
    counts: RedactionCounts,
}

#[derive(Clone)]
pub struct RedactedSummaries {
    values: [Option<RedactedSummary>; crate::event::MAX_EVENT_SUMMARIES],
    count: usize,
}

impl RedactedSummaries {
    pub const fn new() -> Self {
        Self {
            values: [const { None }; crate::event::MAX_EVENT_SUMMARIES],
            count: 0,
        }
    }

    pub fn push(mut self, value: RedactedSummary) -> Result<Self, DiagnosticBuildError> {
        if self.count == crate::event::MAX_EVENT_SUMMARIES {
            return Err(DiagnosticBuildError::CollectionLimit);
        }
        self.values[self.count] = Some(value);
        self.count += 1;
        Ok(self)
    }

    pub(crate) fn into_event_parts(self) -> (Box<[SafeSummary]>, RedactionCounts) {
        let mut summaries = Vec::with_capacity(self.count);
        let mut counts = RedactionCounts::new(0, 0, 0, 0, 0, 0);
        for value in self.values.into_iter().take(self.count).flatten() {
            let (summary, removed) = value.into_bound_parts();
            summaries.push(summary);
            counts.saturating_merge(removed);
        }
        (summaries.into_boxed_slice(), counts)
    }
}

impl Default for RedactedSummaries {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RedactedSummaries {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedactedSummaries([redacted])")
    }
}

impl RedactedSummary {
    pub const fn summary(&self) -> &SafeSummary {
        &self.summary
    }

    pub(crate) fn into_bound_parts(self) -> (SafeSummary, RedactionCounts) {
        (self.summary, self.counts)
    }
}

impl fmt::Debug for RedactedSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedactedSummary([redacted])")
    }
}

pub fn build_structural_summary(
    input: StructuralSummaryInput,
    sensitive: SensitiveValues<'_>,
) -> Result<RedactedSummary, DiagnosticBuildError> {
    let mut counts = [0u64; 6];
    for candidate in sensitive.values.into_iter().take(sensitive.count).flatten() {
        let index = match candidate.category {
            SensitiveCategory::Authorization => 0,
            SensitiveCategory::Cookie => 1,
            SensitiveCategory::Body => 2,
            SensitiveCategory::Path => 3,
            SensitiveCategory::Token => 4,
            SensitiveCategory::Credential => 5,
        };
        counts[index] = counts[index].saturating_add(1);
    }

    let mut full = format!(
        "protocol={};stage={};retry={};failover={};streaming={};platform={};model={};observations=",
        protocol_code(input.protocol),
        stage_code(input.stage),
        retry_code(input.retry),
        failover_code(input.failover),
        input.streaming,
        platform_code(input.platform),
        input.model.as_ref().map_or("unavailable", ModelId::as_str),
    );
    if input.observations.count == 0 {
        full.push_str("none");
    } else {
        for (index, observation) in input
            .observations
            .values
            .into_iter()
            .take(input.observations.count)
            .flatten()
            .enumerate()
        {
            if index != 0 {
                full.push('|');
            }
            full.push_str(observation_code(observation));
        }
    }

    Ok(RedactedSummary {
        summary: SafeSummary::from_already_safe(&full)?,
        counts: RedactionCounts::new(
            counts[0], counts[1], counts[2], counts[3], counts[4], counts[5],
        ),
    })
}

const fn observation_code(value: StructuralObservation) -> &'static str {
    match value {
        StructuralObservation::AdmissionAccepted => "admission_accepted",
        StructuralObservation::RouteSelected => "route_selected",
        StructuralObservation::CacheHit => "cache_hit",
        StructuralObservation::CacheMiss => "cache_miss",
        StructuralObservation::JsonShape => {
            r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#
        }
        StructuralObservation::LocalizedCategory => "category=结构化诊断",
        StructuralObservation::EmojiCategory => "category=stream_🧪_👩‍💻",
    }
}

const fn protocol_code(value: ProviderProtocol) -> &'static str {
    match value {
        ProviderProtocol::OpenAiResponses => "open_ai_responses",
        ProviderProtocol::OpenAiChat => "open_ai_chat",
        ProviderProtocol::AnthropicMessages => "anthropic_messages",
        ProviderProtocol::Gemini => "gemini",
    }
}

const fn stage_code(value: StageCode) -> &'static str {
    match value {
        StageCode::Admission => "admission",
        StageCode::Routing => "routing",
        StageCode::Upstream => "upstream",
        StageCode::Response => "response",
    }
}

const fn retry_code(value: RetryDecision) -> &'static str {
    match value {
        RetryDecision::NotApplicable => "not_applicable",
        RetryDecision::NotRetried => "not_retried",
        RetryDecision::Scheduled => "scheduled",
        RetryDecision::Exhausted => "exhausted",
    }
}

const fn failover_code(value: FailoverDecision) -> &'static str {
    match value {
        FailoverDecision::NotApplicable => "not_applicable",
        FailoverDecision::NotSelected => "not_selected",
        FailoverDecision::Selected => "selected",
        FailoverDecision::Exhausted => "exhausted",
    }
}

const fn platform_code(value: PlatformCategory) -> &'static str {
    match value {
        PlatformCategory::None => "none",
        PlatformCategory::Network => "network",
        PlatformCategory::Permission => "permission",
        PlatformCategory::Filesystem => "filesystem",
        PlatformCategory::Process => "process",
    }
}
