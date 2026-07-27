use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use tokio::sync::Notify;
use wokcore_core::id::{AccountId, ProviderId};
use wokcore_engine::{
    accounts::{AccountAuthentication, AccountHealthPolicy, AccountHealthTable, AccountStatusKind},
    execution::{
        AttemptBoundary, AttemptContext, AttemptFailure, AttemptFailureKind, AttemptResult,
        ExecutionCancellation, ExecutionCandidate, ExecutionClock, ExecutionCoordinator,
        ExecutionOutcome, ExecutionRequest, ExecutionRequestId, ExecutionTerminal,
        PreparedRequestBody, RetryDelay, UpstreamAttempt,
    },
    retry::RetryPolicy,
};

#[tokio::test]
async fn coordinator_attempts_at_most_twice_before_visibility() {
    let fixture = Fixture::new(&["first", "second"]);
    let upstream = Arc::new(ScriptedAttempt::new([
        failure(AttemptFailureKind::Server, AttemptBoundary::BeforeVisible),
        failure(AttemptFailureKind::Reset, AttemptBoundary::BeforeVisible),
        success("forbidden-third"),
    ]));
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));

    let outcome = coordinator
        .execute(
            fixture.request("request-two-attempts", b"synthetic"),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;

    assert!(matches!(
        outcome,
        ExecutionOutcome::Failed(ref failure)
            if matches!(failure.terminal(), ExecutionTerminal::Upstream { .. })
    ));
    assert_eq!(upstream.calls(), 2);
    assert_eq!(outcome.history().len(), 2);
}

#[tokio::test]
async fn visible_failure_never_retries_or_fails_over() {
    let fixture = Fixture::new(&["first", "second"]);
    let upstream = Arc::new(ScriptedAttempt::new([
        failure(AttemptFailureKind::Server, AttemptBoundary::Visible),
        success("forbidden-retry"),
    ]));
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));

    let outcome = coordinator
        .execute(
            fixture.request("request-visible", b"synthetic"),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;

    assert_eq!(upstream.calls(), 1);
    assert!(matches!(
        outcome,
        ExecutionOutcome::Failed(ref failure)
            if matches!(
                failure.terminal(),
                ExecutionTerminal::Upstream {
                    boundary: AttemptBoundary::Visible,
                    ..
                }
            )
    ));
}

#[tokio::test]
async fn cancellation_drops_the_current_attempt_and_prevents_another() {
    let fixture = Fixture::new(&["first", "second"]);
    let upstream = Arc::new(BlockingAttempt::default());
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));
    let cancellation = ExecutionCancellation::new();
    let cancel = cancellation.clone();
    let started = Arc::clone(&upstream.started);
    let request = fixture.request("request-cancel", b"synthetic");

    let (outcome, ()) = tokio::join!(
        coordinator.execute(request, &fixture.health, cancellation),
        async move {
            started.notified().await;
            cancel.cancel();
        }
    );

    assert!(matches!(
        outcome,
        ExecutionOutcome::Failed(ref failure)
            if failure.terminal() == &ExecutionTerminal::Cancelled
    ));
    assert_eq!(upstream.calls.load(Ordering::Acquire), 1);
    assert!(upstream.dropped.load(Ordering::Acquire));
    assert_eq!(outcome.history().len(), 0);
}

#[tokio::test]
async fn non_retryable_failures_never_cross_authentication_types() {
    for kind in [
        AttemptFailureKind::InvalidCredentials,
        AttemptFailureKind::InvalidRequest,
        AttemptFailureKind::Policy,
    ] {
        let fixture = Fixture::new_mixed_auth();
        let upstream = Arc::new(ScriptedAttempt::new([
            failure(kind, AttemptBoundary::BeforeVisible),
            success("forbidden-cross-auth"),
        ]));
        let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));

        let outcome = coordinator
            .execute(
                fixture.request_with_auth(
                    "request-non-retryable",
                    b"synthetic",
                    AccountAuthentication::Oauth,
                ),
                &fixture.health,
                ExecutionCancellation::new(),
            )
            .await;

        assert!(matches!(outcome, ExecutionOutcome::Failed(_)));
        assert_eq!(upstream.calls(), 1);
        assert_eq!(upstream.accounts(), ["first"]);
    }
}

#[tokio::test]
async fn retryable_failure_updates_health_and_uses_a_distinct_account() {
    let fixture = Fixture::new(&["first", "second"]);
    let upstream = Arc::new(ScriptedAttempt::new([
        AttemptResult::Failed {
            failure: AttemptFailure::new(AttemptFailureKind::RateLimited, Some(429), Some(250)),
            boundary: AttemptBoundary::BeforeVisible,
        },
        success("ok"),
    ]));
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));

    let outcome = coordinator
        .execute(
            fixture.request("request-failover", b"synthetic"),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;

    assert!(matches!(
        outcome,
        ExecutionOutcome::Succeeded(ref success) if success.output() == &"ok"
    ));
    assert_eq!(upstream.accounts(), ["first", "second"]);
    assert_eq!(
        fixture
            .health
            .status(&account("first"), 0)
            .expect("status")
            .kind(),
        AccountStatusKind::CoolingDown
    );
}

