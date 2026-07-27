use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use bytes::Bytes;
use reqwest::{
    Client, Response, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    redirect,
};
use secrecy::{ExposeSecret, SecretString};
use tokio::time::Instant;

use crate::execution::ExecutionCancellation;

const MAX_REQUEST_HEADERS: usize = 16;
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const DEFAULT_POOL_IDLE_CONNECTIONS_PER_HOST: usize = 8;
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const ORDINARY_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "anthropic-beta",
    "anthropic-version",
    "content-type",
    "openai-beta",
    "x-client-request-id",
];

const SENSITIVE_REQUEST_HEADERS: &[&str] =
    &["api-key", "authorization", "x-api-key", "x-goog-api-key"];

const OBSERVED_RESPONSE_HEADERS: &[&str] = &[
    "apim-request-id",
    "content-type",
    "request-id",
    "retry-after",
    "x-goog-request-id",
    "x-request-id",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPolicy {
    PublicOnly,
    PrivateAllowed,
    LoopbackOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportTimeouts {
    pub connect: Duration,
    pub response_headers: Duration,
    pub idle_stream: Duration,
    pub non_stream_total: Duration,
}

impl Default for TransportTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            response_headers: Duration::from_secs(30),
            idle_stream: Duration::from_secs(60),
            non_stream_total: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLimits {
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
    pub max_response_headers: usize,
    pub max_response_header_bytes: usize,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_request_body_bytes: 16 * 1024 * 1024,
            max_response_body_bytes: 16 * 1024 * 1024,
            max_response_headers: 128,
            max_response_header_bytes: 64 * 1024,
        }
    }
}

struct SensitiveHeader {
    name: HeaderName,
    value: SecretString,
}

impl fmt::Debug for SensitiveHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SensitiveHeader")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

pub struct TransportRequest {
    url: Url,
    headers: HeaderMap,
    sensitive_header: Option<SensitiveHeader>,
    body: Vec<u8>,
    stream: bool,
    network_policy: NetworkPolicy,
}

