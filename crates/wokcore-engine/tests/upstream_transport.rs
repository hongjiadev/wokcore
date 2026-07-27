use std::{
    collections::BTreeSet,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use secrecy::SecretString;
use tokio::{net::TcpListener, sync::oneshot};
use url::Url;
use wokcore_engine::{
    execution::ExecutionCancellation,
    transport::{
        NetworkPolicy, PooledTransport, TransportErrorKind, TransportLimits, TransportRequest,
        TransportResponse, TransportTimeouts,
    },
};

const SECRET_CANARY: &str = "transport-secret-canary";

struct LoopbackServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl LoopbackServer {
    async fn start(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = receiver.await;
            })
            .await
            .unwrap();
        });
        Self {
            address,
            shutdown: Some(shutdown),
        }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, path)).unwrap()
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn transport(timeouts: TransportTimeouts, limits: TransportLimits) -> PooledTransport {
    PooledTransport::new(timeouts, limits).unwrap()
}

fn loopback_request(url: Url, stream: bool) -> TransportRequest {
    TransportRequest::post(
        url,
        br#"{"input":"offline"}"#.to_vec(),
        stream,
        NetworkPolicy::LoopbackOnly,
    )
    .with_header("content-type", "application/json")
    .unwrap()
}

async fn complete_body(transport: &PooledTransport, request: TransportRequest) -> Vec<u8> {
    match transport
        .execute(request, &ExecutionCancellation::new())
        .await
        .unwrap()
    {
        TransportResponse::Complete(response) => response.body().to_vec(),
        TransportResponse::Streaming(_) => panic!("expected a complete response"),
    }
}

#[tokio::test]
async fn transport_reuses_a_policy_compatible_connection_pool() {
    #[derive(Clone, Default)]
    struct Connections(Arc<Mutex<BTreeSet<SocketAddr>>>);

    async fn observe(
        State(connections): State<Connections>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ) -> &'static str {
        connections
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer);
        "ok"
    }

    let connections = Connections::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/pooled", post(observe))
            .with_state(connections.clone()),
    )
    .await;
    let transport = transport(TransportTimeouts::default(), TransportLimits::default());

    for _ in 0..2 {
        assert_eq!(
            complete_body(&transport, loopback_request(server.url("/pooled"), false)).await,
            b"ok"
        );
    }

    let unique_connections = connections
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    assert_eq!(unique_connections, 1);
}

#[tokio::test]
async fn transport_streams_a_length_bounded_request_body_without_coalescing_it() {
    async fn echo(body: Bytes) -> Bytes {
        body
    }

    let server = LoopbackServer::start(Router::new().route("/stream-upload", post(echo))).await;
    let transport = transport(
        TransportTimeouts::default(),
        TransportLimits {
            max_request_body_bytes: 64,
            max_response_body_bytes: 64,
            max_response_headers: 32,
            max_response_header_bytes: 4 * 1024,
        },
    );
    let chunks = stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(b"bounded-")),
        Ok(Bytes::from_static(b"stream")),
    ]);
    let request = TransportRequest::post_stream(
        server.url("/stream-upload"),
        chunks,
        14,
        false,
        NetworkPolicy::LoopbackOnly,
    )
    .unwrap()
    .with_header("content-type", "application/octet-stream")
    .unwrap();

    assert_eq!(complete_body(&transport, request).await, b"bounded-stream");

    let too_large = TransportRequest::post_stream(
        server.url("/stream-upload"),
        stream::empty::<Result<Bytes, std::io::Error>>(),
        65,
        false,
        NetworkPolicy::LoopbackOnly,
    )
    .unwrap();
    let error = transport
        .execute(too_large, &ExecutionCancellation::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::InvalidRequest);
}