#[tokio::test]
async fn retry_delay_is_bounded_and_cancellation_safe() {
    let fixture = Fixture::new(&["first", "second"]);
    let upstream = Arc::new(ScriptedAttempt::new([
        AttemptResult::Failed {
            failure: AttemptFailure::new(
                AttemptFailureKind::RateLimited,
                Some(429),
                Some(u64::MAX),
            ),
            boundary: AttemptBoundary::BeforeVisible,
        },
        success("forbidden-after-cancel"),
    ]));
    let delay = Arc::new(BlockingDelay::default());
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::clone(&delay));
    let cancellation = ExecutionCancellation::new();
    let cancel = cancellation.clone();
    let waiting = Arc::clone(&delay.waiting);

    let (outcome, ()) = tokio::join!(
        coordinator.execute(
            fixture.request("request-delay-cancel", b"synthetic"),
            &fixture.health,
            cancellation,
        ),
        async move {
            waiting.notified().await;
            cancel.cancel();
        }
    );

    assert!(matches!(
        outcome,
        ExecutionOutcome::Failed(ref failure)
            if failure.terminal() == &ExecutionTerminal::Cancelled
    ));
    assert_eq!(delay.observed_ms.load(Ordering::Acquire), 1_000);
    assert_eq!(upstream.calls(), 1);
}

#[tokio::test]
async fn request_body_is_bounded_and_released_after_the_pre_visible_window() {
    let fixture = Fixture::new(&["first"]);
    let upstream = Arc::new(ScriptedAttempt::new([success("ok")]));
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));
    let bytes: Arc<[u8]> = Arc::from(b"body-release-sentinel".as_slice());
    let weak = Arc::downgrade(&bytes);
    let body = PreparedRequestBody::from_arc(Arc::clone(&bytes));
    drop(bytes);

    let outcome = coordinator
        .execute(
            fixture.request_with_body("request-body", body),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;

    assert!(matches!(outcome, ExecutionOutcome::Succeeded(_)));
    assert!(weak.upgrade().is_none());
    assert_eq!(
        PreparedRequestBody::new(vec![0; 1_025]).len(),
        1_025,
        "body construction itself is side-effect free; policy rejects it"
    );
    let oversized = coordinator
        .execute(
            fixture.request("request-too-large", &vec![0; 1_025]),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;
    assert!(matches!(
        oversized,
        ExecutionOutcome::Failed(ref failure)
            if failure.terminal() == &ExecutionTerminal::RequestBodyTooLarge
    ));
}

#[tokio::test]
async fn diagnostics_are_fixed_size_stable_and_content_free() {
    let account_ids = (0..100)
        .map(|index| format!("account-{index}"))
        .collect::<Vec<_>>();
    let refs = account_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let fixture = Fixture::new(&refs);
    let upstream = Arc::new(ScriptedAttempt::new([
        failure(AttemptFailureKind::Timeout, AttemptBoundary::BeforeVisible),
        failure(AttemptFailureKind::Server, AttemptBoundary::BeforeVisible),
    ]));
    let coordinator = fixture.coordinator(Arc::clone(&upstream), Arc::new(ImmediateDelay));

    let outcome = coordinator
        .execute(
            fixture.request(
                "request-stable-id",
                b"prompt-and-secret-must-not-enter-diagnostics",
            ),
            &fixture.health,
            ExecutionCancellation::new(),
        )
        .await;
    let rendered = format!("{:?}", outcome.history());

    assert_eq!(outcome.history().len(), 2);
    assert!(rendered.contains("request-stable-id"));
    assert!(rendered.contains("ordinal: 1"));
    assert!(rendered.contains("ordinal: 2"));
    assert!(!rendered.contains("prompt-and-secret"));
    assert!(!rendered.contains("authorization"));

    let execution_source = include_str!("../src/execution.rs");
    let retry_source = include_str!("../src/retry.rs");
    for forbidden in ["Semaphore", "mpsc::channel", "concurrency_limit"] {
        assert!(!execution_source.contains(forbidden));
        assert!(!retry_source.contains(forbidden));
    }
    assert!(!execution_source.contains("candidates: Vec<AccountCandidate"));
}

struct Fixture {
    provider_id: ProviderId,
    candidates: Vec<ExecutionCandidate>,
    health: AccountHealthTable,
    clock: Arc<TestClock>,
}

impl Fixture {
    fn new(accounts: &[&str]) -> Self {
        let account_ids = accounts
            .iter()
            .map(|value| account(value))
            .collect::<Vec<_>>();
        Self {
            provider_id: provider("provider"),
            candidates: account_ids
                .iter()
                .map(|id| {
                    ExecutionCandidate::new(id.clone(), AccountAuthentication::ApiKey, 1)
                        .expect("candidate")
                })
                .collect(),
            health: AccountHealthTable::new(policy(), &account_ids).expect("health"),
            clock: Arc::new(TestClock::default()),
        }
    }

    fn new_mixed_auth() -> Self {
        let mut fixture = Self::new(&["first", "second"]);
        fixture.candidates = vec![
            ExecutionCandidate::new(account("first"), AccountAuthentication::Oauth, 1)
                .expect("candidate"),
            ExecutionCandidate::new(account("second"), AccountAuthentication::ApiKey, 1)
                .expect("candidate"),
        ];
        fixture
    }

    fn coordinator<A, D>(
        &self,
        upstream: Arc<A>,
        delay: Arc<D>,
    ) -> ExecutionCoordinator<A, D, TestClock>
    where
        A: UpstreamAttempt<Output = &'static str>,
        D: RetryDelay,
    {
        ExecutionCoordinator::new(
            upstream,
            delay,
            Arc::clone(&self.clock),
            RetryPolicy::new(100, 1_000, 1_024).expect("policy"),
        )
    }

    fn request<'a>(&'a self, id: &str, body: &[u8]) -> ExecutionRequest<'a> {
        self.request_with_body(id, PreparedRequestBody::new(body.to_vec()))
    }

    fn request_with_body<'a>(
        &'a self,
        id: &str,
        body: PreparedRequestBody,
    ) -> ExecutionRequest<'a> {
        self.request_with_auth_and_body(id, body, AccountAuthentication::ApiKey)
    }

    fn request_with_auth<'a>(
        &'a self,
        id: &str,
        body: &[u8],
        authentication: AccountAuthentication,
    ) -> ExecutionRequest<'a> {
        self.request_with_auth_and_body(id, PreparedRequestBody::new(body.to_vec()), authentication)
    }

    fn request_with_auth_and_body<'a>(
        &'a self,
        id: &str,
        body: PreparedRequestBody,
        authentication: AccountAuthentication,
    ) -> ExecutionRequest<'a> {
        ExecutionRequest::new(
            ExecutionRequestId::new(id).expect("request ID"),
            &self.provider_id,
            "model",
            &self.candidates,
            authentication,
            None,
            body,
        )
    }
}