impl TransportRequest {
    pub fn post(url: Url, body: Vec<u8>, stream: bool, network_policy: NetworkPolicy) -> Self {
        Self {
            url,
            headers: HeaderMap::new(),
            sensitive_header: None,
            body,
            stream,
            network_policy,
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Result<Self, TransportError> {
        let name = parse_allowed_name(name, ORDINARY_REQUEST_HEADERS)?;
        if self.headers.len() >= MAX_REQUEST_HEADERS
            || self.headers.contains_key(&name)
            || header_bytes(&name, value.as_bytes()) > MAX_REQUEST_HEADER_BYTES
        {
            return Err(TransportError::new(TransportErrorKind::InvalidRequest));
        }
        let value = HeaderValue::from_str(value)
            .map_err(|_| TransportError::new(TransportErrorKind::InvalidRequest))?;
        self.headers.insert(name, value);
        Ok(self)
    }

    pub fn with_sensitive_header(
        mut self,
        name: &str,
        value: SecretString,
    ) -> Result<Self, TransportError> {
        let name = parse_allowed_name(name, SENSITIVE_REQUEST_HEADERS)?;
        if self.sensitive_header.is_some()
            || value.expose_secret().is_empty()
            || header_bytes(&name, value.expose_secret().as_bytes()) > MAX_REQUEST_HEADER_BYTES
            || HeaderValue::from_str(value.expose_secret()).is_err()
        {
            return Err(TransportError::new(TransportErrorKind::InvalidRequest));
        }
        self.sensitive_header = Some(SensitiveHeader { name, value });
        Ok(self)
    }
}

impl fmt::Debug for TransportRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportRequest")
            .field("url", &"[redacted]")
            .field("header_count", &self.headers.len())
            .field(
                "sensitive_header",
                &self.sensitive_header.as_ref().map(|_| "[redacted]"),
            )
            .field("body_bytes", &self.body.len())
            .field("stream", &self.stream)
            .field("network_policy", &self.network_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct PooledTransport {
    public_client: Client,
    private_client: Client,
    loopback_client: Client,
    timeouts: TransportTimeouts,
    limits: TransportLimits,
}

impl PooledTransport {
    pub fn new(
        timeouts: TransportTimeouts,
        limits: TransportLimits,
    ) -> Result<Self, TransportBuildError> {
        validate_configuration(timeouts, limits)?;
        Ok(Self {
            public_client: build_client(NetworkPolicy::PublicOnly, timeouts.connect)?,
            private_client: build_client(NetworkPolicy::PrivateAllowed, timeouts.connect)?,
            loopback_client: build_client(NetworkPolicy::LoopbackOnly, timeouts.connect)?,
            timeouts,
            limits,
        })
    }

    pub async fn execute(
        &self,
        request: TransportRequest,
        cancellation: &ExecutionCancellation,
    ) -> Result<TransportResponse, TransportError> {
        validate_request(&request, self.limits)?;
        if cancellation.is_cancelled() {
            return Err(TransportError::new(TransportErrorKind::Cancelled));
        }

        let started_at = Instant::now();
        let total_deadline = started_at + self.timeouts.non_stream_total;
        let client = match request.network_policy {
            NetworkPolicy::PublicOnly => &self.public_client,
            NetworkPolicy::PrivateAllowed => &self.private_client,
            NetworkPolicy::LoopbackOnly => &self.loopback_client,
        };
        let mut builder = client
            .post(request.url)
            .headers(request.headers)
            .header(header::ACCEPT_ENCODING, "identity")
            .body(request.body);
        if let Some(sensitive) = request.sensitive_header {
            let value = HeaderValue::from_str(sensitive.value.expose_secret())
                .map_err(|_| TransportError::new(TransportErrorKind::InvalidRequest))?;
            builder = builder.header(sensitive.name, value);
        }

        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(TransportError::new(TransportErrorKind::Cancelled));
            }
            response = tokio::time::timeout(self.timeouts.response_headers, builder.send()) => {
                match response {
                    Err(_) => return Err(TransportError::new(TransportErrorKind::HeaderTimeout)),
                    Ok(Err(error)) => return Err(map_reqwest_error(&error)),
                    Ok(Ok(response)) => response,
                }
            }
        };
        let head = validate_response_head(&response, self.limits)?;

        if request.stream {
            return Ok(TransportResponse::Streaming(StreamingTransportResponse {
                head,
                response: Some(response),
                received_bytes: 0,
                maximum_body_bytes: self.limits.max_response_body_bytes,
                idle_timeout: self.timeouts.idle_stream,
            }));
        }

        let body = read_complete_body(
            response,
            cancellation,
            total_deadline,
            self.limits.max_response_body_bytes,
        )
        .await?;
        Ok(TransportResponse::Complete(CompleteTransportResponse {
            head,
            body,
        }))
    }
}

impl fmt::Debug for PooledTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PooledTransport")
            .field("timeouts", &self.timeouts)
            .field("limits", &self.limits)
            .field("policy_client_count", &3)
            .finish()
    }
}

#[derive(Debug)]
pub enum TransportResponse {
    Complete(CompleteTransportResponse),
    Streaming(StreamingTransportResponse),
}

pub struct CompleteTransportResponse {
    head: TransportResponseHead,
    body: Vec<u8>,
}

impl CompleteTransportResponse {
    pub const fn head(&self) -> &TransportResponseHead {
        &self.head
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for CompleteTransportResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompleteTransportResponse")
            .field("head", &self.head)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

pub struct StreamingTransportResponse {
    head: TransportResponseHead,
    response: Option<Response>,
    received_bytes: usize,
    maximum_body_bytes: usize,
    idle_timeout: Duration,
}

impl StreamingTransportResponse {
    pub const fn head(&self) -> &TransportResponseHead {
        &self.head
    }

    pub fn is_closed(&self) -> bool {
        self.response.is_none()
    }

    pub async fn next_chunk(
        &mut self,
        cancellation: &ExecutionCancellation,
    ) -> Result<Option<Bytes>, TransportError> {
        let response = self
            .response
            .as_mut()
            .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidResponse))?;
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.response.take();
                return Err(TransportError::new(TransportErrorKind::Cancelled));
            }
            chunk = tokio::time::timeout(self.idle_timeout, response.chunk()) => {
                match chunk {
                    Err(_) => {
                        self.response.take();
                        return Err(TransportError::new(TransportErrorKind::IdleTimeout));
                    }
                    Ok(Err(error)) => {
                        self.response.take();
                        return Err(map_reqwest_error(&error));
                    }
                    Ok(Ok(chunk)) => chunk,
                }
            }
        };
        let Some(chunk) = chunk else {
            self.response.take();
            return Ok(None);
        };
        self.received_bytes = self.received_bytes.saturating_add(chunk.len());
        if self.received_bytes > self.maximum_body_bytes {
            self.response.take();
            return Err(TransportError::new(TransportErrorKind::ResponseTooLarge));
        }
        Ok(Some(chunk))
    }
}