#[tokio::test]
async fn transport_disables_redirects_and_never_reaches_the_location() {
    #[derive(Clone, Default)]
    struct RedirectState {
        followed: Arc<AtomicUsize>,
    }

    async fn redirect() -> impl IntoResponse {
        (StatusCode::FOUND, [(header::LOCATION, "/forbidden")])
    }

    async fn forbidden(State(state): State<RedirectState>) -> &'static str {
        state.followed.fetch_add(1, Ordering::SeqCst);
        "must not be reached"
    }

    let state = RedirectState::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/redirect", post(redirect))
            .route("/forbidden", get(forbidden).post(forbidden))
            .with_state(state.clone()),
    )
    .await;
    let transport = transport(TransportTimeouts::default(), TransportLimits::default());

    let response = transport
        .execute(
            loopback_request(server.url("/redirect"), false),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap();
    let status = match response {
        TransportResponse::Complete(response) => response.head().status(),
        TransportResponse::Streaming(_) => panic!("expected a complete response"),
    };

    assert_eq!(status, 302);
    assert_eq!(state.followed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transport_allows_only_safe_headers_and_one_redacted_auth_header() {
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Option<HeaderMap>>>);

    async fn capture(State(captured): State<Captured>, headers: HeaderMap) -> &'static str {
        *captured
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(headers);
        "ok"
    }

    let captured = Captured::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/headers", post(capture))
            .with_state(captured.clone()),
    )
    .await;
    let transport = transport(TransportTimeouts::default(), TransportLimits::default());
    let request = loopback_request(server.url("/headers"), false)
        .with_header("accept", "application/json")
        .unwrap()
        .with_sensitive_header(
            "authorization",
            SecretString::from(format!("Bearer {SECRET_CANARY}")),
        )
        .unwrap();

    let rendered = format!("{request:?}");
    assert!(!rendered.contains(SECRET_CANARY));
    assert!(rendered.contains("[redacted]"));
    assert_eq!(complete_body(&transport, request).await, b"ok");

    let headers = captured
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap();
    assert_eq!(
        headers.get(header::AUTHORIZATION).unwrap(),
        &format!("Bearer {SECRET_CANARY}")
    );
    assert_eq!(headers.get(header::ACCEPT_ENCODING).unwrap(), "identity");

    let invalid = loopback_request(server.url("/headers"), false)
        .with_header("cookie", "not-allowed")
        .unwrap_err();
    assert_eq!(invalid.kind(), TransportErrorKind::InvalidRequest);

    let duplicate = loopback_request(server.url("/headers"), false)
        .with_sensitive_header("authorization", SecretString::from("Bearer first"))
        .unwrap()
        .with_sensitive_header("x-api-key", SecretString::from("second"))
        .unwrap_err();
    assert_eq!(duplicate.kind(), TransportErrorKind::InvalidRequest);

    let rendered = format!("{duplicate:?}");
    assert!(!rendered.contains("first"));
    assert!(!rendered.contains("second"));
}

#[tokio::test]
async fn loopback_policy_rejects_non_loopback_before_any_connection() {
    let transport = transport(TransportTimeouts::default(), TransportLimits::default());
    let request = loopback_request(
        Url::parse("http://192.0.2.10/provider-must-not-be-reached").unwrap(),
        false,
    );

    let error = transport
        .execute(request, &ExecutionCancellation::new())
        .await
        .unwrap_err();

    assert_eq!(error.kind(), TransportErrorKind::Policy);
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("192.0.2.10"));
}

