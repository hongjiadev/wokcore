//! Loopback-only synthetic Provider behavior for offline verification.

mod protocols;
mod scenario;
mod server;

pub use scenario::{
    FrameMode, MAX_CHUNK_BYTES, MAX_EVENTS, MAX_SCENARIO_BYTES, PayloadProfile, Protocol, Scenario,
    ScenarioError, Schedule, ScheduledChunk, validate_loopback_socket, validate_loopback_url,
};
pub use server::{Simulator, SimulatorError, SimulatorSummary};