impl fmt::Debug for StreamingTransportResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingTransportResponse")
            .field("head", &self.head)
            .field("received_bytes", &self.received_bytes)
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportResponseHead {
    status: u16,
    headers: BTreeMap<String, String>,
}

impl TransportResponseHead {
    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    InvalidRequest,
    Policy,
    Cancelled,
    ConnectTimeout,
    HeaderTimeout,
    IdleTimeout,
    TotalTimeout,
    ResponseTooLarge,
    InvalidResponse,
    Transport,
}

#[derive(Clone, Copy, Eq, PartialEq, thiserror::Error)]
#[error("upstream transport failed ({kind:?})")]
pub struct TransportError {
    kind: TransportErrorKind,
}

impl TransportError {
    const fn new(kind: TransportErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(self) -> TransportErrorKind {
        self.kind
    }
}

impl fmt::Debug for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportError")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the upstream transport configuration is invalid")]
pub struct TransportBuildError;

fn validate_configuration(
    timeouts: TransportTimeouts,
    limits: TransportLimits,
) -> Result<(), TransportBuildError> {
    if timeouts.connect.is_zero()
        || timeouts.response_headers.is_zero()
        || timeouts.idle_stream.is_zero()
        || timeouts.non_stream_total.is_zero()
        || limits.max_request_body_bytes == 0
        || limits.max_response_body_bytes == 0
        || limits.max_response_headers == 0
        || limits.max_response_header_bytes == 0
    {
        return Err(TransportBuildError);
    }
    Ok(())
}

fn build_client(
    network_policy: NetworkPolicy,
    connect_timeout: Duration,
) -> Result<Client, TransportBuildError> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(DEFAULT_POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(DEFAULT_POOL_IDLE_CONNECTIONS_PER_HOST)
        .tcp_nodelay(true)
        .user_agent(concat!("wokcore/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(GuardedDnsResolver { network_policy })
        .build()
        .map_err(|_| TransportBuildError)
}

fn validate_request(
    request: &TransportRequest,
    limits: TransportLimits,
) -> Result<(), TransportError> {
    if request.body.len() > limits.max_request_body_bytes
        || request.headers.len() > MAX_REQUEST_HEADERS
        || request.headers.contains_key(header::ACCEPT_ENCODING)
        || !request.url.username().is_empty()
        || request.url.password().is_some()
        || request.url.fragment().is_some()
        || request.url.port() == Some(0)
    {
        return Err(TransportError::new(TransportErrorKind::InvalidRequest));
    }
    validate_url_policy(&request.url, request.network_policy)
}

fn validate_url_policy(url: &Url, policy: NetworkPolicy) -> Result<(), TransportError> {
    let Some(host) = url.host() else {
        return Err(TransportError::new(TransportErrorKind::InvalidRequest));
    };
    match policy {
        NetworkPolicy::PublicOnly => {
            if url.scheme() != "https" || !host_is_public(host) {
                return Err(TransportError::new(TransportErrorKind::Policy));
            }
        }
        NetworkPolicy::PrivateAllowed => {
            if !matches!(url.scheme(), "http" | "https") || !host_is_connectable(host) {
                return Err(TransportError::new(TransportErrorKind::Policy));
            }
        }
        NetworkPolicy::LoopbackOnly => {
            if !matches!(url.scheme(), "http" | "https") || !host_is_loopback(host) {
                return Err(TransportError::new(TransportErrorKind::Policy));
            }
        }
    }
    Ok(())
}

fn host_is_public(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => !domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => ip_is_public(IpAddr::V4(address)),
        url::Host::Ipv6(address) => ip_is_public(IpAddr::V6(address)),
    }
}

fn host_is_connectable(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => !domain.is_empty(),
        url::Host::Ipv4(address) => ip_is_connectable(IpAddr::V4(address)),
        url::Host::Ipv6(address) => ip_is_connectable(IpAddr::V6(address)),
    }
}

fn host_is_loopback(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    }
}

