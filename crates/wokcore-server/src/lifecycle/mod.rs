mod admission;
mod service;

pub use admission::{ActiveRequestGuard, AdmissionController, MaintenanceAdmission};
pub use service::{
    DrainOutcome, LifecycleError, LifecyclePhase, LifecycleSnapshot, ServiceLifecycle,
};
