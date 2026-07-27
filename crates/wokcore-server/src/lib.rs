//! Local runtime authentication and server primitives for WokCore.

pub mod api;
pub mod auth;
pub mod lifecycle;
pub mod observability;
pub mod runtime;
mod server;

pub use server::{RunningServer, ServerError, ServerShutdown, ServerState};