#[derive(Default)]
struct TestClock {
    now_ms: AtomicU64,
}

impl ExecutionClock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::Acquire)
    }
}

struct ScriptedAttempt {
    results: Mutex<VecDeque<AttemptResult<&'static str>>>,
    calls: AtomicUsize,
    accounts: Mutex<Vec<String>>,
}

impl ScriptedAttempt {
    fn new(results: impl IntoIterator<Item = AttemptResult<&'static str>>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
            calls: AtomicUsize::new(0),
            accounts: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn accounts(&self) -> Vec<String> {
        self.accounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl UpstreamAttempt for ScriptedAttempt {
    type Output = &'static str;

    async fn execute(
        &self,
        context: AttemptContext<'_>,
        _body: &[u8],
        _cancellation: &ExecutionCancellation,
    ) -> AttemptResult<Self::Output> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.accounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context.account_id().as_str().to_owned());
        self.results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .expect("scripted result")
    }
}

#[derive(Default)]
struct BlockingAttempt {
    started: Arc<Notify>,
    calls: AtomicUsize,
    dropped: Arc<AtomicBool>,
}

struct DropMarker(Arc<AtomicBool>);

impl Drop for DropMarker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait]
impl UpstreamAttempt for BlockingAttempt {
    type Output = &'static str;

    async fn execute(
        &self,
        _context: AttemptContext<'_>,
        _body: &[u8],
        _cancellation: &ExecutionCancellation,
    ) -> AttemptResult<Self::Output> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let _marker = DropMarker(Arc::clone(&self.dropped));
        self.started.notify_one();
        pending().await
    }
}

struct ImmediateDelay;

#[async_trait]
impl RetryDelay for ImmediateDelay {
    async fn wait(&self, _delay_ms: u64, cancellation: &ExecutionCancellation) -> bool {
        !cancellation.is_cancelled()
    }
}

#[derive(Default)]
struct BlockingDelay {
    waiting: Arc<Notify>,
    observed_ms: AtomicU64,
}

#[async_trait]
impl RetryDelay for BlockingDelay {
    async fn wait(&self, delay_ms: u64, cancellation: &ExecutionCancellation) -> bool {
        self.observed_ms.store(delay_ms, Ordering::Release);
        self.waiting.notify_one();
        cancellation.cancelled().await;
        false
    }
}

fn success(output: &'static str) -> AttemptResult<&'static str> {
    AttemptResult::Succeeded {
        output,
        boundary: AttemptBoundary::BeforeVisible,
    }
}

fn failure(kind: AttemptFailureKind, boundary: AttemptBoundary) -> AttemptResult<&'static str> {
    AttemptResult::Failed {
        failure: AttemptFailure::new(kind, None, None),
        boundary,
    }
}

fn policy() -> AccountHealthPolicy {
    AccountHealthPolicy::new(100, 1_000).expect("health policy")
}

fn provider(value: &str) -> ProviderId {
    ProviderId::new(value).expect("provider")
}

fn account(value: &str) -> AccountId {
    AccountId::new(value).expect("account")
}
