use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::sync::Notify;
use wokcore_core::id::{AccountId, ProviderId};

use crate::{
    accounts::{
        AccountAuthentication, AccountCandidate, AccountHealthTable, AccountObservation,
        AccountStateError, SelectionError,
    },
    retry::{MAX_TOTAL_ATTEMPTS, RetryClass, RetryPolicy},
};

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionRequestId(Arc<str>);

impl ExecutionRequestId {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidExecutionRequestId> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(InvalidExecutionRequestId);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ExecutionRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ExecutionRequestId")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the execution request identifier is invalid")]
pub struct InvalidExecutionRequestId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptId {
    request_id: ExecutionRequestId,
    ordinal: u8,
}

impl AttemptId {
    fn new(request_id: &ExecutionRequestId, ordinal: u8) -> Self {
        Self {
            request_id: request_id.clone(),
            ordinal,
        }
    }

    pub fn request_id(&self) -> &ExecutionRequestId {
        &self.request_id
    }

    pub const fn ordinal(&self) -> u8 {
        self.ordinal
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptBoundary {
    BeforeVisible,
    Visible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptFailureKind {
    RateLimited,
    Server,
    Timeout,
    Reset,
    InvalidCredentials,
    InvalidRequest,
    Policy,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptFailure {
    kind: AttemptFailureKind,
    status_code: Option<u16>,
    retry_after_ms: Option<u64>,
}

impl AttemptFailure {
    pub fn new(
        kind: AttemptFailureKind,
        status_code: Option<u16>,
        retry_after_ms: Option<u64>,
    ) -> Self {
        Self {
            kind,
            status_code: status_code.filter(|status| (100..=599).contains(status)),
            retry_after_ms,
        }
    }

    pub const fn kind(self) -> AttemptFailureKind {
        self.kind
    }

    pub const fn status_code(self) -> Option<u16> {
        self.status_code
    }

    pub const fn retry_after_ms(self) -> Option<u64> {
        self.retry_after_ms
    }

    fn retry_class(self) -> RetryClass {
        match self.kind {
            AttemptFailureKind::RateLimited => RetryClass::RateLimited,
            AttemptFailureKind::Server
            | AttemptFailureKind::Timeout
            | AttemptFailureKind::Reset => RetryClass::Temporary,
            AttemptFailureKind::InvalidCredentials
            | AttemptFailureKind::InvalidRequest
            | AttemptFailureKind::Policy
            | AttemptFailureKind::Other => RetryClass::Never,
        }
    }
}

#[derive(Debug)]
pub enum AttemptResult<T> {
    Succeeded {
        output: T,
        boundary: AttemptBoundary,
    },
    Failed {
        failure: AttemptFailure,
        boundary: AttemptBoundary,
    },
}

#[derive(Clone)]
pub struct PreparedRequestBody {
    bytes: Arc<[u8]>,
}

impl PreparedRequestBody {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    pub fn from_arc(bytes: Arc<[u8]>) -> Self {
        Self { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for PreparedRequestBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRequestBody")
            .field("length", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionCandidate {
    account_id: AccountId,
    authentication: AccountAuthentication,
    weight: u16,
}

impl ExecutionCandidate {
    pub fn new(
        account_id: AccountId,
        authentication: AccountAuthentication,
        weight: u16,
    ) -> Result<Self, AccountStateError> {
        AccountCandidate::new(&account_id, authentication, weight)?;
        Ok(Self {
            account_id,
            authentication,
            weight,
        })
    }

    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub const fn authentication(&self) -> AccountAuthentication {
        self.authentication
    }

    pub const fn weight(&self) -> u16 {
        self.weight
    }

    fn as_account_candidate(&self) -> AccountCandidate<'_> {
        AccountCandidate::new(&self.account_id, self.authentication, self.weight)
            .expect("ExecutionCandidate is validated at construction")
    }
}

pub struct ExecutionRequest<'a> {
    request_id: ExecutionRequestId,
    provider_id: &'a ProviderId,
    model: &'a str,
    candidates: &'a [ExecutionCandidate],
    authentication: AccountAuthentication,
    affinity_account: Option<&'a AccountId>,
    body: PreparedRequestBody,
}

impl<'a> ExecutionRequest<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: ExecutionRequestId,
        provider_id: &'a ProviderId,
        model: &'a str,
        candidates: &'a [ExecutionCandidate],
        authentication: AccountAuthentication,
        affinity_account: Option<&'a AccountId>,
        body: PreparedRequestBody,
    ) -> Self {
        Self {
            request_id,
            provider_id,
            model,
            candidates,
            authentication,
            affinity_account,
            body,
        }
    }
}

impl fmt::Debug for ExecutionRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionRequest")
            .field("request_id", &self.request_id)
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .field("candidate_count", &self.candidates.len())
            .field("authentication", &self.authentication)
            .field("has_affinity", &self.affinity_account.is_some())
            .field("body_length", &self.body.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct AttemptContext<'a> {
    request_id: &'a ExecutionRequestId,
    attempt_id: AttemptId,
    provider_id: &'a ProviderId,
    account_id: &'a AccountId,
    model: &'a str,
}

impl<'a> AttemptContext<'a> {
    pub fn request_id(&self) -> &ExecutionRequestId {
        self.request_id
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn provider_id(&self) -> &ProviderId {
        self.provider_id
    }

    pub fn account_id(&self) -> &AccountId {
        self.account_id
    }

    pub fn model(&self) -> &str {
        self.model
    }
}

#[async_trait]
pub trait UpstreamAttempt: Send + Sync + 'static {
    type Output: Send + 'static;

    async fn execute(
        &self,
        context: AttemptContext<'_>,
        body: &[u8],
        cancellation: &ExecutionCancellation,
    ) -> AttemptResult<Self::Output>;
}

pub trait ExecutionClock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemExecutionClock;

impl ExecutionClock for SystemExecutionClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

#[async_trait]
pub trait RetryDelay: Send + Sync + 'static {
    async fn wait(&self, delay_ms: u64, cancellation: &ExecutionCancellation) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioRetryDelay;

#[async_trait]
impl RetryDelay for TokioRetryDelay {
    async fn wait(&self, delay_ms: u64, cancellation: &ExecutionCancellation) -> bool {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => false,
            () = tokio::time::sleep(Duration::from_millis(delay_ms)) => true,
        }
    }
}

#[derive(Clone, Default)]
pub struct ExecutionCancellation {
    inner: Arc<ExecutionCancellationInner>,
}

#[derive(Default)]
struct ExecutionCancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl ExecutionCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for ExecutionCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

pub struct ExecutionCoordinator<A, D, C> {
    upstream: Arc<A>,
    delay: Arc<D>,
    clock: Arc<C>,
    policy: RetryPolicy,
}

impl<A, D, C> ExecutionCoordinator<A, D, C>
where
    A: UpstreamAttempt,
    D: RetryDelay,
    C: ExecutionClock,
{
    pub fn new(upstream: Arc<A>, delay: Arc<D>, clock: Arc<C>, policy: RetryPolicy) -> Self {
        Self {
            upstream,
            delay,
            clock,
            policy,
        }
    }

    pub async fn execute(
        &self,
        request: ExecutionRequest<'_>,
        health: &AccountHealthTable,
        cancellation: ExecutionCancellation,
    ) -> ExecutionOutcome<A::Output> {
        let mut history = AttemptHistory::default();
        if request.body.len() > self.policy.maximum_body_bytes() {
            return ExecutionOutcome::Failed(ExecutionFailure {
                terminal: ExecutionTerminal::RequestBodyTooLarge,
                history,
            });
        }

        for ordinal in 1..=MAX_TOTAL_ATTEMPTS {
            if cancellation.is_cancelled() {
                return cancelled(history);
            }
            let now_ms = self.clock.now_ms();
            let choice = match health.select_from(
                request
                    .candidates
                    .iter()
                    .map(ExecutionCandidate::as_account_candidate),
                request.authentication,
                request.affinity_account,
                now_ms,
            ) {
                Ok(choice) => choice,
                Err(SelectionError::NoEligibleAccount | SelectionError::AccountState(_)) => {
                    return ExecutionOutcome::Failed(ExecutionFailure {
                        terminal: ExecutionTerminal::NoEligibleAccount,
                        history,
                    });
                }
            };
            let attempt_id = AttemptId::new(&request.request_id, ordinal);
            let context = AttemptContext {
                request_id: &request.request_id,
                attempt_id: attempt_id.clone(),
                provider_id: request.provider_id,
                account_id: choice.account_id(),
                model: request.model,
            };
            let attempt = self
                .upstream
                .execute(context, request.body.as_slice(), &cancellation);
            tokio::pin!(attempt);
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => return cancelled(history),
                result = &mut attempt => result,
            };

            match result {
                AttemptResult::Succeeded { output, boundary } => {
                    let _ =
                        health.observe(choice.account_id(), AccountObservation::Success, now_ms);
                    history.push(AttemptDiagnostic::success(
                        &request,
                        choice.account_id(),
                        attempt_id,
                        boundary,
                    ));
                    return ExecutionOutcome::Succeeded(ExecutionSuccess {
                        output,
                        boundary,
                        history,
                    });
                }
                AttemptResult::Failed { failure, boundary } => {
                    observe_failure(health, choice.account_id(), failure, now_ms);
                    history.push(AttemptDiagnostic::failure(
                        &request,
                        choice.account_id(),
                        attempt_id,
                        failure,
                        boundary,
                    ));
                    let retry_delay = (boundary == AttemptBoundary::BeforeVisible
                        && ordinal < MAX_TOTAL_ATTEMPTS)
                        .then(|| {
                            self.policy
                                .delay_ms(failure.retry_class(), failure.retry_after_ms())
                        })
                        .flatten();
                    let Some(retry_delay) = retry_delay else {
                        return ExecutionOutcome::Failed(ExecutionFailure {
                            terminal: ExecutionTerminal::Upstream { failure, boundary },
                            history,
                        });
                    };
                    let wait = self.delay.wait(retry_delay, &cancellation);
                    tokio::pin!(wait);
                    let completed = tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return cancelled(history),
                        completed = &mut wait => completed,
                    };
                    if !completed || cancellation.is_cancelled() {
                        return cancelled(history);
                    }
                }
            }
        }
        unreachable!("the fixed attempt loop always returns")
    }
}

fn observe_failure(
    health: &AccountHealthTable,
    account_id: &AccountId,
    failure: AttemptFailure,
    now_ms: u64,
) {
    let observation = match failure.kind() {
        AttemptFailureKind::RateLimited => Some(AccountObservation::RateLimited {
            retry_after_ms: failure.retry_after_ms(),
        }),
        AttemptFailureKind::Server | AttemptFailureKind::Timeout | AttemptFailureKind::Reset => {
            Some(AccountObservation::TemporaryFailure {
                retry_after_ms: failure.retry_after_ms(),
            })
        }
        AttemptFailureKind::InvalidCredentials => Some(AccountObservation::InvalidCredentials),
        AttemptFailureKind::InvalidRequest
        | AttemptFailureKind::Policy
        | AttemptFailureKind::Other => None,
    };
    if let Some(observation) = observation {
        let _ = health.observe(account_id, observation, now_ms);
    }
}

fn cancelled<T>(history: AttemptHistory) -> ExecutionOutcome<T> {
    ExecutionOutcome::Failed(ExecutionFailure {
        terminal: ExecutionTerminal::Cancelled,
        history,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttemptDiagnosticOutcome {
    Succeeded {
        boundary: AttemptBoundary,
    },
    Failed {
        kind: AttemptFailureKind,
        status_code: Option<u16>,
        boundary: AttemptBoundary,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptDiagnostic {
    request_id: ExecutionRequestId,
    attempt_id: AttemptId,
    provider_id: ProviderId,
    account_id: AccountId,
    outcome: AttemptDiagnosticOutcome,
}

impl AttemptDiagnostic {
    fn success(
        request: &ExecutionRequest<'_>,
        account_id: &AccountId,
        attempt_id: AttemptId,
        boundary: AttemptBoundary,
    ) -> Self {
        Self {
            request_id: request.request_id.clone(),
            attempt_id,
            provider_id: request.provider_id.clone(),
            account_id: account_id.clone(),
            outcome: AttemptDiagnosticOutcome::Succeeded { boundary },
        }
    }

    fn failure(
        request: &ExecutionRequest<'_>,
        account_id: &AccountId,
        attempt_id: AttemptId,
        failure: AttemptFailure,
        boundary: AttemptBoundary,
    ) -> Self {
        Self {
            request_id: request.request_id.clone(),
            attempt_id,
            provider_id: request.provider_id.clone(),
            account_id: account_id.clone(),
            outcome: AttemptDiagnosticOutcome::Failed {
                kind: failure.kind(),
                status_code: failure.status_code(),
                boundary,
            },
        }
    }

    pub fn request_id(&self) -> &ExecutionRequestId {
        &self.request_id
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub fn outcome(&self) -> &AttemptDiagnosticOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttemptHistory {
    entries: [Option<AttemptDiagnostic>; MAX_TOTAL_ATTEMPTS as usize],
    length: u8,
}

impl AttemptHistory {
    fn push(&mut self, diagnostic: AttemptDiagnostic) {
        let index = usize::from(self.length);
        if index < self.entries.len() {
            self.entries[index] = Some(diagnostic);
            self.length += 1;
        }
    }

    pub const fn len(&self) -> usize {
        self.length as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &AttemptDiagnostic> {
        self.entries[..self.len()]
            .iter()
            .map(|entry| entry.as_ref().expect("history prefix is initialized"))
    }
}

#[derive(Debug)]
pub enum ExecutionOutcome<T> {
    Succeeded(ExecutionSuccess<T>),
    Failed(ExecutionFailure),
}

impl<T> ExecutionOutcome<T> {
    pub fn history(&self) -> &AttemptHistory {
        match self {
            Self::Succeeded(success) => success.history(),
            Self::Failed(failure) => failure.history(),
        }
    }
}

#[derive(Debug)]
pub struct ExecutionSuccess<T> {
    output: T,
    boundary: AttemptBoundary,
    history: AttemptHistory,
}

impl<T> ExecutionSuccess<T> {
    pub fn output(&self) -> &T {
        &self.output
    }

    pub const fn boundary(&self) -> AttemptBoundary {
        self.boundary
    }

    pub fn history(&self) -> &AttemptHistory {
        &self.history
    }

    pub fn into_output(self) -> T {
        self.output
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFailure {
    terminal: ExecutionTerminal,
    history: AttemptHistory,
}

impl ExecutionFailure {
    pub fn terminal(&self) -> &ExecutionTerminal {
        &self.terminal
    }

    pub fn history(&self) -> &AttemptHistory {
        &self.history
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionTerminal {
    Cancelled,
    RequestBodyTooLarge,
    NoEligibleAccount,
    Upstream {
        failure: AttemptFailure,
        boundary: AttemptBoundary,
    },
}
