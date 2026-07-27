use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue},
};
use serde::Serialize;
use thiserror::Error;
use tokio::task::JoinSet;
use url::Url;

use crate::validate_loopback_url;

const MAX_CONCURRENCY: usize = 10_000;
const MAX_RAMP: Duration = Duration::from_secs(10 * 60);
const MAX_DURATION: Duration = Duration::from_secs(60 * 60);
const MAX_SLOW_CONSUMER_DELAY: Duration = Duration::from_secs(60);
const MAX_PROTOCOLS: usize = 3;
const MAX_PROTOCOL_WEIGHT: u32 = 10_000;
const MAX_ERROR_SAMPLES: usize = 16;
const DEFAULT_SEED: u64 = 0x0057_4f4b_434f_5245;
const STANDARD_REQUEST_BYTES: usize = 32 * 1024;
const LARGE_REQUEST_BYTES: usize = 1024 * 1024;
const LONG_STRUCTURED_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct LoadConfig {
    target: Url,
    concurrency: usize,
    ramp: Duration,
    duration: Duration,
    protocol_mix: Vec<ProtocolWeight>,
    payload_profile: LoadPayloadProfile,
    cancellation_permyriad: u16,
    slow_consumer_delay: Duration,
    bearer_token: Option<Arc<str>>,
    seed: u64,
}

impl fmt::Debug for LoadConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadConfig")
            .field("concurrency", &self.concurrency)
            .field("ramp", &self.ramp)
            .field("duration", &self.duration)
            .field("protocol_mix", &self.protocol_mix)
            .field("payload_profile", &self.payload_profile)
            .field("cancellation_permyriad", &self.cancellation_permyriad)
            .field("slow_consumer_delay", &self.slow_consumer_delay)
            .field("has_bearer_token", &self.bearer_token.is_some())
            .field("seed", &self.seed)
            .finish()
    }
}

impl LoadConfig {
    pub fn new(target: &str) -> Result<Self, LoadError> {
        let mut target = validate_loopback_url(target).map_err(|_| LoadError::NonLoopback)?;
        target.set_query(None);
        target.set_fragment(None);
        Ok(Self {
            target,
            concurrency: 1,
            ramp: Duration::ZERO,
            duration: Duration::from_secs(30),
            protocol_mix: vec![ProtocolWeight::new(LoadProtocol::Responses, 1)],
            payload_profile: LoadPayloadProfile::Standard32K,
            cancellation_permyriad: 0,
            slow_consumer_delay: Duration::ZERO,
            bearer_token: None,
            seed: DEFAULT_SEED,
        })
    }

    #[must_use]
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    #[must_use]
    pub fn with_ramp(mut self, ramp: Duration) -> Self {
        self.ramp = ramp;
        self
    }

    #[must_use]
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    #[must_use]
    pub fn with_protocol_mix(mut self, protocol_mix: Vec<ProtocolWeight>) -> Self {
        self.protocol_mix = protocol_mix;
        self
    }

    #[must_use]
    pub fn with_payload_profile(mut self, payload_profile: LoadPayloadProfile) -> Self {
        self.payload_profile = payload_profile;
        self
    }

    #[must_use]
    pub fn with_cancellation_permyriad(mut self, cancellation_permyriad: u16) -> Self {
        self.cancellation_permyriad = cancellation_permyriad;
        self
    }

