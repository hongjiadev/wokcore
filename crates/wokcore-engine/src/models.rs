use serde::Serialize;
use wokcore_core::id::ProviderId;

use crate::catalog::ProviderCapabilities;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicModelMetadata {
    pub id: String,
    pub owned_by: ProviderId,
    pub capabilities: ProviderCapabilities,
}
