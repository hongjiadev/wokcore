use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{any, get},
};
use futures_util::stream;
use serde::Serialize;
use thiserror::Error;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use url::Url;

use crate::{Scenario, ScenarioError};

#[derive(Clone)]
struct ServerState {
    scenario: Arc<Scenario>,
    counters: Arc<Counters>,
}

#[derive(Default)]
struct Counters {
    started: AtomicU64,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    completed: AtomicU64,
}

pub struct Simulator {
    address: SocketAddr,
    counters: Arc<Counters>,
    shutdown: Option<oneshot::Sender<()>>,
    owner: Option<JoinHandle<Result<(), std::io::Error>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SimulatorSummary {
    started: u64,
    active: usize,
    peak_active: usize,
    completed: u64,
}

impl SimulatorSummary {
    #[must_use]
    pub fn started(self) -> u64 {
        self.started
    }

    #[must_use]
    pub fn active(self) -> usize {
        self.active
    }

    #[must_use]
    pub fn peak_active(self) -> usize {
        self.peak_active
    }

    #[must_use]
    pub fn completed(self) -> u64 {
        self.completed
    }
}

#[derive(Debug, Error)]
pub enum SimulatorError {
    #[error("simulator endpoint policy rejected the address")]
    Endpoint(#[from] ScenarioError),
    #[error("simulator listener failed")]
    Io(#[from] std::io::Error),
    #[error("simulator owner task failed")]
    Join,
}

impl Simulator {
    pub async fn start(address: SocketAddr, scenario: Scenario) -> Result<Self, SimulatorError> {
        if !address.ip().is_loopback() {
            return Err(ScenarioError::NonLoopback.into());
        }
        scenario.schedule()?;
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let counters = Arc::new(Counters::default());
        let state = ServerState {
            scenario: Arc::new(scenario),
            counters: Arc::clone(&counters),
        };
        let router = Router::new()
            .route("/__wokcore_sim/summary", get(summary))
            .fallback(any(serve_scenario))
            .with_state(state);
        let (shutdown, receiver) = oneshot::channel();
        let owner = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await
        });
        Ok(Self {
            address,
            counters,
            shutdown: Some(shutdown),
            owner: Some(owner),
        })
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, normalized_path(path)))
            .expect("a loopback socket address and normalized path always form a URL")
    }

    #[must_use]
    pub fn summary(&self) -> SimulatorSummary {
        snapshot(&self.counters)
    }

    pub async fn shutdown(mut self) -> Result<(), SimulatorError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        owner.await.map_err(|_| SimulatorError::Join)??;
        Ok(())
    }
}

impl Drop for Simulator {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(owner) = self.owner.take() {
            owner.abort();
        }
    }
}

async fn summary(State(state): State<ServerState>) -> axum::Json<SimulatorSummary> {
    axum::Json(snapshot(&state.counters))
}

async fn serve_scenario(State(state): State<ServerState>) -> Response {
    let ordinal = state.counters.started.fetch_add(1, Ordering::Relaxed) as usize;
    let active = state.counters.active.fetch_add(1, Ordering::Relaxed) + 1;
    state
        .counters
        .peak_active
        .fetch_max(active, Ordering::Relaxed);
    let lifecycle = ActiveRequest::new(Arc::clone(&state.counters));
    let scenario = state.scenario.for_attempt(ordinal);
    let schedule = match scenario.schedule() {
        Ok(schedule) => schedule,
        Err(_) => return internal_response(lifecycle),
    };

    let mut response = if scenario.stream() {
        stream_response(schedule.into_chunks(), lifecycle)
    } else {
        let bytes = schedule
            .into_chunks()
            .into_iter()
            .flat_map(|chunk| chunk.into_parts().1)
            .collect::<Vec<_>>();
        lifecycle.complete();
        Response::new(Body::from(bytes))
    };
    *response.status_mut() =
        StatusCode::from_u16(scenario.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if scenario.stream() && scenario.status() < 400 {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    for (name, value) in scenario.headers() {
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), value.parse::<HeaderValue>()) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn stream_response(chunks: Vec<crate::ScheduledChunk>, lifecycle: ActiveRequest) -> Response {
    struct StreamState {
        chunks: std::vec::IntoIter<crate::ScheduledChunk>,
        lifecycle: Option<ActiveRequest>,
    }

    let state = StreamState {
        chunks: chunks.into_iter(),
        lifecycle: Some(lifecycle),
    };
    let body_stream = stream::unfold(state, |mut state| async move {
        let Some(chunk) = state.chunks.next() else {
            if let Some(lifecycle) = state.lifecycle.take() {
                lifecycle.complete();
            }
            return None;
        };
        let (delay, bytes) = chunk.into_parts();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Some((Ok::<Bytes, Infallible>(Bytes::from(bytes)), state))
    });
    Response::new(Body::from_stream(body_stream))
}

fn internal_response(lifecycle: ActiveRequest) -> Response {
    lifecycle.complete();
    let mut response = Response::new(Body::from(
        r#"{"error":{"type":"synthetic_internal_error"}}"#,
    ));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

fn snapshot(counters: &Counters) -> SimulatorSummary {
    SimulatorSummary {
        started: counters.started.load(Ordering::Relaxed),
        active: counters.active.load(Ordering::Relaxed),
        peak_active: counters.peak_active.load(Ordering::Relaxed),
        completed: counters.completed.load(Ordering::Relaxed),
    }
}

struct ActiveRequest {
    counters: Arc<Counters>,
}

impl ActiveRequest {
    fn new(counters: Arc<Counters>) -> Self {
        Self { counters }
    }

    fn complete(self) {
        self.counters.completed.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.counters.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn normalized_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}