    #[must_use]
    pub fn with_slow_consumer_delay(mut self, slow_consumer_delay: Duration) -> Self {
        self.slow_consumer_delay = slow_consumer_delay;
        self
    }

    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: String) -> Self {
        self.bearer_token = Some(Arc::from(bearer_token));
        self
    }

    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn validate(&self) -> Result<(), LoadError> {
        if self.concurrency == 0
            || self.concurrency > MAX_CONCURRENCY
            || self.ramp > MAX_RAMP
            || self.duration.is_zero()
            || self.duration > MAX_DURATION
            || self.slow_consumer_delay > MAX_SLOW_CONSUMER_DELAY
            || self.cancellation_permyriad > 10_000
            || self.protocol_mix.is_empty()
            || self.protocol_mix.len() > MAX_PROTOCOLS
            || self
                .protocol_mix
                .iter()
                .any(|entry| entry.weight == 0 || entry.weight > MAX_PROTOCOL_WEIGHT)
            || self
                .protocol_mix
                .iter()
                .fold(0_u32, |total, entry| total.saturating_add(entry.weight))
                == 0
        {
            return Err(LoadError::InvalidConfig);
        }
        let mut seen = [false; 3];
        for entry in &self.protocol_mix {
            let index = entry.protocol.index();
            if seen[index] {
                return Err(LoadError::InvalidConfig);
            }
            seen[index] = true;
        }
        if let Some(token) = &self.bearer_token
            && (token.is_empty()
                || token.len() > 64 * 1024
                || HeaderValue::from_str(&format!("Bearer {token}")).is_err())
        {
            return Err(LoadError::InvalidConfig);
        }
        Ok(())
    }

    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    #[must_use]
    pub fn ramp(&self) -> Duration {
        self.ramp
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    #[must_use]
    pub fn cancellation_permyriad(&self) -> u16 {
        self.cancellation_permyriad
    }

    #[must_use]
    pub fn slow_consumer_delay(&self) -> Duration {
        self.slow_consumer_delay
    }

    #[must_use]
    pub fn protocol_for_worker(&self, worker: usize) -> LoadProtocol {
        let total = self
            .protocol_mix
            .iter()
            .map(|entry| entry.weight)
            .sum::<u32>();
        let mut slot = (worker as u64 % u64::from(total)) as u32;
        for entry in &self.protocol_mix {
            if slot < entry.weight {
                return entry.protocol;
            }
            slot -= entry.weight;
        }
        self.protocol_mix[0].protocol
    }

    fn should_cancel(&self, worker: usize) -> bool {
        let slot = (worker as u64).wrapping_mul(9_973).wrapping_add(self.seed) % 10_000;
        slot < u64::from(self.cancellation_permyriad)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolWeight {
    protocol: LoadProtocol,
    weight: u32,
}

impl ProtocolWeight {
    #[must_use]
    pub const fn new(protocol: LoadProtocol, weight: u32) -> Self {
        Self { protocol, weight }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadProtocol {
    Responses,
    Chat,
    Anthropic,
}

impl LoadProtocol {
    const fn index(self) -> usize {
        match self {
            Self::Responses => 0,
            Self::Chat => 1,
            Self::Anthropic => 2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::Anthropic => "anthropic",
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::Responses => "/v1/responses",
            Self::Chat => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadPayloadProfile {
    Standard32K,
    Body1MiB,
    LongTool,
    LongReasoning,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadReport {
    configured_concurrency: usize,
    started: u64,
    active: usize,
    peak_active: usize,
    completed: u64,
    cancelled: u64,
    errors: u64,
    bytes_received: u64,
    latency_ms: LatencyPercentiles,
    protocol_started: BTreeMap<&'static str, u64>,
    error_samples: Vec<LoadErrorSample>,
}

impl LoadReport {
    #[must_use]
    pub fn started(&self) -> u64 {
        self.started
    }

    #[must_use]
    pub fn active(&self) -> usize {
        self.active
    }

    #[must_use]
    pub fn peak_active(&self) -> usize {
        self.peak_active
    }

    #[must_use]
    pub fn completed(&self) -> u64 {
        self.completed
    }

    #[must_use]
    pub fn cancelled(&self) -> u64 {
        self.cancelled
    }

    #[must_use]
    pub fn errors(&self) -> u64 {
        self.errors
    }

    #[must_use]
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    #[must_use]
    pub fn protocol_started(&self) -> &BTreeMap<&'static str, u64> {
        &self.protocol_started
    }

    #[must_use]
    pub fn error_samples(&self) -> &[LoadErrorSample] {
        &self.error_samples
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadErrorSample {
    code: &'static str,
}

impl LoadErrorSample {
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct LatencyPercentiles {
    p50: u64,
    p95: u64,
    p99: u64,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("load target must be a literal loopback HTTP URL")]
    NonLoopback,
    #[error("load configuration is invalid")]
    InvalidConfig,
    #[error("load client construction failed")]
    Client,
    #[error("load worker failed")]
    Worker,
}

pub async fn run_load(config: LoadConfig) -> Result<LoadReport, LoadError> {
    config.validate()?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(config.concurrency.min(1_024))
        .build()
        .map_err(|_| LoadError::Client)?;
    let metrics = Arc::new(LoadMetrics::default());
    let bodies = Arc::new([
        request_payload(LoadProtocol::Responses, config.payload_profile),
        request_payload(LoadProtocol::Chat, config.payload_profile),
        request_payload(LoadProtocol::Anthropic, config.payload_profile),
    ]);
    let mut workers = JoinSet::new();
    for worker in 0..config.concurrency {
        workers.spawn(run_worker(
            worker,
            config.clone(),
            client.clone(),
            Arc::clone(&bodies),
            Arc::clone(&metrics),
        ));
    }

    while let Some(result) = workers.join_next().await {
        if result.is_err() {
            metrics.record_error("worker_join");
            return Err(LoadError::Worker);
        }
    }
    Ok(metrics.report(config.concurrency))
}

async fn run_worker(
    worker: usize,
    config: LoadConfig,
    client: Client,
    bodies: Arc<[Bytes; 3]>,
    metrics: Arc<LoadMetrics>,
) {
    let ramp_delay = proportional_delay(config.ramp, worker, config.concurrency);
    if !ramp_delay.is_zero() {
        tokio::time::sleep(ramp_delay).await;
    }

    let protocol = config.protocol_for_worker(worker);
    metrics.started.fetch_add(1, Ordering::Relaxed);
    metrics.protocol_started[protocol.index()].fetch_add(1, Ordering::Relaxed);
    let _active = ActiveLoad::new(Arc::clone(&metrics));
    let started_at = Instant::now();

    let mut target = config.target.clone();
    target.set_path(protocol.path());
    target.set_query(None);
    target.set_fragment(None);
    let mut request = client
        .post(target)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "text/event-stream")
        .body(bodies[protocol.index()].clone());
    if let Some(token) = &config.bearer_token {
        let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) else {
            metrics.record_error("authorization");
            return;
        };
        request = request.header(AUTHORIZATION, value);
    }

    let outcome = tokio::time::timeout(config.duration, async {
        let response = request.send().await.map_err(|_| "transport")?;
        let status = response.status();
        let mut stream = response.bytes_stream();
        if config.should_cancel(worker) {
            let _ = stream.next().await;
            return Ok(WorkerOutcome::Cancelled);
        }
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let mut bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "body")?;
            bytes = bytes.saturating_add(chunk.len() as u64);
            if !config.slow_consumer_delay.is_zero() {
                tokio::time::sleep(config.slow_consumer_delay).await;
            }
        }
        Ok(WorkerOutcome::Completed(bytes))
    })
    .await;

    let elapsed = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    match outcome {
        Ok(Ok(WorkerOutcome::Completed(bytes))) => {
            metrics.completed.fetch_add(1, Ordering::Relaxed);
            metrics.bytes_received.fetch_add(bytes, Ordering::Relaxed);
            metrics.record_latency(elapsed);
        }
        Ok(Ok(WorkerOutcome::Cancelled)) => {
            metrics.cancelled.fetch_add(1, Ordering::Relaxed);
            metrics.record_latency(elapsed);
        }
        Ok(Err(code)) => metrics.record_error(code),
        Err(_) => metrics.record_error("timeout"),
    }
}

enum WorkerOutcome {
    Completed(u64),
    Cancelled,
}

fn classify_status(status: StatusCode) -> &'static str {
    if status.is_client_error() || status.is_server_error() {
        "http_status"
    } else {
        "http_contract"
    }
}

#[derive(Default)]
struct LoadMetrics {
    started: AtomicU64,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    completed: AtomicU64,
    cancelled: AtomicU64,
    errors: AtomicU64,
    bytes_received: AtomicU64,
    protocol_started: [AtomicU64; 3],
    latencies: Mutex<Vec<u64>>,
    error_samples: Mutex<Vec<LoadErrorSample>>,
}

impl LoadMetrics {
    fn record_latency(&self, latency_ms: u64) {
        self.latencies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(latency_ms);
    }

    fn record_error(&self, code: &'static str) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut samples = self
            .error_samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() < MAX_ERROR_SAMPLES {
            samples.push(LoadErrorSample { code });
        }
    }

    fn report(&self, configured_concurrency: usize) -> LoadReport {
        let mut latencies = self
            .latencies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        latencies.sort_unstable();
        let protocol_started = [
            LoadProtocol::Responses,
            LoadProtocol::Chat,
            LoadProtocol::Anthropic,
        ]
        .into_iter()
        .map(|protocol| {
            (
                protocol.name(),
                self.protocol_started[protocol.index()].load(Ordering::Relaxed),
            )
        })
        .collect();
        LoadReport {
            configured_concurrency,
            started: self.started.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            peak_active: self.peak_active.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            latency_ms: LatencyPercentiles {
                p50: percentile(&latencies, 50),
                p95: percentile(&latencies, 95),
                p99: percentile(&latencies, 99),
            },
            protocol_started,
            error_samples: self
                .error_samples
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

struct ActiveLoad {
    metrics: Arc<LoadMetrics>,
}

impl ActiveLoad {
    fn new(metrics: Arc<LoadMetrics>) -> Self {
        let active = metrics.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics.peak_active.fetch_max(active, Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for ActiveLoad {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() - 1).saturating_mul(percentile) / 100;
    sorted[index]
}

fn proportional_delay(total: Duration, worker: usize, concurrency: usize) -> Duration {
    if total.is_zero() || concurrency <= 1 {
        return Duration::ZERO;
    }
    let nanos = total
        .as_nanos()
        .saturating_mul(worker as u128)
        .checked_div((concurrency - 1) as u128)
        .unwrap_or(0)
        .min(u128::from(u64::MAX));
    Duration::from_nanos(nanos as u64)
}

fn request_payload(protocol: LoadProtocol, profile: LoadPayloadProfile) -> Bytes {
    let target = match profile {
        LoadPayloadProfile::Standard32K => STANDARD_REQUEST_BYTES,
        LoadPayloadProfile::Body1MiB => LARGE_REQUEST_BYTES,
        LoadPayloadProfile::LongTool | LoadPayloadProfile::LongReasoning => {
            LONG_STRUCTURED_REQUEST_BYTES
        }
    };
    let (prefix, suffix) = match (protocol, profile) {
        (LoadProtocol::Responses, LoadPayloadProfile::LongTool) => (
            r#"{"model":"synthetic","stream":true,"input":"offline","tools":[{"type":"function","name":"synthetic_tool","description":""#,
            r#"","parameters":{"type":"object"}}]}"#,
        ),
        (LoadProtocol::Chat, LoadPayloadProfile::LongTool) => (
            r#"{"model":"synthetic","stream":true,"messages":[{"role":"user","content":"offline"}],"tools":[{"type":"function","function":{"name":"synthetic_tool","description":""#,
            r#"","parameters":{"type":"object"}}}]}"#,
        ),
        (LoadProtocol::Anthropic, LoadPayloadProfile::LongTool) => (
            r#"{"model":"synthetic","stream":true,"max_tokens":1024,"messages":[{"role":"user","content":"offline"}],"tools":[{"name":"synthetic_tool","description":""#,
            r#"","input_schema":{"type":"object"}}]}"#,
        ),
        (LoadProtocol::Responses, _) => {
            (r#"{"model":"synthetic","stream":true,"input":""#, r#""}"#)
        }
        (LoadProtocol::Chat, _) => (
            r#"{"model":"synthetic","stream":true,"messages":[{"role":"user","content":""#,
            r#""}]}"#,
        ),
        (LoadProtocol::Anthropic, _) => (
            r#"{"model":"synthetic","stream":true,"max_tokens":1024,"messages":[{"role":"user","content":""#,
            r#""}]}"#,
        ),
    };
    let padding = target.saturating_sub(prefix.len().saturating_add(suffix.len()));
    let mut bytes = Vec::with_capacity(
        prefix
            .len()
            .saturating_add(padding)
            .saturating_add(suffix.len()),
    );
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.resize(bytes.len().saturating_add(padding), b'x');
    bytes.extend_from_slice(suffix.as_bytes());
    Bytes::from(bytes)
}