fn ip_is_connectable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => !address.is_unspecified() && !address.is_multicast(),
        IpAddr::V6(address) => !address.is_unspecified() && !address.is_multicast(),
    }
}

fn ip_is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_public(address),
        IpAddr::V6(address) => ipv6_is_public(address),
    }
}

fn ipv4_is_public(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_broadcast()
        && !address.is_documentation()
        && !address.is_unspecified()
        && !address.is_multicast()
        && octets[0] != 0
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 198 && matches!(octets[1], 18 | 19))
        && octets[0] < 240
}

fn ipv6_is_public(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
    {
        return false;
    }
    address.to_ipv4_mapped().is_none_or(ipv4_is_public)
}

fn parse_allowed_name(name: &str, allowed: &[&str]) -> Result<HeaderName, TransportError> {
    let name = name.to_ascii_lowercase();
    if !allowed.contains(&name.as_str()) {
        return Err(TransportError::new(TransportErrorKind::InvalidRequest));
    }
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| TransportError::new(TransportErrorKind::InvalidRequest))
}

fn header_bytes(name: &HeaderName, value: &[u8]) -> usize {
    name.as_str().len().saturating_add(value.len())
}

fn validate_response_head(
    response: &Response,
    limits: TransportLimits,
) -> Result<TransportResponseHead, TransportError> {
    let headers = response.headers();
    if headers.len() > limits.max_response_headers {
        return Err(TransportError::new(TransportErrorKind::InvalidResponse));
    }
    let mut total_header_bytes = 0usize;
    for (name, value) in headers {
        total_header_bytes = total_header_bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len());
    }
    if total_header_bytes > limits.max_response_header_bytes {
        return Err(TransportError::new(TransportErrorKind::InvalidResponse));
    }

    let mut encodings = headers.get_all(header::CONTENT_ENCODING).iter();
    if let Some(encoding) = encodings.next()
        && (encodings.next().is_some()
            || !encoding
                .to_str()
                .is_ok_and(|value| value.eq_ignore_ascii_case("identity")))
    {
        return Err(TransportError::new(TransportErrorKind::InvalidResponse));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limits.max_response_body_bytes as u64)
    {
        return Err(TransportError::new(TransportErrorKind::ResponseTooLarge));
    }

    let mut observed = BTreeMap::new();
    for name in OBSERVED_RESPONSE_HEADERS {
        if let Some(value) = headers.get(*name)
            && let Ok(value) = value.to_str()
        {
            observed.insert((*name).to_owned(), value.to_owned());
        }
    }
    Ok(TransportResponseHead {
        status: response.status().as_u16(),
        headers: observed,
    })
}

async fn read_complete_body(
    mut response: Response,
    cancellation: &ExecutionCancellation,
    total_deadline: Instant,
    maximum_body_bytes: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(maximum_body_bytes),
    );
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(TransportError::new(TransportErrorKind::Cancelled));
            }
            chunk = tokio::time::timeout_at(total_deadline, response.chunk()) => {
                match chunk {
                    Err(_) => return Err(TransportError::new(TransportErrorKind::TotalTimeout)),
                    Ok(Err(error)) => return Err(map_reqwest_error(&error)),
                    Ok(Ok(chunk)) => chunk,
                }
            }
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > maximum_body_bytes {
            return Err(TransportError::new(TransportErrorKind::ResponseTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
}

fn map_reqwest_error(error: &reqwest::Error) -> TransportError {
    let kind = if error.is_timeout() && error.is_connect() {
        TransportErrorKind::ConnectTimeout
    } else if error.is_timeout() {
        TransportErrorKind::HeaderTimeout
    } else {
        TransportErrorKind::Transport
    };
    TransportError::new(kind)
}

#[derive(Clone, Copy)]
struct GuardedDnsResolver {
    network_policy: NetworkPolicy,
}

impl Resolve for GuardedDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let network_policy = self.network_policy;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            if addresses.is_empty()
                || addresses
                    .iter()
                    .any(|address| !address_allowed(address.ip(), network_policy))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "resolved address rejected by network policy",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn address_allowed(address: IpAddr, policy: NetworkPolicy) -> bool {
    match policy {
        NetworkPolicy::PublicOnly => ip_is_public(address),
        NetworkPolicy::PrivateAllowed => ip_is_connectable(address),
        NetworkPolicy::LoopbackOnly => address.is_loopback(),
    }
}