#[tokio::test]
async fn transport_enforces_response_and_header_bounds_without_decompression() {
    async fn oversized() -> Vec<u8> {
        vec![b'x'; 65]
    }

    async fn compressed() -> Response {
        (
            StatusCode::OK,
            [(header::CONTENT_ENCODING, "gzip")],
            "not-really-compressed",
        )
            .into_response()
    }

    let server = LoopbackServer::start(
        Router::new()
            .route("/oversized", post(oversized))
            .route("/compressed", post(compressed)),
    )
    .await;
    let limits = TransportLimits {
        max_request_body_bytes: 64,
        max_response_body_bytes: 64,
        max_response_headers: 32,
        max_response_header_bytes: 4 * 1024,
    };
    let transport = transport(TransportTimeouts::default(), limits);

    let oversized = transport
        .execute(
            loopback_request(server.url("/oversized"), false),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(oversized.kind(), TransportErrorKind::ResponseTooLarge);

    let compressed = transport
        .execute(
            loopback_request(server.url("/compressed"), false),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(compressed.kind(), TransportErrorKind::InvalidResponse);
}

#[tokio::test]
async fn transport_distinguishes_header_total_and_stream_idle_timeouts() {
    async fn slow_headers() -> &'static str {
        tokio::time::sleep(Duration::from_millis(150)).await;
        "late"
    }

    async fn slow_complete() -> Body {
        Body::from_stream(
            stream::iter([
                Ok::<_, Infallible>(Bytes::from_static(b"first")),
                Ok(Bytes::from_static(b"second")),
            ])
            .then_with_delay(Duration::from_millis(80)),
        )
    }

    async fn idle_stream() -> Body {
        Body::from_stream(stream::once(async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok::<_, Infallible>(Bytes::from_static(b"late"))
        }))
    }

    let server = LoopbackServer::start(
        Router::new()
            .route("/slow-headers", post(slow_headers))
            .route("/slow-complete", post(slow_complete))
            .route("/idle-stream", post(idle_stream)),
    )
    .await;
    let timeouts = TransportTimeouts {
        connect: Duration::from_secs(1),
        response_headers: Duration::from_millis(50),
        idle_stream: Duration::from_millis(50),
        non_stream_total: Duration::from_millis(100),
    };
    let transport = transport(timeouts, TransportLimits::default());

    let headers = transport
        .execute(
            loopback_request(server.url("/slow-headers"), false),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(headers.kind(), TransportErrorKind::HeaderTimeout);

    let total = transport
        .execute(
            loopback_request(server.url("/slow-complete"), false),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(total.kind(), TransportErrorKind::TotalTimeout);

    let mut stream = match transport
        .execute(
            loopback_request(server.url("/idle-stream"), true),
            &ExecutionCancellation::new(),
        )
        .await
        .unwrap()
    {
        TransportResponse::Streaming(stream) => stream,
        TransportResponse::Complete(_) => panic!("expected a streaming response"),
    };
    let idle = stream
        .next_chunk(&ExecutionCancellation::new())
        .await
        .unwrap_err();
    assert_eq!(idle.kind(), TransportErrorKind::IdleTimeout);
}

#[tokio::test]
async fn cancellation_aborts_header_waits_and_open_response_bodies() {
    async fn slow_headers() -> &'static str {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "late"
    }

    async fn never_streams() -> Body {
        Body::from_stream(stream::pending::<Result<Bytes, Infallible>>())
    }

    let server = LoopbackServer::start(
        Router::new()
            .route("/slow-headers", post(slow_headers))
            .route("/never-streams", post(never_streams)),
    )
    .await;
    let transport = transport(TransportTimeouts::default(), TransportLimits::default());

    let cancellation = ExecutionCancellation::new();
    cancellation.cancel();
    let cancelled = transport
        .execute(
            loopback_request(server.url("/slow-headers"), false),
            &cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(cancelled.kind(), TransportErrorKind::Cancelled);

    let cancellation = ExecutionCancellation::new();
    let mut stream = match transport
        .execute(
            loopback_request(server.url("/never-streams"), true),
            &cancellation,
        )
        .await
        .unwrap()
    {
        TransportResponse::Streaming(stream) => stream,
        TransportResponse::Complete(_) => panic!("expected a streaming response"),
    };
    cancellation.cancel();
    let cancelled = stream.next_chunk(&cancellation).await.unwrap_err();
    assert_eq!(cancelled.kind(), TransportErrorKind::Cancelled);
    assert!(stream.is_closed());
}

trait DelayedStreamExt: futures_util::Stream + Sized {
    fn then_with_delay(self, delay: Duration) -> impl futures_util::Stream<Item = Self::Item> {
        self.then(move |item| async move {
            tokio::time::sleep(delay).await;
            item
        })
    }
}

impl<T> DelayedStreamExt for T where T: futures_util::Stream + Sized {}
