use std::{net::SocketAddr, str::FromStr, time::Duration};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::protocols;

pub const MAX_SCENARIO_BYTES: usize = 64 * 1024;
pub const MAX_EVENTS: usize = 4_096;
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_DELAY_MS: u64 = 60_000;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 512;
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ATTEMPT_STATUSES: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum Protocol {
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "azure_openai")]
    AzureOpenAi,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FrameMode {
    #[default]
    Normal,
    Partial,
    Coalesced,
    Malformed,
    Utf8Split,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PayloadProfile {
    #[default]
    Standard,
    Tool,
    Reasoning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scenario {
    protocol: Protocol,
    stream: bool,
    status: u16,
    ttft: Duration,
    cadence: Duration,
    jitter: Duration,
    event_count: usize,
    content_bytes: usize,
    chunk_bytes: usize,
    terminal: bool,
    seed: u64,
    frame_mode: FrameMode,
    payload_profile: PayloadProfile,
    attempt_statuses: Vec<u16>,
    disconnect_after_chunks: Option<usize>,
    headers: Vec<(String, String)>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioDocument {
    protocol: Protocol,
    #[serde(default = "default_true")]
    stream: bool,
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default)]
    ttft_ms: u64,
    #[serde(default)]
    cadence_ms: u64,
    #[serde(default)]
    jitter_ms: u64,
    #[serde(default = "default_event_count")]
    event_count: usize,
    #[serde(default)]
    content_bytes: usize,
    #[serde(default = "default_chunk_bytes")]
    chunk_bytes: usize,
    #[serde(default = "default_true")]
    terminal: bool,
    #[serde(default = "default_seed")]
    seed: u64,
    #[serde(default)]
    frame_mode: FrameMode,
    #[serde(default)]
    payload_profile: PayloadProfile,
    #[serde(default)]
    attempt_statuses: Vec<u16>,
    disconnect_after_chunks: Option<usize>,
    #[serde(default)]
    headers: Vec<HeaderDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderDocument {
    name: String,
    value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schedule {
    chunks: Vec<ScheduledChunk>,
}

impl Schedule {
    #[must_use]
    pub fn chunks(&self) -> &[ScheduledChunk] {
        &self.chunks
    }

    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.chunks.iter().fold(0_usize, |total, chunk| {
            total.saturating_add(chunk.bytes.len())
        })
    }

    pub(crate) fn into_chunks(self) -> Vec<ScheduledChunk> {
        self.chunks
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledChunk {
    delay: Duration,
    bytes: Vec<u8>,
}

impl ScheduledChunk {
    pub(crate) fn new(delay: Duration, bytes: Vec<u8>) -> Self {
        Self { delay, bytes }
    }

    #[must_use]
    pub fn delay(&self) -> Duration {
        self.delay
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_parts(self) -> (Duration, Vec<u8>) {
        (self.delay, self.bytes)
    }
}

#[derive(Debug, Error)]
pub enum ScenarioError {
    #[error("scenario exceeds its bounded input size")]
    InputTooLarge,
    #[error("scenario is invalid")]
    Invalid,
    #[error("endpoint must be a literal loopback address")]
    NonLoopback,
}

impl Scenario {
    pub fn from_toml(input: &str) -> Result<Self, ScenarioError> {
        if input.len() > MAX_SCENARIO_BYTES {
            return Err(ScenarioError::InputTooLarge);
        }
        let document: ScenarioDocument =
            toml_edit::de::from_str(input).map_err(|_| ScenarioError::Invalid)?;
        let scenario = Self {
            protocol: document.protocol,
            stream: document.stream,
            status: document.status,
            ttft: Duration::from_millis(document.ttft_ms),
            cadence: Duration::from_millis(document.cadence_ms),
            jitter: Duration::from_millis(document.jitter_ms),
            event_count: document.event_count,
            content_bytes: document.content_bytes,
            chunk_bytes: document.chunk_bytes,
            terminal: document.terminal,
            seed: document.seed,
            frame_mode: document.frame_mode,
            payload_profile: document.payload_profile,
            attempt_statuses: document.attempt_statuses,
            disconnect_after_chunks: document.disconnect_after_chunks,
            headers: document
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
        };
        scenario.validate()?;
        Ok(scenario)
    }

    #[must_use]
    pub fn standard(protocol: Protocol) -> Self {
        Self {
            protocol,
            stream: true,
            status: 200,
            ttft: Duration::ZERO,
            cadence: Duration::ZERO,
            jitter: Duration::ZERO,
            event_count: default_event_count(),
            content_bytes: 0,
            chunk_bytes: default_chunk_bytes(),
            terminal: true,
            seed: default_seed(),
            frame_mode: FrameMode::Normal,
            payload_profile: PayloadProfile::Standard,
            attempt_statuses: Vec::new(),
            disconnect_after_chunks: None,
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_event_count(mut self, event_count: usize) -> Self {
        self.event_count = event_count;
        self
    }

    #[must_use]
    pub fn with_ttft(mut self, ttft: Duration) -> Self {
        self.ttft = ttft;
        self
    }

    #[must_use]
    pub fn with_cadence(mut self, cadence: Duration) -> Self {
        self.cadence = cadence;
        self
    }

    #[must_use]
    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub fn with_frame_mode(mut self, frame_mode: FrameMode) -> Self {
        self.frame_mode = frame_mode;
        self
    }

    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    #[must_use]
    pub fn frame_mode(&self) -> FrameMode {
        self.frame_mode
    }

    #[must_use]
    pub fn payload_profile(&self) -> PayloadProfile {
        self.payload_profile
    }

    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn stream(&self) -> bool {
        self.stream
    }

    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn schedule(&self) -> Result<Schedule, ScenarioError> {
        self.validate()?;
        let frames = protocols::render(self);
        let mut chunks = protocols::frame(frames, self.frame_mode, self.chunk_bytes);
        if let Some(limit) = self.disconnect_after_chunks {
            chunks.truncate(limit);
        }

        let mut random = DeterministicRandom::new(self.seed);
        let chunks = chunks
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| {
                let base = if index == 0 { self.ttft } else { self.cadence };
                let jitter_ms = random.bounded(self.jitter.as_millis() as u64);
                ScheduledChunk::new(base + Duration::from_millis(jitter_ms), bytes)
            })
            .collect();
        Ok(Schedule { chunks })
    }

    pub(crate) fn event_count(&self) -> usize {
        self.event_count
    }

    pub(crate) fn terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn content(&self, index: usize) -> String {
        if self.content_bytes == 0 {
            return format!("烧{index}");
        }
        let bytes_per_event = self.content_bytes.div_ceil(self.event_count);
        let prefix = format!("烧{index}:");
        let padding = bytes_per_event.saturating_sub(prefix.len());
        let mut content = String::with_capacity(prefix.len().saturating_add(padding));
        content.push_str(&prefix);
        content.extend(std::iter::repeat_n('x', padding));
        content
    }

    pub(crate) fn for_attempt(&self, ordinal: usize) -> Self {
        let mut scenario = self.clone();
        if let Some(status) = self
            .attempt_statuses
            .get(ordinal.min(self.attempt_statuses.len().saturating_sub(1)))
        {
            scenario.status = *status;
        }
        scenario
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        if !(100..=599).contains(&self.status)
            || self.event_count == 0
            || self.event_count > MAX_EVENTS
            || self.content_bytes > MAX_CONTENT_BYTES
            || self.chunk_bytes == 0
            || self.chunk_bytes > MAX_CHUNK_BYTES
            || self.ttft.as_millis() > u128::from(MAX_DELAY_MS)
            || self.cadence.as_millis() > u128::from(MAX_DELAY_MS)
            || self.jitter.as_millis() > u128::from(MAX_DELAY_MS)
            || self.disconnect_after_chunks == Some(0)
            || self.headers.len() > MAX_HEADERS
            || self.attempt_statuses.len() > MAX_ATTEMPT_STATUSES
            || self
                .attempt_statuses
                .iter()
                .any(|status| !(100..=599).contains(status))
        {
            return Err(ScenarioError::Invalid);
        }
        for (name, value) in &self.headers {
            if name.is_empty()
                || name.len() > MAX_HEADER_NAME_BYTES
                || value.len() > MAX_HEADER_VALUE_BYTES
                || name.parse::<axum::http::HeaderName>().is_err()
                || value.parse::<axum::http::HeaderValue>().is_err()
            {
                return Err(ScenarioError::Invalid);
            }
        }
        Ok(())
    }
}

pub fn validate_loopback_socket(value: &str) -> Result<SocketAddr, ScenarioError> {
    let address = SocketAddr::from_str(value).map_err(|_| ScenarioError::NonLoopback)?;
    if !address.ip().is_loopback() {
        return Err(ScenarioError::NonLoopback);
    }
    Ok(address)
}

pub fn validate_loopback_url(value: &str) -> Result<Url, ScenarioError> {
    let url = Url::parse(value).map_err(|_| ScenarioError::NonLoopback)?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return Err(ScenarioError::NonLoopback);
    }
    let Some(host) = url.host() else {
        return Err(ScenarioError::NonLoopback);
    };
    let is_loopback = match host {
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
        url::Host::Domain(_) => false,
    };
    if !is_loopback {
        return Err(ScenarioError::NonLoopback);
    }
    Ok(url)
}

struct DeterministicRandom {
    state: u64,
}

impl DeterministicRandom {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn bounded(&mut self, maximum: u64) -> u64 {
        if maximum == 0 {
            return 0;
        }
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state % (maximum.saturating_add(1))
    }
}

const fn default_true() -> bool {
    true
}

const fn default_status() -> u16 {
    200
}

const fn default_event_count() -> usize {
    8
}

const fn default_chunk_bytes() -> usize {
    4 * 1024
}

const fn default_seed() -> u64 {
    1
}
